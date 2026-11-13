# dwl-rs

[dwl](https://codeberg.org/dwl/dwl) rewritten in Rust on [Smithay](https://github.com/Smithay/smithay).
Same tags, layouts, keybindings, and status output as dwl, in about 2,700 lines of Rust.
Configuration is done by editing a source file and rebuilding, like dwm.

## Features

- dwm-style tags, `tile`, `monocle` and floating layouts, master/stack with `mfact` and `nmaster`
- Multiple monitors, each with its own tags and layout
- dwl's `-s` status output on stdin of the startup command, so bars like [dwlb](https://github.com/kolunmi/dwlb) work unchanged
- Window rules, sticky windows, autostart, centered floating windows (the dwl patches `autostart`, `sticky`, `always-center`)
- Interactive move and resize with the mouse, sloppy focus
- Server-side borders, fullscreen with a background
- Picks the fastest refresh rate at your monitor's native resolution
- X11 apps through [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite), started automatically when installed
- Runs on a TTY (DRM/KMS + libinput via libseat) or nested in another Wayland or X11 session

Supported protocols include xdg-shell, xdg-decoration, layer-shell, ext-session-lock (swaylock), ext-idle-notify and idle-inhibit (swayidle), ext-image-copy-capture (grim, xdg-desktop-portal-wlr), wlr-gamma-control (gammastep, wlsunset), wlr-data-control and primary selection (clipboard managers), pointer constraints and relative pointer (games), virtual keyboard and pointer (wtype, wvkbd), cursor-shape, fractional-scale, viewporter, xdg-activation and linux-dmabuf.

## Not (yet) supported

- Keybindings do not repeat while held
- wlr-output-management (wlr-randr, kanshi) and wlr-output-power-management
- The older wlr-screencopy protocol, and dmabuf screen capture
- Rendering on more than one GPU: outputs must be connected to the primary GPU
- Touch, tablets and gestures

## Building

You need Rust 1.88 or newer and the development files for wayland, libinput, libseat, libudev, mesa (EGL and GBM), libxkbcommon, libdrm and libdisplay-info.
On Arch Linux:

```sh
pacman -S rust wayland libinput seatd libxkbcommon mesa libdisplay-info
```

Then:

```sh
cargo build --release
```

The binary is `target/release/dwl-rs`.

## Running

From a TTY:

```sh
dwl-rs -s 'dwlb -font "monospace:size=10"'
```

Inside an existing Wayland or X11 session it opens a nested window, which is handy for trying out config changes.
Your user needs seat access (seatd or logind) to run on a TTY.
Set `RUST_LOG=dwl_rs=info` to see the Wayland socket, X display and chosen monitor modes.

## Configuration

The first build copies `src/config.def.rs` to `src/config.rs`.
Edit `src/config.rs` and rebuild.
It mirrors dwl's `config.h`: colors, tags, rules, layouts, monitor rules, keyboard and trackpad settings, keybindings and mouse buttons.
`src/config.rs` is not tracked by git, so your settings survive pulls.

The default keybindings are dwl's, with Alt as the modifier.
Set `MODKEY` to `LOGO` to use the Super key.

## Status output

With `-s`, dwl-rs runs the given command through `/bin/sh` and writes dwl's status lines to its stdin:

```
DP-1 title foot
DP-1 appid foot
DP-1 fullscreen 0
DP-1 floating 0
DP-1 selmon 1
DP-1 tags 3 1 1 0
DP-1 layout []=
```

`tags` lists the occupied, selected, focused-client and urgent tag masks.
Without `-s` the lines go to stdout.

## License

GPL-3.0-or-later, like dwl, which this is a port of.
Parts derived from dwm are under the MIT/X license in `LICENSE.dwm`.
