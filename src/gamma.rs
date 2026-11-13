// wlr-gamma-control: lets gammastep/wlsunset set the CRTC gamma ramps.
use std::os::unix::fs::FileExt;

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols_wlr::gamma_control::v1::server::{
            zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
            zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
        },
        wayland_server::{Client, DataInit, DisplayHandle, New, backend::ClientId},
    },
    wayland::{Dispatch2, GlobalDispatch2},
};

use crate::state::State;

pub struct Global;
pub struct Manager;
pub struct Control(Option<Output>);

pub fn init(dh: &DisplayHandle) {
    dh.create_global::<State, ZwlrGammaControlManagerV1, _>(1, Global);
}

impl GlobalDispatch2<ZwlrGammaControlManagerV1, State> for Global {
    fn bind(&self, _: &mut State, _: &DisplayHandle, _: &Client, res: New<ZwlrGammaControlManagerV1>, init: &mut DataInit<'_, State>) {
        init.init(res, Manager);
    }
}

impl Dispatch2<ZwlrGammaControlManagerV1, State> for Manager {
    fn request(
        &self,
        s: &mut State,
        _: &Client,
        _: &ZwlrGammaControlManagerV1,
        req: zwlr_gamma_control_manager_v1::Request,
        _: &DisplayHandle,
        init: &mut DataInit<'_, State>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = req else { return };
        let o = Output::from_resource(&output);
        let ctl = init.init(id, Control(o.clone()));
        // one client per output, like wlroots
        let size = o.as_ref().filter(|o| !s.gamma.iter().any(|(go, _)| go == *o)).and_then(|o| s.backend.gamma_size(o));
        match (o, size) {
            (Some(o), Some(size)) => {
                ctl.gamma_size(size);
                s.gamma.push((o, ctl));
            }
            _ => ctl.failed(),
        }
    }
}

impl Dispatch2<ZwlrGammaControlV1, State> for Control {
    fn request(
        &self,
        s: &mut State,
        _: &Client,
        ctl: &ZwlrGammaControlV1,
        req: zwlr_gamma_control_v1::Request,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State>,
    ) {
        let zwlr_gamma_control_v1::Request::SetGamma { fd } = req else { return };
        let Some(o) = self.0.as_ref().filter(|o| s.gamma.iter().any(|(go, c)| go == *o && c == ctl)) else { return };
        let Some(size) = s.backend.gamma_size(o) else { return };
        // three ramps of `size` u16 values, read from offset 0
        let mut buf = vec![0u8; size as usize * 3 * 2];
        let ramp = std::fs::File::from(fd)
            .read_exact_at(&mut buf, 0)
            .map(|_| buf.as_chunks::<2>().0.iter().map(|b| u16::from_ne_bytes(*b)).collect());
        if let Err(e) = ramp.and_then(|r| s.backend.set_gamma(o, Some(r))) {
            tracing::warn!("gamma for {}: {e}", o.name());
            ctl.failed();
            s.gamma.retain(|(_, c)| c != ctl);
        }
    }

    fn destroyed(&self, s: &mut State, _: ClientId, ctl: &ZwlrGammaControlV1) {
        let Some(o) = &self.0 else { return };
        if s.gamma.iter().any(|(_, c)| c == ctl) {
            s.gamma.retain(|(_, c)| c != ctl);
            let _ = s.backend.set_gamma(o, None); // back to linear
        }
    }
}
