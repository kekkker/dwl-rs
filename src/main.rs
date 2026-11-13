mod config;
mod gamma;
mod input;
mod render;
mod state;
mod udev;
mod winit;
mod wm;

use std::{path::Path, process::Stdio};

use smithay::reexports::{
    calloop::EventLoop,
    rustix::process::{Pid, Signal, kill_process},
    wayland_server::Display,
};

use crate::{config::*, state::State};

fn main() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let mut startup = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-s" if startup.is_none() => startup = args.next(),
            "-v" => return println!("dwl-rs {}", env!("CARGO_PKG_VERSION")),
            _ => {
                eprintln!("Usage: dwl-rs [-v] [-s startup command]");
                std::process::exit(1);
            }
        }
    }

    let mut event_loop = EventLoop::try_new().expect("failed to create event loop");
    let display = Display::new().expect("failed to create display");
    let mut state = State::new(&mut event_loop, display, Box::new(std::io::sink()));

    // like wlroots: nested inside another session if there is one, else take the TTY
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    let res = if nested { winit::init(&mut event_loop, &mut state) } else { udev::init(&mut event_loop, &mut state) };
    if let Err(e) = res {
        eprintln!("failed to start backend: {e}");
        std::process::exit(1);
    }

    tracing::info!("listening on WAYLAND_DISPLAY={}", state.socket.to_string_lossy());
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &state.socket);
        std::env::remove_var("DISPLAY");
    }
    let mut pids = vec![];
    if XWAYLAND {
        let n = (0..64).find(|n| {
            !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists() && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
        });
        if let Some(pid) = n.and_then(|n| wm::spawn(&["xwayland-satellite", &format!(":{n}")])) {
            unsafe { std::env::set_var("DISPLAY", format!(":{}", n.unwrap())) };
            tracing::info!("X11 apps on DISPLAY=:{}", n.unwrap());
            pids.push(pid);
        }
    }
    for cmd in AUTOSTART {
        pids.extend(wm::spawn(cmd));
    }
    // dwl pipes its status lines into the startup command's stdin
    state.status = Box::new(std::io::stdout());
    if let Some(cmd) = startup {
        match wm::command(&["/bin/sh", "-c", &cmd]).stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                state.status = Box::new(child.stdin.take().unwrap());
                pids.push(child.id());
                std::thread::spawn(move || child.wait());
            }
            Err(e) => eprintln!("failed to run startup command: {e}"),
        }
    }
    state.last_status.clear();
    state.printstatus();

    let signal = event_loop.get_signal();
    event_loop
        .run(None, &mut state, |s| {
            if !s.running {
                return signal.stop();
            }
            s.space.refresh();
            s.popups.cleanup();
            s.image_copy_capture_state.cleanup();
            udev::render_pending(s);
            let _ = s.dh.flush_clients();
        })
        .expect("event loop failed");

    for pid in pids {
        if let Some(pid) = Pid::from_raw(pid as i32) {
            let _ = kill_process(pid, Signal::TERM);
        }
    }
}
