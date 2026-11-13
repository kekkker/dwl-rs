// TTY backend: libseat session, one GPU via DRM/KMS + GBM, libinput.
use std::{collections::HashMap, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode,
            compositor::FrameFlags,
            exporter::gbm::GbmFramebufferExporter,
            output::{DrmOutput, DrmOutputManager, DrmOutputRenderElements},
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{ImportDma, ImportMemWl, gles::GlesRenderer},
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, all_gpus, primary_gpu},
    },
    output::{Mode, Output, PhysicalProperties},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{Device as _, ModeTypeFlags, connector, crtc},
        input::{AccelProfile, ClickMethod, Device as InputDevice, DragLockState, Libinput, ScrollMethod},
        rustix::fs::OFlags,
        wayland_server::backend::GlobalId,
    },
    utils::DeviceFd,
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState},
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};

use crate::{
    config::*,
    render::{self, Elem},
    state::{Backend, State},
};

type Alloc = GbmAllocator<DrmDeviceFd>;
type Exporter = GbmFramebufferExporter<DrmDeviceFd>;

pub struct Udev {
    session: LibSeatSession,
    drm: DrmOutputManager<Alloc, Exporter, (), DrmDeviceFd>,
    scanner: DrmScanner,
    renderer: GlesRenderer,
    surfaces: HashMap<crtc::Handle, Surface>,
}

struct Surface {
    output: Output,
    drm: DrmOutput<Alloc, Exporter, (), DrmDeviceFd>,
    global: GlobalId,
    pending: bool, // needs a new frame
    gamma: Option<Vec<u16>>,
    busy: bool,    // waiting for vblank
}

impl Udev {
    pub fn renderer(&mut self) -> Option<&mut GlesRenderer> {
        Some(&mut self.renderer)
    }
    pub fn change_vt(&mut self, vt: i32) {
        if let Err(e) = self.session.change_vt(vt) {
            tracing::warn!("chvt {vt}: {e}");
        }
    }
    fn crtc_of(&self, o: &Output) -> Option<crtc::Handle> {
        self.surfaces.iter().find(|(_, x)| &x.output == o).map(|(c, _)| *c)
    }
    pub fn gamma_size(&self, o: &Output) -> Option<u32> {
        let info = self.drm.device().get_crtc(self.crtc_of(o)?).ok()?;
        Some(info.gamma_length()).filter(|&n| n > 1)
    }
    /// `None` restores a linear ramp.
    pub fn set_gamma(&mut self, o: &Output, ramp: Option<Vec<u16>>) -> std::io::Result<()> {
        let (Some(crtc), Some(n)) = (self.crtc_of(o), self.gamma_size(o)) else { return Ok(()) };
        let n = n as usize;
        let r = ramp.clone().unwrap_or_else(|| (0..3 * n).map(|i| ((i % n) * 0xffff / (n - 1)) as u16).collect());
        self.drm.device().set_gamma(crtc, &r[..n], &r[n..2 * n], &r[2 * n..])?;
        self.surfaces.get_mut(&crtc).unwrap().gamma = ramp;
        Ok(())
    }
    pub fn queue_redraw(&mut self) {
        for s in self.surfaces.values_mut() {
            s.pending = true;
        }
    }
}

pub fn init(event_loop: &mut EventLoop<'static, State>, s: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (mut session, notifier) = LibSeatSession::new()?;
    let seat = session.seat();
    let path = primary_gpu(&seat)?.or_else(|| all_gpus(&seat).ok()?.into_iter().next()).ok_or("no GPU found")?;
    let node = DrmNode::from_path(&path)?;
    let fd = session.open(&path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK)?;
    let fd = DrmDeviceFd::new(DeviceFd::from(fd));
    let (drm, drm_notifier) = DrmDevice::new(fd.clone(), true)?;
    let gbm = GbmDevice::new(fd)?;
    let display = unsafe { EGLDisplay::new(gbm.clone())? };
    let render_node = EGLDevice::device_for_display(&display)?.try_get_render_node()?.unwrap_or(node);
    let renderer = unsafe { GlesRenderer::new(EGLContext::new(&display)?)? };
    let drm = DrmOutputManager::new(
        drm,
        GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT),
        GbmFramebufferExporter::new(gbm.clone(), Some(render_node).into()),
        Some(gbm),
        [Fourcc::Argb8888, Fourcc::Abgr8888],
        renderer.egl_context().dmabuf_render_formats().clone(),
    );

    s.shm_state.update_formats(renderer.shm_formats());
    let feedback = DmabufFeedbackBuilder::new(render_node.dev_id(), renderer.dmabuf_formats()).build()?;
    let mut dmabuf = DmabufState::new();
    let global = dmabuf.create_global_with_default_feedback::<State>(&s.dh, &feedback);
    s.dmabuf_state = Some((dmabuf, global));

    let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput.udev_assign_seat(&seat).map_err(|_| "libinput: failed to assign seat")?;
    let lh = event_loop.handle();
    lh.insert_source(LibinputInputBackend::new(libinput.clone()), |mut event, _, s| {
        if let InputEvent::DeviceAdded { device } = &mut event {
            configure_input(device);
        }
        s.process_input_event(event);
    })?;
    lh.insert_source(notifier, move |event, _, s| {
        let Backend::Udev(u) = &mut s.backend else { return };
        match event {
            SessionEvent::PauseSession => {
                libinput.suspend();
                u.drm.pause();
            }
            SessionEvent::ActivateSession => {
                let _ = libinput.resume();
                if let Err(e) = u.drm.lock().activate(true) {
                    tracing::error!("failed to reactivate drm: {e}");
                }
                for x in u.surfaces.values_mut() {
                    x.busy = false;
                    x.pending = true;
                }
                // another DRM master may have changed the gamma while we were away
                let ramps: Vec<_> = u.surfaces.values().filter_map(|x| Some((x.output.clone(), x.gamma.clone()?))).collect();
                for (o, r) in ramps {
                    let _ = u.set_gamma(&o, Some(r));
                }
            }
        }
    })?;
    lh.insert_source(drm_notifier, |event, _, s| {
        if let DrmEvent::VBlank(crtc) = event
            && let Backend::Udev(u) = &mut s.backend
            && let Some(x) = u.surfaces.get_mut(&crtc)
        {
            if let Err(e) = x.drm.frame_submitted() {
                tracing::warn!("frame_submitted: {e}");
            }
            x.busy = false;
        }
    })?;
    lh.insert_source(UdevBackend::new(&seat)?, move |event, _, s| {
        if let UdevEvent::Changed { device_id } = event
            && device_id == node.dev_id()
        {
            scan(s);
        }
    })?;

    crate::gamma::init(&s.dh);
    s.backend = Backend::Udev(Box::new(Udev { session, drm, scanner: DrmScanner::new(), renderer, surfaces: HashMap::new() }));
    scan(s);
    Ok(())
}

fn configure_input(d: &mut InputDevice) {
    // errors just mean the device lacks that setting
    let _ = d.config_tap_set_enabled(TAP_TO_CLICK);
    let _ = d.config_tap_set_drag_enabled(TAP_AND_DRAG);
    let _ = d.config_tap_set_drag_lock_enabled(if DRAG_LOCK { DragLockState::EnabledTimeout } else { DragLockState::Disabled });
    let _ = d.config_scroll_set_natural_scroll_enabled(NATURAL_SCROLLING);
    let _ = d.config_dwt_set_enabled(DISABLE_WHILE_TYPING);
    let _ = d.config_left_handed_set(LEFT_HANDED);
    let _ = d.config_middle_emulation_set_enabled(MIDDLE_BUTTON_EMULATION);
    let _ = d.config_scroll_set_method(ScrollMethod::TwoFinger);
    let _ = d.config_click_set_method(ClickMethod::ButtonAreas);
    let _ = d.config_accel_set_profile(if ACCEL_FLAT { AccelProfile::Flat } else { AccelProfile::Adaptive });
    let _ = d.config_accel_set_speed(ACCEL_SPEED);
}

/// Sync outputs with the connectors that are plugged in.
fn scan(s: &mut State) {
    let Backend::Udev(u) = &mut s.backend else { return };
    let events = match u.scanner.scan_connectors(u.drm.device()) {
        Ok(e) => e,
        Err(e) => return tracing::warn!("scanning connectors: {e}"),
    };
    for event in events {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => connect(s, connector, crtc),
            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                let Backend::Udev(u) = &mut s.backend else { return };
                if let Some(x) = u.surfaces.remove(&crtc) {
                    s.remove_monitor(&x.output);
                    s.dh.remove_global::<State>(x.global);
                }
            }
            _ => {}
        }
    }
}

fn connect(s: &mut State, conn: connector::Info, crtc: crtc::Handle) {
    let Backend::Udev(u) = &mut s.backend else { return };
    let modes = conn.modes();
    let Some(&pref) = modes.iter().find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or(modes.first()) else {
        return;
    };
    // EDID "preferred" is often 60 Hz; take the fastest refresh at that resolution
    let mode = *modes.iter().filter(|m| m.size() == pref.size()).max_by_key(|m| Mode::from(**m).refresh).unwrap_or(&pref);
    let info = display_info::for_connector(u.drm.device(), conn.handle());
    let (w, h) = conn.size().unwrap_or((0, 0));
    let output = Output::new(
        format!("{}-{}", conn.interface().as_str(), conn.interface_id()),
        PhysicalProperties {
            size: (w as i32, h as i32).into(),
            subpixel: conn.subpixel().into(),
            make: info.as_ref().and_then(|i| i.make()).unwrap_or_else(|| "Unknown".into()),
            model: info.as_ref().and_then(|i| i.model()).unwrap_or_else(|| "Unknown".into()),
            serial_number: info.as_ref().and_then(|i| i.serial()).unwrap_or_else(|| "Unknown".into()),
        },
    );
    output.set_preferred(Mode::from(mode));
    output.change_current_state(Some(Mode::from(mode)), None, None, None);
    let global = output.create_global::<State>(&s.dh);
    let drm = match u.drm.lock().initialize_output::<_, Elem>(
        crtc,
        mode,
        &[conn.handle()],
        &output,
        None,
        &mut u.renderer,
        &DrmOutputRenderElements::default(),
    ) {
        Ok(d) => d,
        Err(e) => return tracing::warn!("failed to set up {}: {e}", output.name()),
    };
    let m = Mode::from(mode);
    tracing::info!("{}: {}x{}@{:.2}Hz", output.name(), m.size.w, m.size.h, m.refresh as f64 / 1000.0);
    u.surfaces.insert(crtc, Surface { output: output.clone(), drm, global, pending: true, busy: false, gamma: None });
    s.add_monitor(output);
}

/// Render every output that has new content and is not waiting for a vblank.
pub fn render_pending(s: &mut State) {
    let Backend::Udev(u) = &s.backend else { return };
    if !u.session.is_active() {
        return;
    }
    let crtcs: Vec<_> = u.surfaces.iter().filter(|(_, x)| x.pending && !x.busy).map(|(c, _)| *c).collect();
    for crtc in crtcs {
        render_surface(s, crtc);
    }
}

fn render_surface(s: &mut State, crtc: crtc::Handle) {
    let Backend::Udev(mut u) = std::mem::replace(&mut s.backend, Backend::None) else { return };
    if let Some(x) = u.surfaces.get_mut(&crtc) {
        let output = x.output.clone();
        let (elems, ncursor, clear) = render::output_elements(s, &mut u.renderer, &output);
        x.pending = false;
        match x.drm.render_frame(&mut u.renderer, &elems, clear, FrameFlags::DEFAULT) {
            Ok(r) if !r.is_empty => match x.drm.queue_frame(()) {
                Ok(()) => x.busy = true,
                Err(e) => tracing::warn!("queue_frame: {e}"),
            },
            Ok(_) => {
                // nothing changed; pace frame callbacks as if a vblank happened
                x.busy = true;
                let refresh = output.current_mode().map_or(60_000, |m| m.refresh).max(1) as u64;
                let timer = Timer::from_duration(Duration::from_micros(1_000_000_000 / refresh));
                let _ = s.lh.insert_source(timer, move |_, _, s| {
                    if let Backend::Udev(u) = &mut s.backend
                        && let Some(x) = u.surfaces.get_mut(&crtc)
                    {
                        x.busy = false;
                    }
                    TimeoutAction::Drop
                });
            }
            Err(e) => tracing::warn!("render_frame: {e}"),
        }
        render::post_render(s, &mut u.renderer, &output, &elems, ncursor, clear);
    }
    s.backend = Backend::Udev(u);
}
