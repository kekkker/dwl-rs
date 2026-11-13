// Compositor state and Wayland protocol handlers.
use std::{ffi::OsString, io::Write, sync::Arc, time::Instant};

use smithay::{
    backend::{allocator::dmabuf::Dmabuf, renderer::{ImportDma, utils::on_commit_buffer_handler}},
    delegate_dispatch2,
    desktop::{
        LayerSurface, PopupKind, PopupManager, Space, Window, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, layer_map_for_output,
    },
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::XkbConfig,
        pointer::{CursorImageStatus, CursorImageSurfaceData, Focus},
    },
    output::{Output, WeakOutput},
    reexports::{
        calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction, generic::Generic},
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecoMode,
        wayland_protocols::xdg::shell::server::xdg_toplevel::{State as XdgState, WmCapabilities},
        wayland_server::{
            Client as WlClient, Display, DisplayHandle, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{IsAlive, Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes, get_parent,
            is_sync_subsurface, with_states,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState},
        idle_notify::{IdleNotifierHandler, IdleNotifierState},
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler, OutputCaptureSourceState,
        },
        image_copy_capture::{BufferConstraints, Frame, ImageCopyCaptureHandler, ImageCopyCaptureState, Session, SessionRef},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus},
            ext_data_control::{DataControlHandler as ExtDataControlHandler, DataControlState as ExtDataControlState},
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState, set_primary_focus},
            wlr_data_control::{DataControlHandler, DataControlState},
        },
        session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker},
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState},
            xdg::{
                PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
                decoration::{XdgDecorationHandler, XdgDecorationState},
            },
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState, with_pointer_constraint},
        relative_pointer::RelativePointerManagerState,
        virtual_keyboard::{VirtualKeyboardBackend, VirtualKeyboardHandler, VirtualKeyboardManagerState},
        virtual_pointer::{VirtualPointerBackend, VirtualPointerHandler, VirtualPointerManagerState},
        xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData},
    },
};

use crate::{config::*, render::CursorCache, wm::{Client, Key, Monitor}};

pub enum Backend {
    None,
    Winit(Box<crate::winit::Winit>),
    Udev(Box<crate::udev::Udev>),
}

impl Backend {
    pub fn renderer(&mut self) -> Option<&mut smithay::backend::renderer::gles::GlesRenderer> {
        match self {
            Backend::Winit(w) => Some(w.backend.renderer()),
            Backend::Udev(u) => u.renderer(),
            Backend::None => None,
        }
    }
    pub fn gamma_size(&self, o: &Output) -> Option<u32> {
        if let Backend::Udev(u) = self { u.gamma_size(o) } else { None }
    }
    pub fn set_gamma(&mut self, o: &Output, ramp: Option<Vec<u16>>) -> std::io::Result<()> {
        if let Backend::Udev(u) = self { u.set_gamma(o, ramp) } else { Ok(()) }
    }
    pub fn change_vt(&mut self, vt: i32) {
        if let Backend::Udev(u) = self {
            u.change_vt(vt);
        }
    }
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}

pub struct State {
    pub dh: DisplayHandle,
    pub lh: LoopHandle<'static, State>,
    pub socket: OsString,
    pub running: bool,
    pub start: Instant,
    pub backend: Backend,
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub seat: Seat<State>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub dmabuf_state: Option<(DmabufState, DmabufGlobal)>,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    pub ext_data_control_state: ExtDataControlState,
    pub lock_state: SessionLockManagerState,
    pub idle_notifier_state: IdleNotifierState<State>,
    pub activation_state: XdgActivationState,
    pub output_capture_source_state: OutputCaptureSourceState,
    pub image_copy_capture_state: ImageCopyCaptureState,

    // dwl state
    pub keys: Vec<Key>,
    pub clients: Vec<Client>,
    pub fstack: Vec<Window>,
    pub mons: Vec<Monitor>,
    pub selmon: Option<Output>,
    pub unmapped: Vec<Window>,
    pub grab: Option<(Window, smithay::utils::Point<f64, smithay::utils::Logical>)>,
    pub grab_resize: bool,
    pub cursor_status: CursorImageStatus,
    pub cursors: CursorCache,
    pub locked: bool,
    pub lock_surfaces: Vec<(Output, LockSurface)>,
    pub inhibitors: Vec<WlSurface>,
    pub gamma: Vec<(Output, smithay::reexports::wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_v1::ZwlrGammaControlV1)>,
    pub capture_sessions: Vec<Session>,
    pub captures: Vec<(Output, Frame, bool)>,
    pub status: Box<dyn Write>,
    pub last_status: String,
}

impl State {
    pub fn new(event_loop: &mut EventLoop<'static, State>, display: Display<State>, status: Box<dyn Write>) -> Self {
        let dh = display.handle();
        let lh = event_loop.handle();

        let source = ListeningSocketSource::new_auto().expect("failed to create wayland socket");
        let socket = source.socket_name().to_os_string();
        lh.insert_source(source, |stream, _, s| {
            if let Err(e) = s.dh.insert_client(stream, Arc::new(ClientState::default())) {
                tracing::warn!("failed to add client: {e}");
            }
        })
        .unwrap();
        lh.insert_source(Generic::new(display, Interest::READ, Mode::Level), |_, d, s| {
            unsafe { d.get_mut().dispatch_clients(s).unwrap() };
            Ok(PostAction::Continue)
        })
        .unwrap();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "seat0");
        let xkb = XkbConfig { layout: XKB_LAYOUT, options: XKB_OPTIONS.map(String::from), ..Default::default() };
        seat.add_keyboard(xkb, REPEAT_DELAY, REPEAT_RATE).expect("bad xkb config");
        seat.add_pointer();

        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        ViewporterState::new::<Self>(&dh);
        FractionalScaleManagerState::new::<Self>(&dh);
        SinglePixelBufferState::new::<Self>(&dh);
        CursorShapeManagerState::new::<Self>(&dh);
        IdleInhibitManagerState::new::<Self>(&dh);
        OutputManagerState::new_with_xdg_output::<Self>(&dh);
        XdgDecorationState::new::<Self>(&dh);
        VirtualKeyboardManagerState::new::<Self, _>(&dh, |_| true);
        RelativePointerManagerState::new::<Self>(&dh);
        PointerConstraintsState::new::<Self>(&dh);
        VirtualPointerManagerState::new::<Self, _>(&dh, |_| true);

        State {
            socket,
            running: true,
            start: Instant::now(),
            backend: Backend::None,
            space: Space::default(),
            popups: PopupManager::default(),
            seat,
            compositor_state: CompositorState::new::<Self>(&dh),
            // like dwl: only fullscreen, so clients hide maximize/minimize buttons
            xdg_shell_state: XdgShellState::new_with_capabilities::<Self>(&dh, [WmCapabilities::Fullscreen]),
            layer_shell_state: WlrLayerShellState::new::<Self>(&dh),
            shm_state: ShmState::new::<Self>(&dh, vec![]),
            dmabuf_state: None,
            seat_state,
            data_device_state: DataDeviceState::new::<Self>(&dh),
            data_control_state: DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true),
            ext_data_control_state: ExtDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true),
            primary_selection_state,
            lock_state: SessionLockManagerState::new::<Self, _>(&dh, |_| true),
            idle_notifier_state: IdleNotifierState::new(&dh, lh.clone()),
            activation_state: XdgActivationState::new::<Self>(&dh),
            output_capture_source_state: OutputCaptureSourceState::new::<Self>(&dh),
            image_copy_capture_state: ImageCopyCaptureState::new::<Self>(&dh),
            keys: keys(),
            clients: vec![],
            fstack: vec![],
            mons: vec![],
            selmon: None,
            unmapped: vec![],
            grab: None,
            grab_resize: false,
            cursor_status: CursorImageStatus::default_named(),
            cursors: CursorCache::default(),
            locked: false,
            lock_surfaces: vec![],
            inhibitors: vec![],
            gamma: vec![],
            capture_sessions: vec![],
            captures: vec![],
            status,
            last_status: String::new(),
            dh,
            lh,
        }
    }

    pub fn redraw(&mut self) {
        if let Backend::Udev(u) = &mut self.backend {
            u.queue_redraw();
        }
    }

    pub fn layer_for(&self, s: &WlSurface) -> Option<LayerSurface> {
        self.mons.iter().find_map(|m| {
            layer_map_for_output(&m.output).layer_for_surface(s, WindowSurfaceType::TOPLEVEL).cloned()
        })
    }

    pub fn window_for(&self, s: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .chain(self.clients.iter().map(|c| &c.window))
            .find(|w| w.toplevel().unwrap().wl_surface() == s)
            .cloned()
    }

    pub fn update_idle_inhibit(&mut self) {
        self.inhibitors.retain(|s| s.alive());
        let visible = self.inhibitors.iter().any(|s| {
            let mut root = s.clone();
            while let Some(p) = get_parent(&root) {
                root = p;
            }
            // a client's inhibitor counts only while the client is shown; layer surfaces always count
            match self.clients.iter().find(|c| c.window.toplevel().unwrap().wl_surface() == &root) {
                Some(c) => self.space.elements().any(|e| e == &c.window),
                None => true,
            }
        });
        self.idle_notifier_state.set_is_inhibited(visible);
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else { return };
        let (origin, out) = if let Some(w) = self.window_for(&root) {
            let Some(loc) = self.space.element_location(&w) else { return };
            let out = self.client_idx(&w).map(|ci| self.clients[ci].mon.clone());
            (loc - w.geometry().loc, out)
        } else if let Some(l) = self.layer_for(&root) {
            let Some(o) = self.mons.iter().map(|m| &m.output).find(|o| layer_map_for_output(o).layers().any(|x| x == &l))
            else {
                return;
            };
            let geo = layer_map_for_output(o).layer_geometry(&l).unwrap_or_default();
            (self.space.output_geometry(o).unwrap_or_default().loc + geo.loc, Some(o.clone()))
        } else {
            return;
        };
        let Some(area) = out.and_then(|o| self.space.output_geometry(&o)) else { return };
        let mut target = area;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone())) + origin;
        popup.with_pending_state(|s| s.geometry = s.positioner.get_unconstrained_geometry(target));
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a WlClient) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }
    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(p)) = self.popups.find_popup(surface)
            && !p.is_initial_configure_sent()
        {
            let _ = p.send_configure();
        }
        self.redraw();
        if is_sync_subsurface(surface) {
            return;
        }
        let mut root = surface.clone();
        while let Some(p) = get_parent(&root) {
            root = p;
        }

        // cursor surfaces move their hotspot with the buffer offset
        if matches!(&self.cursor_status, CursorImageStatus::Surface(s) if s == surface) {
            with_states(surface, |s| {
                let delta = s.cached_state.get::<SurfaceAttributes>().current().buffer_delta.take();
                if let (Some(d), Some(a)) = (delta, s.data_map.get::<CursorImageSurfaceData>()) {
                    a.lock().unwrap().hotspot -= d;
                }
            });
        }

        // toplevels: initial configure, then map on first buffer
        if let Some(i) = self.unmapped.iter().position(|w| w.toplevel().unwrap().wl_surface() == &root) {
            let w = self.unmapped[i].clone();
            let t = w.toplevel().unwrap();
            if !t.is_initial_configure_sent() {
                t.with_pending_state(|s| {
                    for st in [XdgState::TiledLeft, XdgState::TiledRight, XdgState::TiledTop, XdgState::TiledBottom] {
                        s.states.set(st);
                    }
                    s.decoration_mode = Some(DecoMode::ServerSide);
                });
                t.send_configure();
            } else if has_buffer(surface) {
                w.on_commit();
                self.unmapped.remove(i);
                self.map_client(w);
            }
            return;
        }
        if let Some(w) = self.clients.iter().find(|c| c.window.toplevel().unwrap().wl_surface() == &root) {
            let w = w.window.clone();
            w.on_commit();
            if &root == surface && !has_buffer(surface) {
                // client unmapped itself; treat it as new again
                self.unmap_client(&w);
                self.unmapped.push(w);
            }
            return;
        }

        // layer surfaces
        if let Some(o) = self
            .mons
            .iter()
            .map(|m| m.output.clone())
            .find(|o| layer_map_for_output(o).layer_for_surface(&root, WindowSurfaceType::TOPLEVEL).is_some())
        {
            let mut map = layer_map_for_output(&o);
            let zone = map.non_exclusive_zone();
            map.arrange();
            let changed = map.non_exclusive_zone() != zone;
            let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL).unwrap().clone();
            drop(map);
            let configured = with_states(&root, |s| {
                s.data_map.get::<smithay::wayland::shell::wlr_layer::LayerSurfaceData>().unwrap().lock().unwrap().initial_configure_sent
            });
            if !configured {
                layer.layer_surface().send_configure();
                return;
            }
            if changed && let Some(mi) = self.mon_idx(&o) {
                self.arrange(mi);
            }
            let st = layer.cached_state();
            let kb = self.seat.get_keyboard().unwrap();
            if !self.locked
                && has_buffer(&root)
                && st.keyboard_interactivity == KeyboardInteractivity::Exclusive
                && matches!(st.layer, Layer::Top | Layer::Overlay)
                && kb.current_focus().as_ref() != Some(&root)
            {
                kb.set_focus(self, Some(root), Serial::from(0));
            }
        }
    }
}

fn has_buffer(s: &WlSurface) -> bool {
    smithay::backend::renderer::utils::with_renderer_surface_state(s, |st| st.buffer().is_some()).unwrap_or(false)
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state.as_mut().unwrap().0
    }
    fn dmabuf_imported(&mut self, _: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        if self.backend.renderer().is_some_and(|r| r.import_dmabuf(&dmabuf, None).is_ok()) {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.unmapped.push(Window::new_wayland_window(surface));
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unmapped.retain(|w| w.toplevel().unwrap() != &surface);
        if let Some(w) = self.window_for(surface.wl_surface()) {
            self.unmap_client(&w);
        }
    }
    fn new_popup(&mut self, surface: PopupSurface, _: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }
    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
    fn grab(&mut self, surface: PopupSurface, _: WlSeat, serial: Serial) {
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        if let Ok(mut grab) = self.popups.grab_popup(root, kind, &self.seat.clone(), serial) {
            let kb = self.seat.get_keyboard().unwrap();
            let ptr = self.seat.get_pointer().unwrap();
            if kb.is_grabbed() && !(kb.has_grab(serial) || grab.previous_serial().is_some_and(|s| kb.has_grab(s))) {
                grab.ungrab(smithay::desktop::PopupUngrabStrategy::All);
                return;
            }
            kb.set_focus(self, grab.current_grab(), serial);
            kb.set_grab(self, smithay::desktop::PopupKeyboardGrab::new(&grab), serial);
            ptr.set_grab(self, smithay::desktop::PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _: Option<WlOutput>) {
        match self.window_for(surface.wl_surface()).and_then(|w| self.client_idx(&w)) {
            Some(ci) => self.setfullscreen(ci, true),
            None => {
                surface.with_pending_state(|s| s.states.set(XdgState::Fullscreen));
            }
        }
    }
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        match self.window_for(surface.wl_surface()).and_then(|w| self.client_idx(&w)) {
            Some(ci) => self.setfullscreen(ci, false),
            None => {
                surface.with_pending_state(|s| s.states.unset(XdgState::Fullscreen));
            }
        }
    }
    fn title_changed(&mut self, _: ToplevelSurface) {
        self.printstatus();
    }
    fn app_id_changed(&mut self, _: ToplevelSurface) {
        self.printstatus();
    }
}

impl XdgDecorationHandler for State {
    // dwl draws borders; ask every client to drop its decorations
    fn new_decoration(&mut self, t: ToplevelSurface) {
        t.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
    }
    fn request_mode(&mut self, t: ToplevelSurface, _: DecoMode) {
        self.new_decoration(t.clone());
        if t.is_initial_configure_sent() {
            t.send_pending_configure();
        }
    }
    fn unset_mode(&mut self, t: ToplevelSurface) {
        self.request_mode(t, DecoMode::ServerSide);
    }
    fn decoration_destroyed(&mut self, _: ToplevelSurface) {}
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }
    fn new_layer_surface(&mut self, surface: WlrLayerSurface, output: Option<WlOutput>, _: Layer, namespace: String) {
        let Some(o) = output.as_ref().and_then(Output::from_resource).or_else(|| self.selmon.clone()) else {
            surface.send_close();
            return;
        };
        let _ = layer_map_for_output(&o).map_layer(&LayerSurface::new(surface, namespace));
    }
    fn new_popup(&mut self, _: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }
    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let mut arrange = None;
        for (mi, m) in self.mons.iter().enumerate() {
            let mut map = layer_map_for_output(&m.output);
            let found = map.layers().find(|l| l.layer_surface() == &surface).cloned();
            if let Some(l) = found {
                map.unmap_layer(&l);
                arrange = Some(mi);
            }
        }
        if let Some(mi) = arrange {
            self.arrange(mi);
        }
        let kb = self.seat.get_keyboard().unwrap();
        if kb.current_focus().is_none_or(|f| &f == surface.wl_surface()) {
            kb.set_focus(self, None, Serial::from(0));
            self.focus_top();
        }
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, seat, client.clone());
        set_primary_focus(&self.dh, seat, client);
    }
    fn cursor_image(&mut self, _: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
        self.redraw();
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}
impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}
impl smithay::input::dnd::DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {}
impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}
impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}
impl ExtDataControlHandler for State {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

// wtype, wvkbd, ydotool-style tools: handled like real devices
impl VirtualKeyboardHandler for State {
    fn process_virtual_keyboard_event(&mut self, e: smithay::backend::input::InputEvent<VirtualKeyboardBackend>) {
        self.process_input_event(e);
    }
}
impl VirtualPointerHandler for State {
    fn process_virtual_pointer_event(&mut self, e: smithay::backend::input::InputEvent<VirtualPointerBackend>) {
        self.process_input_event(e);
    }
}

impl OutputHandler for State {}
impl smithay::input::tablet::TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}
// games: lock/confine the pointer once it is over the requesting surface
impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &smithay::input::pointer::PointerHandle<Self>) {
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |c| c.map(|c| c.activate()));
        }
    }
}
impl FractionalScaleHandler for State {}

impl SessionLockHandler for State {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.lock_state
    }
    fn lock(&mut self, confirmation: SessionLocker) {
        self.locked = true;
        let kb = self.seat.get_keyboard().unwrap();
        kb.set_focus(self, None, Serial::from(0));
        confirmation.lock();
        self.redraw();
    }
    fn unlock(&mut self) {
        self.locked = false;
        self.lock_surfaces.clear();
        self.focus_top();
        self.redraw();
    }
    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(o) = Output::from_resource(&output) else { return };
        let Some(geo) = self.space.output_geometry(&o) else { return };
        surface.with_pending_state(|s| s.size = Some((geo.size.w as u32, geo.size.h as u32).into()));
        surface.send_configure();
        if self.selmon.as_ref() == Some(&o) || self.lock_surfaces.is_empty() {
            let kb = self.seat.get_keyboard().unwrap();
            kb.set_focus(self, Some(surface.wl_surface().clone()), Serial::from(0));
        }
        self.lock_surfaces.push((o, surface));
    }
}

impl IdleNotifierHandler for State {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.idle_notifier_state
    }
}
impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        self.inhibitors.push(surface);
        self.update_idle_inhibit();
    }
    fn uninhibit(&mut self, surface: WlSurface) {
        self.inhibitors.retain(|s| s != &surface);
        self.update_idle_inhibit();
    }
}

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }
    // dwl marks the client urgent instead of stealing focus
    fn request_activation(&mut self, _: XdgActivationToken, _: XdgActivationTokenData, surface: WlSurface) {
        let Some(ci) = self.window_for(&surface).and_then(|w| self.client_idx(&w)) else { return };
        if Some(ci) != self.sel() {
            self.clients[ci].urgent = true;
            self.printstatus();
            self.redraw();
        }
    }
}

impl ImageCaptureSourceHandler for State {
    fn source_destroyed(&mut self, _: ImageCaptureSource) {}
}
impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }
    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}
impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }
    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let output = source.user_data().get::<WeakOutput>()?.upgrade()?;
        let size = output.current_transform().transform_size(output.current_mode()?.size);
        Some(BufferConstraints {
            size: size.to_logical(1).to_buffer(1, Transform::Normal),
            shm: vec![wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888],
            dma: None,
        })
    }
    fn new_session(&mut self, session: Session) {
        self.capture_sessions.push(session); // dropping a session stops it
    }
    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture_sessions.retain(|s| s != &session);
    }
    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        let Some(o) = session.source().user_data().get::<WeakOutput>().and_then(|o| o.upgrade()) else {
            return frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Stopped);
        };
        self.captures.push((o, frame, session.draw_cursor()));
        self.redraw();
    }
}

delegate_dispatch2!(State);
