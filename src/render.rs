// Scene building (dwl's layer order), borders, cursor, frame callbacks and screencopy.
use std::collections::HashMap;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::{
                AsRenderElements, Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::{GlesRenderer, GlesTexture},
        },
    },
    desktop::{layer_map_for_output, utils::send_frames_surface_tree},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
    utils::{Clock, IsAlive, Logical, Monotonic, Point, Rectangle, Transform},
    wayland::{
        compositor::{SurfaceData, send_surface_state, with_states},
        fractional_scale::with_fractional_scale,
        image_copy_capture::CaptureFailureReason,
        shell::wlr_layer::Layer,
        shm,
    },
};
use xcursor::{CursorTheme, parser::parse_xcursor};

use crate::{config::*, state::State};

smithay::backend::renderer::element::render_elements! {
    pub Elem<=GlesRenderer>;
    Surface=WaylandSurfaceRenderElement<GlesRenderer>,
    Solid=SolidColorRenderElement,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
}

type CursorImage = Option<(MemoryRenderBuffer, Point<i32, Logical>)>;

#[derive(Default)]
pub struct CursorCache {
    theme: Option<CursorTheme>,
    images: HashMap<(String, u32), CursorImage>,
}

impl CursorCache {
    fn get(&mut self, name: &str, scale: u32) -> CursorImage {
        let theme = self
            .theme
            .get_or_insert_with(|| CursorTheme::load(&std::env::var("XCURSOR_THEME").unwrap_or("default".into())));
        let size: u32 = std::env::var("XCURSOR_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(24) * scale;
        let load = || {
            let images = parse_xcursor(&std::fs::read(theme.load_icon(name)?).ok()?)?;
            let img = images.iter().min_by_key(|i| (i.size as i32 - size as i32).abs())?;
            let buf = MemoryRenderBuffer::from_slice(
                &img.pixels_rgba, // raw file order, which is BGRA = Argb8888
                Fourcc::Argb8888,
                (img.width as i32, img.height as i32),
                scale as i32,
                Transform::Normal,
                None,
            );
            Some((buf, Point::from((img.xhot as i32 / scale as i32, img.yhot as i32 / scale as i32))))
        };
        self.images.entry((name.into(), scale)).or_insert_with(load).clone()
    }
}

fn border_rects(g: Rectangle<i32, Logical>, bw: i32) -> [Rectangle<i32, Logical>; 4] {
    let (x, y, w, h) = (g.loc.x, g.loc.y, g.size.w, g.size.h);
    [
        Rectangle::new((x, y).into(), (w, bw).into()),
        Rectangle::new((x, y + h - bw).into(), (w, bw).into()),
        Rectangle::new((x, y + bw).into(), (bw, h - 2 * bw).into()),
        Rectangle::new((x + w - bw, y + bw).into(), (bw, h - 2 * bw).into()),
    ]
}

/// Front-to-back elements for `output`, how many of them are the cursor, and the clear color.
pub fn output_elements(s: &mut State, r: &mut GlesRenderer, output: &Output) -> (Vec<Elem>, usize, [f32; 4]) {
    let scale = output.current_scale().fractional_scale();
    let Some(og) = s.space.output_geometry(output) else { return (vec![], 0, ROOTCOLOR) };
    let mut e: Vec<Elem> = vec![];

    let ptr = s.seat.get_pointer().unwrap().current_location();
    if og.to_f64().contains(ptr) {
        let pos = ptr - og.loc.to_f64();
        match s.cursor_status.clone() {
            CursorImageStatus::Hidden => {}
            CursorImageStatus::Surface(surf) if surf.alive() => {
                let hot = with_states(&surf, |st| {
                    st.data_map.get::<CursorImageSurfaceData>().map(|a| a.lock().unwrap().hotspot)
                })
                .unwrap_or_default();
                let loc = (pos - hot.to_f64()).to_physical(scale).to_i32_round();
                e.extend(
                    render_elements_from_surface_tree(r, &surf, loc, scale, 1.0, Kind::Cursor).into_iter().map(Elem::Surface),
                );
            }
            status => {
                let name = if let CursorImageStatus::Named(i) = status { i.name() } else { "default" };
                let iscale = scale.ceil() as u32;
                if let Some((buf, hot)) = s.cursors.get(name, iscale).or_else(|| s.cursors.get("default", iscale))
                    && let Ok(el) = MemoryRenderBufferRenderElement::from_buffer(
                        r,
                        (pos - hot.to_f64()).to_physical(scale),
                        &buf,
                        None,
                        None,
                        None,
                        Kind::Cursor,
                    )
                {
                    e.push(Elem::Cursor(el));
                }
            }
        }
    }
    let ncursor = e.len();

    if s.locked {
        if let Some((_, l)) = s.lock_surfaces.iter().find(|(o, _)| o == output) {
            let els = render_elements_from_surface_tree(r, l.wl_surface(), (0, 0), scale, 1.0, Kind::Unspecified);
            e.extend(els.into_iter().map(Elem::Surface));
        }
        return (e, ncursor, [0.0, 0.0, 0.0, 1.0]);
    }

    // refresh border and fullscreen background colors
    let focused = s.seat.get_keyboard().unwrap().current_focus();
    for c in &mut s.clients {
        let color = if c.urgent {
            URGENTCOLOR
        } else if focused.as_ref() == Some(c.window.toplevel().unwrap().wl_surface()) {
            FOCUSCOLOR
        } else {
            BORDERCOLOR
        };
        let rects = border_rects(c.geom, c.bw());
        for (b, rect) in c.borders.iter_mut().zip(rects) {
            b.update(rect.size, color);
        }
    }
    let mi = s.mon_idx(output);
    if let Some(mi) = mi {
        s.mons[mi].fullscreen_bg.update(og.size, FULLSCREEN_BG);
    }
    let s = &*s;

    let map = layer_map_for_output(output);
    let layers = |e: &mut Vec<Elem>, r: &mut GlesRenderer, layer| {
        for l in map.layers_on(layer).rev() {
            let loc = map.layer_geometry(l).unwrap_or_default().loc.to_physical_precise_round(scale);
            let els: Vec<WaylandSurfaceRenderElement<_>> = l.render_elements(r, loc, scale.into(), 1.0);
            e.extend(els.into_iter().map(Elem::Surface));
        }
    };
    let clients: Vec<usize> = s
        .space
        .elements()
        .rev()
        .filter_map(|w| s.client_idx(w))
        .filter(|&ci| s.clients[ci].geom.overlaps(og))
        .collect();
    let window = |e: &mut Vec<Elem>, r: &mut GlesRenderer, ci: usize| {
        let c = &s.clients[ci];
        let Some(loc) = s.space.element_location(&c.window) else { return };
        let loc = (loc - c.window.geometry().loc - og.loc).to_physical_precise_round(scale);
        let els: Vec<WaylandSurfaceRenderElement<_>> = c.window.render_elements(r, loc, scale.into(), 1.0);
        e.extend(els.into_iter().map(Elem::Surface));
        if c.bw() > 0 {
            for (b, rect) in c.borders.iter().zip(border_rects(c.geom, c.bw())) {
                let loc = (rect.loc - og.loc).to_physical_precise_round(scale);
                e.push(Elem::Solid(SolidColorRenderElement::from_buffer(b, loc, scale, 1.0, Kind::Unspecified)));
            }
        }
    };

    layers(&mut e, r, Layer::Overlay);
    for &ci in clients.iter().filter(|&&ci| s.clients[ci].fullscreen) {
        window(&mut e, r, ci);
        if let Some(mi) = mi.filter(|&mi| s.clients[ci].mon == s.mons[mi].output) {
            let bg = &s.mons[mi].fullscreen_bg;
            e.push(Elem::Solid(SolidColorRenderElement::from_buffer(bg, (0, 0), scale, 1.0, Kind::Unspecified)));
        }
    }
    layers(&mut e, r, Layer::Top);
    for &ci in clients.iter().filter(|&&ci| s.clients[ci].floating && !s.clients[ci].fullscreen) {
        window(&mut e, r, ci);
    }
    for &ci in clients.iter().filter(|&&ci| !s.clients[ci].floating && !s.clients[ci].fullscreen) {
        window(&mut e, r, ci);
    }
    layers(&mut e, r, Layer::Bottom);
    layers(&mut e, r, Layer::Background);
    (e, ncursor, ROOTCOLOR)
}

/// After a frame: serve pending screencopies and send frame callbacks.
pub fn post_render(s: &mut State, r: &mut GlesRenderer, output: &Output, elems: &[Elem], ncursor: usize, clear: [f32; 4]) {
    let (mine, rest) = std::mem::take(&mut s.captures).into_iter().partition(|(o, _, _)| o == output);
    s.captures = rest;
    for (_, frame, cursor) in mine {
        let els = if cursor { elems } else { &elems[ncursor..] };
        match capture(r, output, els, clear, &frame.buffer()) {
            Ok(()) => frame.success(Transform::Normal, None, Clock::<Monotonic>::new().now()),
            Err(e) => {
                tracing::warn!("screencopy failed: {e}");
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    let now = s.start.elapsed();
    let o = output.clone();
    let og = s.space.output_geometry(output).unwrap_or_default();
    // preferred buffer scale for HiDPI outputs
    let (scale, tr) = (output.current_scale(), output.current_transform());
    let set_scale = |surface: &WlSurface, data: &SurfaceData| {
        send_surface_state(surface, data, scale.integer_scale(), tr);
        with_fractional_scale(data, |f| f.set_preferred_scale(scale.fractional_scale()));
    };
    for c in s.clients.iter().filter(|c| c.geom.overlaps(og)) {
        c.window.with_surfaces(set_scale);
        c.window.send_frame(output, now, None, |_, _| Some(o.clone()));
    }
    for l in layer_map_for_output(output).layers() {
        l.with_surfaces(set_scale);
        l.send_frame(output, now, None, |_, _| Some(o.clone()));
    }
    for (_, l) in s.lock_surfaces.iter().filter(|(lo, _)| lo == output) {
        send_frames_surface_tree(l.wl_surface(), output, now, None, |_, _| Some(o.clone()));
    }
    if let CursorImageStatus::Surface(surf) = &s.cursor_status {
        send_frames_surface_tree(surf, output, now, None, |_, _| Some(o.clone()));
    }
}

fn capture(r: &mut GlesRenderer, o: &Output, els: &[Elem], clear: [f32; 4], buffer: &WlBuffer) -> Result<(), String> {
    let mode = o.current_mode().ok_or("no mode")?;
    let size = o.current_transform().transform_size(mode.size);
    let bsize = size.to_logical(1).to_buffer(1, Transform::Normal);
    let mut tex: GlesTexture = r.create_buffer(Fourcc::Argb8888, bsize).map_err(|e| e.to_string())?;
    let mut fb = r.bind(&mut tex).map_err(|e| e.to_string())?;
    OutputDamageTracker::new(size, o.current_scale().fractional_scale(), Transform::Normal)
        .render_output(r, &mut fb, 0, els, clear)
        .map_err(|e| format!("{e:?}"))?;
    let mapping = r.copy_framebuffer(&fb, Rectangle::from_size(bsize), Fourcc::Argb8888).map_err(|e| e.to_string())?;
    let px = r.map_texture(&mapping).map_err(|e| e.to_string())?;
    let (w, h) = (bsize.w as usize, bsize.h as usize);
    shm::with_buffer_contents_mut(buffer, |ptr, len, d| {
        let (off, stride) = (d.offset as usize, d.stride as usize);
        if d.width as usize != w || d.height as usize != h || off + stride * (h - 1) + w * 4 > len {
            return Err("buffer does not match constraints".to_string());
        }
        for y in 0..h {
            let row = &px[y * w * 4..(y + 1) * w * 4];
            unsafe { std::ptr::copy_nonoverlapping(row.as_ptr(), ptr.add(off + y * stride), w * 4) };
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}
