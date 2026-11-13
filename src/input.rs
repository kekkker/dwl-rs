// Keyboard and pointer handling, keybindings, interactive move/resize.
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent, InputTime, KeyState,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    desktop::{Window, WindowSurfaceType, layer_map_for_output, utils::under_from_surface_tree},
    input::{
        keyboard::{FilterResult, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::get_parent,
        pointer_constraints::{PointerConstraint, with_pointer_constraint},
        shell::wlr_layer::Layer,
    },
};

use crate::{config::*, state::State, wm::Client};


fn modmask(m: &ModifiersState) -> u32 {
    (m.shift as u32 * SHIFT) | (m.ctrl as u32 * CTRL) | (m.alt as u32 * ALT) | (m.logo as u32 * LOGO)
}

type Under = (WlSurface, Point<f64, Logical>, Option<Window>);

impl State {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);
        match event {
            InputEvent::Keyboard { event } => {
                let pressed = event.state() == KeyState::Pressed;
                let kb = self.seat.get_keyboard().unwrap();
                let serial = SERIAL_COUNTER.next_serial();
                let action = kb.input(self, event.key_code(), event.state(), serial, event.time(), |s, mods, h| {
                    if pressed && !s.locked {
                        let m = modmask(mods);
                        for sym in h.modified_syms() {
                            if let Some(k) = s.keys.iter().find(|k| k.0 == m && k.1 == sym.raw()) {
                                return FilterResult::Intercept(k.2);
                            }
                        }
                    }
                    FilterResult::Forward
                });
                if let Some(a) = action {
                    self.run(a);
                }
            }
            InputEvent::PointerMotion { event } => {
                let ptr = self.seat.get_pointer().unwrap();
                let old = ptr.current_location();
                let under = self.surface_under(old);
                let (mut locked, mut confined) = (false, None);
                if let Some((s, origin, _)) = &under {
                    with_pointer_constraint(s, &ptr, |c| match c.as_deref() {
                        Some(c @ PointerConstraint::Locked(_)) => locked = c.is_active(),
                        Some(c @ PointerConstraint::Confined(_)) if c.is_active() => {
                            confined = Some((s.clone(), *origin, c.region().cloned()));
                        }
                        _ => {}
                    });
                }
                let rel = RelativeMotionEvent { delta: event.delta(), delta_unaccel: event.delta_unaccel(), time: event.time() };
                ptr.relative_motion(self, under.map(|(s, p, _)| (s, p)), &rel);
                let new = self.clamp(old, old + event.delta());
                // a locked pointer stays put; a confined one stays on its surface and region
                let escapes = confined.is_some_and(|(s, origin, region)| {
                    self.surface_under(new).is_none_or(|(ns, _, _)| ns != s)
                        || region.is_some_and(|r| !r.contains((new - origin).to_i32_round()))
                });
                if locked || escapes {
                    ptr.frame(self);
                    return;
                }
                self.motion(new, event.time());
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let Some(o) = self.mons.first().map(|m| m.output.clone()) else { return };
                let g = self.space.output_geometry(&o).unwrap();
                self.motion(event.position_transformed(g.size) + g.loc.to_f64(), event.time());
            }
            InputEvent::PointerButton { event } => self.button(event.button_code(), event.state(), event.time()),
            InputEvent::PointerAxis { event } => {
                let src = event.source();
                let mut frame = AxisFrame::new(event.time()).source(src);
                for axis in [Axis::Horizontal, Axis::Vertical] {
                    let v120 = event.amount_v120(axis);
                    let amount = event.amount(axis).unwrap_or_else(|| v120.unwrap_or(0.0) * 15.0 / 120.0);
                    if amount != 0.0 {
                        frame = frame.value(axis, amount);
                        if let Some(d) = v120 {
                            frame = frame.v120(axis, d as i32);
                        }
                    }
                    if src == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                        frame = frame.stop(axis);
                    }
                }
                let ptr = self.seat.get_pointer().unwrap();
                ptr.axis(self, frame);
                ptr.frame(self);
            }
            _ => {}
        }
    }

    /// Keep the pointer on some output.
    fn clamp(&self, old: Point<f64, Logical>, new: Point<f64, Logical>) -> Point<f64, Logical> {
        if self.mon_at(new).is_some() {
            return new;
        }
        let Some(mi) = self.mon_at(old).or(self.selmon_idx()) else { return old };
        let g = self.mon_area(mi).to_f64();
        (new.x.clamp(g.loc.x, g.loc.x + g.size.w - 1.0), new.y.clamp(g.loc.y, g.loc.y + g.size.h - 1.0)).into()
    }

    pub fn motion(&mut self, pos: Point<f64, Logical>, time: InputTime) {
        let ptr = self.seat.get_pointer().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        if let Some((w, off)) = self.grab.clone() {
            if let Some(ci) = self.client_idx(&w) {
                let g = self.clients[ci].geom;
                let geo = if self.grab_resize {
                    Rectangle::new(g.loc, (pos - g.loc.to_f64()).to_i32_round().to_size())
                } else {
                    Rectangle::new((pos - off).to_i32_round(), g.size)
                };
                self.resize(ci, geo, true);
            }
            ptr.motion(self, None, &MotionEvent { location: pos, serial, time });
            ptr.frame(self);
            return;
        }
        if SLOPPYFOCUS && let Some(mi) = self.mon_at(pos) {
            self.selmon = Some(self.mons[mi].output.clone());
        }
        let under = self.surface_under(pos);
        if !ptr.is_grabbed() {
            if SLOPPYFOCUS && let Some((_, _, Some(w))) = &under {
                self.focus(Some(w.clone()), false);
            }
            if under.is_none() {
                self.cursor_status = CursorImageStatus::default_named();
            }
        }
        let focus = under.map(|(s, p, _)| (s, p));
        ptr.motion(self, focus.clone(), &MotionEvent { location: pos, serial, time });
        ptr.frame(self);
        if let Some((s, _)) = focus {
            with_pointer_constraint(&s, &ptr, |c| c.map(|c| c.activate()));
        }
        self.redraw();
    }

    /// Re-evaluate what is under the pointer after the scene changed (dwl's motionnotify(0)).
    pub fn pointer_refresh(&mut self) {
        let ptr = self.seat.get_pointer().unwrap();
        if ptr.is_grabbed() || self.grab.is_some() {
            return;
        }
        let pos = ptr.current_location();
        let under = self.surface_under(pos).map(|(s, p, _)| (s, p));
        let time = InputTime::now();
        ptr.motion(self, under, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time });
        ptr.frame(self);
    }

    fn button(&mut self, button: u32, state: ButtonState, time: InputTime) {
        let ptr = self.seat.get_pointer().unwrap();
        let kb = self.seat.get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        let pos = ptr.current_location();
        if state == ButtonState::Pressed && !self.locked && !ptr.is_grabbed() {
            match self.surface_under(pos) {
                Some((_, _, Some(w))) => self.focus(Some(w), true),
                Some((s, _, None)) => {
                    let mut root = s;
                    while let Some(p) = get_parent(&root) {
                        root = p;
                    }
                    if self.layer_for(&root).is_some_and(|l| l.can_receive_keyboard_focus()) {
                        kb.set_focus(self, Some(root), serial);
                    }
                }
                None => {
                    if let Some(ci) = self.client_at(pos) {
                        self.focus(Some(self.clients[ci].window.clone()), true);
                    }
                }
            }
            let mods = modmask(&kb.modifier_state());
            if let Some(&(_, _, a)) = BUTTONS.iter().find(|b| b.0 == mods && b.1 == button) {
                self.run(a);
                return;
            }
        }
        if state == ButtonState::Released && self.grab.is_some() {
            // drop the window off on the monitor under the cursor
            let (w, _) = self.grab.take().unwrap();
            self.cursor_status = CursorImageStatus::default_named();
            if let Some(mi) = self.mon_at(pos) {
                self.selmon = Some(self.mons[mi].output.clone());
                if let Some(ci) = self.client_idx(&w) {
                    self.setmon(ci, mi, 0);
                }
            }
            self.pointer_refresh();
            return;
        }
        ptr.button(self, &ButtonEvent { button, state, serial, time });
        ptr.frame(self);
    }

    pub fn start_grab(&mut self, resize: bool) {
        let ptr = self.seat.get_pointer().unwrap();
        let pos = ptr.current_location();
        let ci = match self.surface_under(pos) {
            Some((_, _, Some(w))) => self.client_idx(&w),
            Some(_) => None,
            None => self.client_at(pos),
        };
        let Some(ci) = ci.filter(|&ci| !self.clients[ci].fullscreen) else { return };
        let w = self.clients[ci].window.clone();
        self.setfloating(ci, true);
        let g = self.clients[ci].geom;
        self.grab_resize = resize;
        if resize {
            let corner = (g.loc + g.size).to_f64();
            let time = InputTime::now();
            ptr.motion(self, None, &MotionEvent { location: corner, serial: SERIAL_COUNTER.next_serial(), time });
            ptr.frame(self);
            self.grab = Some((w, (0.0, 0.0).into()));
            self.cursor_status = CursorImageStatus::Named(CursorIcon::SeResize);
        } else {
            self.grab = Some((w, pos - g.loc.to_f64()));
            self.cursor_status = CursorImageStatus::Named(CursorIcon::AllScroll);
        }
        self.redraw();
    }

    /// Topmost client whose frame (including the border) contains `pos`.
    fn client_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let order: [&dyn Fn(&Client) -> bool; 3] =
            [&|c| c.fullscreen, &|c| c.floating && !c.fullscreen, &|c| !c.floating && !c.fullscreen];
        let stack: Vec<usize> = self.space.elements().rev().filter_map(|w| self.client_idx(w)).collect();
        order
            .iter()
            .find_map(|pred| stack.iter().copied().find(|&ci| pred(&self.clients[ci]) && self.clients[ci].geom.to_f64().contains(pos)))
    }

    /// Surface under `pos`, in dwl's stacking order.
    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<Under> {
        let mi = self.mon_at(pos)?;
        let o = &self.mons[mi].output;
        let og = self.mon_area(mi).loc;
        if self.locked {
            let (_, l) = self.lock_surfaces.iter().find(|(lo, _)| lo == o)?;
            return under_from_surface_tree(l.wl_surface(), pos - og.to_f64(), (0, 0), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + og).to_f64(), None));
        }
        let map = layer_map_for_output(o);
        let layer = |l: Layer| {
            map.layers_on(l).rev().find_map(|ls| {
                let g = map.layer_geometry(ls)?.loc + og;
                ls.surface_under(pos - g.to_f64(), WindowSurfaceType::ALL).map(|(s, p)| (s, (p + g).to_f64(), None))
            })
        };
        let stack: Vec<usize> = self.space.elements().rev().filter_map(|w| self.client_idx(w)).collect();
        let win = |pred: &dyn Fn(&Client) -> bool| {
            stack.iter().filter(|&&ci| pred(&self.clients[ci])).find_map(|&ci| {
                let w = &self.clients[ci].window;
                let rl = self.space.element_location(w)? - w.geometry().loc;
                w.surface_under(pos - rl.to_f64(), WindowSurfaceType::ALL).map(|(s, p)| (s, (p + rl).to_f64(), Some(w.clone())))
            })
        };
        layer(Layer::Overlay)
            .or_else(|| win(&|c| c.fullscreen))
            .or_else(|| layer(Layer::Top))
            .or_else(|| win(&|c| c.floating && !c.fullscreen))
            .or_else(|| win(&|c| !c.floating && !c.fullscreen))
            .or_else(|| layer(Layer::Bottom))
            .or_else(|| layer(Layer::Background))
    }
}
