// dwl's window management: clients, monitors, tags, layouts, actions.
use std::io::Write;

use smithay::{
    backend::renderer::element::solid::SolidColorBuffer,
    desktop::{Window, layer_map_for_output},
    output::{Output, Scale},
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{KeyboardInteractivity, Layer},
            xdg::{SurfaceCachedState, XdgToplevelSurfaceData},
        },
    },
};

use crate::{config::*, state::State};

pub const TAGMASK: u32 = (1 << TAGCOUNT) - 1;

pub struct Key(pub u32, pub u32, pub Action);

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Spawn(&'static [&'static str]),
    FocusStack(i32),
    IncNmaster(i32),
    SetMfact(f32),
    Zoom,
    View(u32),
    ToggleView(u32),
    Tag(u32),
    ToggleTag(u32),
    KillClient,
    SetLayout(Option<usize>),
    ToggleFloating,
    ToggleFullscreen,
    ToggleSticky,
    FocusMon(i32),
    TagMon(i32),
    #[allow(dead_code)] // bindable, your config just doesn't use it
    Quit,
    Chvt(i32),
    MoveResize(bool), // true = resize
}

pub struct Layout {
    pub symbol: &'static str,
    pub arrange: Option<fn(&mut State, usize)>,
}

pub struct Rule {
    pub app_id: Option<&'static str>,
    pub title: Option<&'static str>,
    pub tags: u32,
    pub floating: bool,
    pub monitor: i32,
}

pub struct MonitorRule {
    pub name: Option<&'static str>,
    pub mfact: f32,
    pub nmaster: i32,
    pub scale: f64,
    pub layout: usize,
}

pub struct Client {
    pub window: Window,
    pub mon: Output,
    pub tags: u32,
    pub floating: bool,
    pub fullscreen: bool,
    pub urgent: bool,
    pub sticky: bool,
    pub geom: Rectangle<i32, Logical>, // includes the border
    pub prev: Rectangle<i32, Logical>,
    pub borders: [SolidColorBuffer; 4],
}

impl Client {
    pub fn bw(&self) -> i32 {
        if self.fullscreen { 0 } else { BORDERPX }
    }
}

pub struct Monitor {
    pub output: Output,
    pub tagset: [u32; 2],
    pub seltags: usize,
    pub lt: [usize; 2],
    pub sellt: usize,
    pub mfact: f32,
    pub nmaster: i32,
    pub ltsymbol: String,
    pub fullscreen_bg: SolidColorBuffer,
}

impl Monitor {
    pub fn tags(&self) -> u32 {
        self.tagset[self.seltags]
    }
    pub fn layout(&self) -> &'static Layout {
        &LAYOUTS[self.lt[self.sellt]]
    }
}

pub fn title_appid(w: &Window) -> (String, String) {
    with_states(w.toplevel().unwrap().wl_surface(), |s| {
        let d = s.data_map.get::<XdgToplevelSurfaceData>().unwrap().lock().unwrap();
        (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default())
    })
}

/// Like dwl's spawn: own session, stdout goes to our stderr.
pub fn command(cmd: &[&str]) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(cmd[0]);
    c.args(&cmd[1..]).stdout(std::io::stderr());
    unsafe {
        c.pre_exec(|| {
            let _ = smithay::reexports::rustix::process::setsid();
            Ok(())
        });
    }
    c
}

pub fn spawn(cmd: &[&str]) -> Option<u32> {
    match command(cmd).spawn() {
        Ok(mut child) => {
            let pid = child.id();
            std::thread::spawn(move || child.wait()); // reap
            Some(pid)
        }
        Err(e) => {
            tracing::warn!("spawn {:?}: {e}", cmd);
            None
        }
    }
}

pub fn tile(s: &mut State, mi: usize) {
    let m = &s.mons[mi];
    let tiled: Vec<usize> = (0..s.clients.len())
        .filter(|&i| s.visible_on(&s.clients[i], m) && !s.clients[i].floating && !s.clients[i].fullscreen)
        .collect();
    let n = tiled.len() as i32;
    if n == 0 {
        return;
    }
    let w = s.win_area(mi);
    let (nmaster, mfact) = (m.nmaster, m.mfact);
    let mw = if n > nmaster {
        if nmaster > 0 { (w.size.w as f32 * mfact) as i32 } else { 0 }
    } else {
        w.size.w
    };
    let (mut my, mut ty) = (0, 0);
    for (i, ci) in tiled.into_iter().enumerate() {
        let i = i as i32;
        if i < nmaster {
            let h = (w.size.h - my) / (n.min(nmaster) - i);
            s.resize(ci, Rectangle::new((w.loc.x, w.loc.y + my).into(), (mw, h).into()), false);
            my += s.clients[ci].geom.size.h;
        } else {
            let h = (w.size.h - ty) / (n - i);
            s.resize(ci, Rectangle::new((w.loc.x + mw, w.loc.y + ty).into(), (w.size.w - mw, h).into()), false);
            ty += s.clients[ci].geom.size.h;
        }
    }
}

pub fn monocle(s: &mut State, mi: usize) {
    let w = s.win_area(mi);
    let mut n = 0;
    for ci in 0..s.clients.len() {
        let c = &s.clients[ci];
        if s.visible_on(c, &s.mons[mi]) && !c.floating && !c.fullscreen {
            s.resize(ci, w, false);
            n += 1;
        }
    }
    if n > 0 {
        s.mons[mi].ltsymbol = format!("[{n}]");
    }
    if let Some(w) = s.focustop(mi) {
        s.space.raise_element(&w, false);
    }
}

impl State {
    pub fn visible_on(&self, c: &Client, m: &Monitor) -> bool {
        c.mon == m.output && (c.tags & m.tags() != 0 || c.sticky)
    }

    pub fn mon_idx(&self, o: &Output) -> Option<usize> {
        self.mons.iter().position(|m| &m.output == o)
    }

    pub fn selmon_idx(&self) -> Option<usize> {
        self.selmon.as_ref().and_then(|o| self.mon_idx(o))
    }

    pub fn client_idx(&self, w: &Window) -> Option<usize> {
        self.clients.iter().position(|c| &c.window == w)
    }

    pub fn mon_area(&self, mi: usize) -> Rectangle<i32, Logical> {
        self.space.output_geometry(&self.mons[mi].output).unwrap_or_default()
    }

    pub fn win_area(&self, mi: usize) -> Rectangle<i32, Logical> {
        let m = self.mon_area(mi);
        let z = layer_map_for_output(&self.mons[mi].output).non_exclusive_zone();
        Rectangle::new(m.loc + z.loc, z.size)
    }

    pub fn mon_at(&self, p: Point<f64, Logical>) -> Option<usize> {
        self.mons.iter().position(|m| self.space.output_geometry(&m.output).is_some_and(|g| g.to_f64().contains(p)))
    }

    /// The most recently focused client visible on monitor `mi`.
    pub fn focustop(&self, mi: usize) -> Option<Window> {
        let m = self.mons.get(mi)?;
        self.fstack
            .iter()
            .find(|w| self.client_idx(w).is_some_and(|ci| self.visible_on(&self.clients[ci], m)))
            .cloned()
    }

    pub fn sel(&self) -> Option<usize> {
        self.selmon_idx().and_then(|mi| self.focustop(mi)).and_then(|w| self.client_idx(&w))
    }

    pub fn resize(&mut self, ci: usize, mut geo: Rectangle<i32, Logical>, interact: bool) {
        let bw = self.clients[ci].bw();
        geo.size.w = geo.size.w.max(1 + 2 * bw);
        geo.size.h = geo.size.h.max(1 + 2 * bw);
        // keep the client reachable (dwl's applybounds)
        let bounds = if interact {
            self.mons.iter().filter_map(|m| self.space.output_geometry(&m.output)).reduce(|a, b| a.merge(b))
        } else {
            self.mon_idx(&self.clients[ci].mon).map(|mi| self.win_area(mi))
        };
        if let Some(b) = bounds {
            if geo.loc.x >= b.loc.x + b.size.w {
                geo.loc.x = b.loc.x + b.size.w - geo.size.w;
            }
            if geo.loc.y >= b.loc.y + b.size.h {
                geo.loc.y = b.loc.y + b.size.h - geo.size.h;
            }
            geo.loc.x = geo.loc.x.max(b.loc.x - geo.size.w + 1);
            geo.loc.y = geo.loc.y.max(b.loc.y - geo.size.h + 1);
        }
        let c = &mut self.clients[ci];
        c.geom = geo;
        let t = c.window.toplevel().unwrap();
        t.with_pending_state(|s| s.size = Some((geo.size.w - 2 * bw, geo.size.h - 2 * bw).into()));
        if t.is_initial_configure_sent() {
            t.send_pending_configure();
        }
        let loc = geo.loc + Point::from((bw, bw));
        if self.space.element_location(&c.window) != Some(loc) {
            self.space.map_element(c.window.clone(), loc, false);
        }
        self.redraw();
    }

    pub fn arrange(&mut self, mi: usize) {
        let m = &self.mons[mi];
        layer_map_for_output(&m.output).arrange(); // usable area follows output size and bars
        for ci in 0..self.clients.len() {
            let c = &self.clients[ci];
            if c.mon != m.output {
                continue;
            }
            let mapped = self.space.elements().any(|e| e == &c.window);
            if self.visible_on(c, m) && !mapped {
                self.space.map_element(c.window.clone(), c.geom.loc + Point::from((c.bw(), c.bw())), false);
            } else if !self.visible_on(c, m) && mapped {
                self.space.unmap_elem(&c.window);
            }
        }
        let lt = self.mons[mi].layout();
        self.mons[mi].ltsymbol = lt.symbol.into();
        if let Some(f) = lt.arrange {
            f(self, mi);
        }
        let area = self.mon_area(mi);
        for ci in 0..self.clients.len() {
            if self.clients[ci].fullscreen && self.visible_on(&self.clients[ci], &self.mons[mi]) {
                self.resize(ci, area, false);
            }
        }
        self.pointer_refresh();
        self.update_idle_inhibit();
        self.printstatus();
        self.redraw();
    }

    pub fn arrange_all(&mut self) {
        for mi in 0..self.mons.len() {
            self.arrange(mi);
        }
    }

    pub fn focus(&mut self, w: Option<Window>, lift: bool) {
        if self.locked {
            return;
        }
        if let (Some(w), true) = (&w, lift) {
            self.space.raise_element(w, false);
        }
        let kb = self.seat.get_keyboard().unwrap();
        let old = kb.current_focus();
        let new = w.as_ref().map(|w| w.toplevel().unwrap().wl_surface().clone());
        if old == new {
            return;
        }
        if let Some(w) = &w {
            let ci = self.client_idx(w).unwrap();
            self.fstack.retain(|f| f != w);
            self.fstack.insert(0, w.clone());
            self.selmon = Some(self.clients[ci].mon.clone());
            self.clients[ci].urgent = false;
        }
        // an exclusive top/overlay layer surface (launcher, locker) keeps the keyboard
        if let Some(l) = old.as_ref().and_then(|s| self.layer_for(s))
            && l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive
            && matches!(l.layer(), Layer::Top | Layer::Overlay)
        {
            return;
        }
        for c in &self.clients {
            let active = Some(&c.window) == w.as_ref();
            if c.window.set_activated(active) {
                c.window.toplevel().unwrap().send_pending_configure();
            }
        }
        kb.set_focus(self, new, SERIAL_COUNTER.next_serial());
        self.printstatus();
        self.redraw();
    }

    pub fn focus_top(&mut self) {
        let w = self.selmon_idx().and_then(|mi| self.focustop(mi));
        self.focus(w, true);
    }

    pub fn setfloating(&mut self, ci: usize, floating: bool) {
        self.clients[ci].floating = floating;
        if floating {
            self.space.raise_element(&self.clients[ci].window.clone(), false);
        }
        self.arrange_client_mon(ci);
    }

    pub fn setfullscreen(&mut self, ci: usize, fullscreen: bool) {
        let c = &mut self.clients[ci];
        if c.fullscreen == fullscreen {
            return;
        }
        c.fullscreen = fullscreen;
        c.window.toplevel().unwrap().with_pending_state(|s| {
            if fullscreen { s.states.set(XdgState::Fullscreen) } else { s.states.unset(XdgState::Fullscreen) }
        });
        if fullscreen {
            c.prev = c.geom;
            let area = self.mon_idx(&self.clients[ci].mon).map(|mi| self.mon_area(mi));
            if let Some(area) = area {
                self.resize(ci, area, false);
            }
        } else {
            let prev = c.prev;
            self.resize(ci, prev, false);
        }
        self.arrange_client_mon(ci);
    }

    fn arrange_client_mon(&mut self, ci: usize) {
        if let Some(mi) = self.mon_idx(&self.clients[ci].mon) {
            self.arrange(mi);
        }
    }

    /// Move client to monitor `mi`; `newtags` 0 means the monitor's current tags.
    pub fn setmon(&mut self, ci: usize, mi: usize, newtags: u32) {
        let old = self.mon_idx(&self.clients[ci].mon);
        let m = &self.mons[mi];
        if old == Some(mi) {
            return;
        }
        let tags = if newtags != 0 { newtags } else { m.tags() };
        let c = &mut self.clients[ci];
        c.mon = m.output.clone();
        c.tags = tags;
        c.prev = c.geom;
        let geom = c.geom;
        self.resize(ci, geom, false);
        if self.clients[ci].fullscreen {
            let area = self.mon_area(mi);
            self.resize(ci, area, false);
        }
        if let Some(old) = old {
            self.arrange(old);
        }
        self.arrange(mi);
        self.focus_top();
    }

    /// A toplevel committed its first buffer (dwl's mapnotify).
    pub fn map_client(&mut self, window: Window) {
        let Some(mut mi) = self.selmon_idx() else { return };
        let t = window.toplevel().unwrap().clone();
        let (title, app_id) = title_appid(&window);
        let (min, max) = with_states(t.wl_surface(), |s| {
            let mut c = s.cached_state.get::<SurfaceCachedState>();
            let c = c.current();
            (c.min_size, c.max_size)
        });
        let mut floating = t.parent().is_some() || (min.w > 0 && min.h > 0 && min == max);
        let mut tags = 0;
        for r in RULES {
            if r.title.is_none_or(|x| title.contains(x)) && r.app_id.is_none_or(|x| app_id.contains(x)) {
                floating = r.floating;
                tags |= r.tags;
                if r.monitor >= 0 && (r.monitor as usize) < self.mons.len() {
                    mi = r.monitor as usize;
                }
            }
        }
        let size = window.geometry().size + (2 * BORDERPX, 2 * BORDERPX).into();
        let m = &self.mons[mi];
        let w = self.win_area(mi);
        // always-center: floating windows start centered on their monitor
        let geom = Rectangle::new(w.loc + Point::from(((w.size.w - size.w) / 2, (w.size.h - size.h) / 2)), size);
        let fullscreen = t.with_pending_state(|s| s.states.contains(XdgState::Fullscreen));
        self.clients.insert(
            0,
            Client {
                window: window.clone(),
                mon: m.output.clone(),
                tags: if tags & TAGMASK != 0 { tags & TAGMASK } else { m.tags() },
                floating,
                fullscreen: false,
                urgent: false,
                sticky: false,
                geom,
                prev: geom,
                borders: Default::default(),
            },
        );
        self.fstack.insert(0, window.clone());
        if floating {
            self.resize(0, geom, false);
        }
        // a new fullscreen client takes over, others on the monitor leave fullscreen
        for ci in 1..self.clients.len() {
            if fullscreen && self.clients[ci].fullscreen && self.clients[ci].mon == self.clients[0].mon {
                self.setfullscreen(ci, false);
            }
        }
        if fullscreen {
            self.setfullscreen(0, true);
        }
        self.arrange(mi);
        self.focus_top();
    }

    pub fn unmap_client(&mut self, w: &Window) {
        let Some(ci) = self.client_idx(w) else { return };
        if self.grab.as_ref().is_some_and(|(g, _)| g == w) {
            self.grab = None;
        }
        let c = self.clients.remove(ci);
        self.fstack.retain(|f| f != w);
        self.space.unmap_elem(w);
        if let Some(mi) = self.mon_idx(&c.mon) {
            self.arrange(mi);
        }
        self.focus_top();
    }

    pub fn add_monitor(&mut self, output: Output) {
        let r = MONRULES.iter().find(|r| r.name.is_none_or(|n| n == output.name())).unwrap();
        output.change_current_state(None, None, Some(Scale::Fractional(r.scale)), None);
        self.mons.push(Monitor {
            output: output.clone(),
            tagset: [1, 1],
            seltags: 0,
            lt: [r.layout, if LAYOUTS.len() > 1 && r.layout != 1 { 1 } else { 0 }],
            sellt: 0,
            mfact: r.mfact,
            nmaster: r.nmaster,
            ltsymbol: LAYOUTS[r.layout].symbol.into(),
            fullscreen_bg: SolidColorBuffer::new((0, 0), FULLSCREEN_BG),
        });
        self.reposition_outputs();
        if self.selmon_idx().is_none() {
            self.selmon = Some(output);
        }
        // clients orphaned by a removed monitor come back here
        let mi = self.mons.len() - 1;
        for ci in 0..self.clients.len() {
            if self.mon_idx(&self.clients[ci].mon).is_none() {
                let tags = self.clients[ci].tags;
                self.setmon(ci, mi, tags);
            }
        }
        self.arrange_all();
        self.focus_top();
    }

    pub fn remove_monitor(&mut self, output: &Output) {
        let Some(mi) = self.mon_idx(output) else { return };
        for l in layer_map_for_output(output).layers() {
            l.layer_surface().send_close();
        }
        self.gamma.retain(|(o, c)| {
            if o == output {
                c.failed();
            }
            o != output
        });
        self.mons.remove(mi);
        self.space.unmap_output(output);
        if self.selmon.as_ref() == Some(output) {
            self.selmon = self.mons.first().map(|m| m.output.clone());
        }
        self.reposition_outputs();
        if let Some(sm) = self.selmon_idx() {
            for ci in 0..self.clients.len() {
                if &self.clients[ci].mon == output {
                    let tags = self.clients[ci].tags;
                    self.setmon(ci, sm, tags);
                }
            }
        }
        self.arrange_all();
        self.focus_top();
    }

    /// Lay outputs out left to right, like wlr_output_layout's auto placement.
    fn reposition_outputs(&mut self) {
        let mut x = 0;
        for m in &self.mons {
            let o = m.output.clone();
            o.change_current_state(None, None, None, Some((x, 0).into()));
            self.space.map_output(&o, (x, 0));
            x += self.space.output_geometry(&o).map_or(0, |g| g.size.w);
        }
    }

    pub fn printstatus(&mut self) {
        let mut out = String::new();
        for (mi, m) in self.mons.iter().enumerate() {
            let name = m.output.name();
            let (mut occ, mut urg) = (0, 0);
            for c in self.clients.iter().filter(|c| c.mon == m.output) {
                occ |= c.tags;
                if c.urgent {
                    urg |= c.tags;
                }
            }
            let sel = match self.focustop(mi).and_then(|w| self.client_idx(&w)) {
                Some(ci) => {
                    let c = &self.clients[ci];
                    let (title, appid) = title_appid(&c.window);
                    out += &format!("{name} title {title}\n{name} appid {appid}\n");
                    out += &format!("{name} fullscreen {}\n{name} floating {}\n", c.fullscreen as u8, c.floating as u8);
                    c.tags
                }
                None => {
                    out += &format!("{name} title \n{name} appid \n{name} fullscreen \n{name} floating \n");
                    0
                }
            };
            out += &format!("{name} selmon {}\n", (self.selmon.as_ref() == Some(&m.output)) as u8);
            out += &format!("{name} tags {occ} {} {sel} {urg}\n", m.tags());
            out += &format!("{name} layout {}\n", m.ltsymbol);
        }
        if out != self.last_status {
            let _ = self.status.write_all(out.as_bytes());
            let _ = self.status.flush();
            self.last_status = out;
        }
    }

    pub fn run(&mut self, a: Action) {
        use Action::*;
        let sm = self.selmon_idx();
        let sel = self.sel();
        match a {
            Spawn(cmd) => {
                spawn(cmd);
            }
            FocusStack(dir) => {
                let (Some(mi), Some(ci)) = (sm, sel) else { return };
                if self.clients[ci].fullscreen {
                    return;
                }
                let vis: Vec<usize> =
                    (0..self.clients.len()).filter(|&i| self.visible_on(&self.clients[i], &self.mons[mi])).collect();
                let pos = vis.iter().position(|&i| i == ci).unwrap() as i32;
                let next = vis[(pos + dir).rem_euclid(vis.len() as i32) as usize];
                self.focus(Some(self.clients[next].window.clone()), true);
            }
            IncNmaster(i) => {
                let Some(mi) = sm else { return };
                self.mons[mi].nmaster = (self.mons[mi].nmaster + i).max(0);
                self.arrange(mi);
            }
            SetMfact(f) => {
                let Some(mi) = sm else { return };
                let m = &mut self.mons[mi];
                let f = m.mfact + f;
                if m.layout().arrange.is_none() || !(0.1..=0.9).contains(&f) {
                    return;
                }
                m.mfact = f;
                self.arrange(mi);
            }
            Zoom => {
                let (Some(mi), Some(ci)) = (sm, sel) else { return };
                if self.mons[mi].layout().arrange.is_none() || self.clients[ci].floating {
                    return;
                }
                let tiled: Vec<usize> = (0..self.clients.len())
                    .filter(|&i| self.visible_on(&self.clients[i], &self.mons[mi]) && !self.clients[i].floating)
                    .collect();
                // the master swaps with the next tiled client, anyone else becomes master
                let target = if tiled.first() == Some(&ci) { tiled.get(1).copied() } else { Some(ci) };
                let Some(t) = target else { return };
                let c = self.clients.remove(t);
                let w = c.window.clone();
                self.clients.insert(0, c);
                self.focus(Some(w), true);
                self.arrange(mi);
            }
            View(ui) => {
                let Some(mi) = sm else { return };
                let m = &mut self.mons[mi];
                if ui & TAGMASK == m.tags() {
                    return;
                }
                m.seltags ^= 1;
                if ui & TAGMASK != 0 {
                    m.tagset[m.seltags] = ui & TAGMASK;
                }
                self.focus_top();
                self.arrange(mi);
            }
            ToggleView(ui) => {
                let Some(mi) = sm else { return };
                let m = &mut self.mons[mi];
                let new = m.tags() ^ (ui & TAGMASK);
                if new != 0 {
                    m.tagset[m.seltags] = new;
                    self.focus_top();
                    self.arrange(mi);
                }
            }
            Tag(ui) => {
                let (Some(mi), Some(ci)) = (sm, sel) else { return };
                if ui & TAGMASK != 0 {
                    self.clients[ci].tags = ui & TAGMASK;
                    self.focus_top();
                    self.arrange(mi);
                }
            }
            ToggleTag(ui) => {
                let (Some(mi), Some(ci)) = (sm, sel) else { return };
                let new = self.clients[ci].tags ^ (ui & TAGMASK);
                if new != 0 {
                    self.clients[ci].tags = new;
                    self.focus_top();
                    self.arrange(mi);
                }
            }
            KillClient => {
                if let Some(ci) = sel {
                    self.clients[ci].window.toplevel().unwrap().send_close();
                }
            }
            SetLayout(l) => {
                let Some(mi) = sm else { return };
                let m = &mut self.mons[mi];
                if l.is_none_or(|l| l != m.lt[m.sellt]) {
                    m.sellt ^= 1;
                }
                if let Some(l) = l {
                    m.lt[m.sellt] = l;
                }
                self.arrange(mi);
            }
            ToggleFloating => {
                if let Some(ci) = sel
                    && !self.clients[ci].fullscreen
                {
                    self.setfloating(ci, !self.clients[ci].floating);
                }
            }
            ToggleFullscreen => {
                if let Some(ci) = sel {
                    self.setfullscreen(ci, !self.clients[ci].fullscreen);
                }
            }
            ToggleSticky => {
                if let Some(ci) = sel {
                    self.clients[ci].sticky ^= true;
                    self.arrange_client_mon(ci);
                }
            }
            FocusMon(dir) => {
                let Some(mi) = sm else { return };
                let n = self.mons.len() as i32;
                let i = (mi as i32 + dir).rem_euclid(n) as usize;
                self.selmon = Some(self.mons[i].output.clone());
                self.focus(self.focustop(i), true);
            }
            TagMon(dir) => {
                let (Some(mi), Some(ci)) = (sm, sel) else { return };
                let n = self.mons.len() as i32;
                self.setmon(ci, (mi as i32 + dir).rem_euclid(n) as usize, 0);
            }
            Quit => self.running = false,
            Chvt(n) => self.backend.change_vt(n),
            MoveResize(resize) => self.start_grab(resize),
        }
    }
}
