// Nested backend: runs inside another Wayland/X11 session as a window.
use smithay::{
    backend::{
        egl::EGLDevice,
        renderer::{ImportDma, ImportMemWl, damage::OutputDamageTracker, gles::GlesRenderer},
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::calloop::EventLoop,
    utils::Transform,
    wayland::dmabuf::{DmabufFeedbackBuilder, DmabufState},
};

use crate::{
    render,
    state::{Backend, State},
};

pub struct Winit {
    pub backend: WinitGraphicsBackend<GlesRenderer>,
    damage: OutputDamageTracker,
    output: Output,
}

pub fn init(event_loop: &mut EventLoop<'static, State>, s: &mut State) -> Result<(), Box<dyn std::error::Error>> {
    let (mut backend, winit) = winit::init::<GlesRenderer>()?;
    let mode = Mode { size: backend.window_size(), refresh: 60_000 };
    let props = PhysicalProperties {
        size: (0, 0).into(),
        subpixel: Subpixel::Unknown,
        make: "dwl-rs".into(),
        model: "nested".into(),
        serial_number: "0".into(),
    };
    let output = Output::new("WL-1".into(), props);
    output.create_global::<State>(&s.dh);
    output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
    output.set_preferred(mode);

    let r = backend.renderer();
    s.shm_state.update_formats(r.shm_formats());
    if let Ok(Some(node)) = EGLDevice::device_for_display(r.egl_context().display()).and_then(|d| d.try_get_render_node()) {
        let feedback = DmabufFeedbackBuilder::new(node.dev_id(), r.dmabuf_formats()).build()?;
        let mut state = DmabufState::new();
        let global = state.create_global_with_default_feedback::<State>(&s.dh, &feedback);
        s.dmabuf_state = Some((state, global));
    }

    let damage = OutputDamageTracker::from_output(&output);
    s.backend = Backend::Winit(Box::new(Winit { backend, damage, output: output.clone() }));
    s.add_monitor(output);

    event_loop.handle().insert_source(winit, |event, _, s| match event {
        WinitEvent::Resized { size, .. } => {
            let Backend::Winit(w) = &s.backend else { return };
            w.output.change_current_state(Some(Mode { size, refresh: 60_000 }), None, None, None);
            s.arrange_all();
        }
        WinitEvent::Input(e) => s.process_input_event(e),
        WinitEvent::Redraw => redraw(s),
        WinitEvent::CloseRequested => s.running = false,
        _ => {}
    })?;
    Ok(())
}

fn redraw(s: &mut State) {
    let Backend::Winit(mut w) = std::mem::replace(&mut s.backend, Backend::None) else { return };
    let output = w.output.clone();
    let (elems, ncursor, clear) = render::output_elements(s, w.backend.renderer(), &output);
    let damage = match w.backend.bind() {
        Ok((r, mut fb)) => w.damage.render_output(r, &mut fb, 0, &elems, clear).ok().and_then(|r| r.damage.cloned()),
        Err(_) => None,
    };
    // always swap, so the host's vsync paces us
    let _ = w.backend.submit(damage.as_deref());
    render::post_render(s, w.backend.renderer(), &output, &elems, ncursor, clear);
    w.backend.window().request_redraw();
    s.backend = Backend::Winit(w);
}
