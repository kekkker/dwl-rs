// Default config. The first build copies this to src/config.rs; edit that and rebuild, like dwm.
use crate::wm::{Action::*, Key, Layout, MonitorRule, Rule, monocle, tile};
use smithay::input::keyboard::keysyms::*;

const fn rgba(hex: u32) -> [f32; 4] {
    [
        ((hex >> 24) & 0xff) as f32 / 255.0,
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    ]
}

// appearance
pub const SLOPPYFOCUS: bool = true; // focus follows mouse
pub const BORDERPX: i32 = 1;
pub const ROOTCOLOR: [f32; 4] = rgba(0x222222ff);
pub const BORDERCOLOR: [f32; 4] = rgba(0x444444ff);
pub const FOCUSCOLOR: [f32; 4] = rgba(0x005577ff);
pub const URGENTCOLOR: [f32; 4] = rgba(0xff0000ff);
pub const FULLSCREEN_BG: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

pub const TAGCOUNT: u32 = 9;

// commands started once at startup, killed on quit
pub const AUTOSTART: &[&[&str]] = &[
    // &["wbg", "/path/to/your/image"],
];

// X11 apps: run xwayland-satellite if it is installed
pub const XWAYLAND: bool = true;

pub const RULES: &[Rule] = &[
    // examples:
    Rule { app_id: Some("Gimp_EXAMPLE"), title: None, tags: 0, floating: true, monitor: -1 },
    Rule { app_id: Some("firefox_EXAMPLE"), title: None, tags: 1 << 8, floating: false, monitor: -1 },
];

pub const LAYOUTS: &[Layout] = &[
    Layout { symbol: "[]=", arrange: Some(tile) },
    Layout { symbol: "><>", arrange: None }, // floating
    Layout { symbol: "[M]", arrange: Some(monocle) },
];

// name None is the fallback rule; outputs are placed left to right
pub const MONRULES: &[MonitorRule] = &[
    // MonitorRule { name: Some("eDP-1"), mfact: 0.5, nmaster: 1, scale: 2.0, layout: 0 },
    MonitorRule { name: None, mfact: 0.55, nmaster: 1, scale: 1.0, layout: 0 },
];

// keyboard
pub const XKB_LAYOUT: &str = ""; // e.g. "us,ru"
pub const XKB_OPTIONS: Option<&str> = None; // e.g. Some("ctrl:nocaps")
pub const REPEAT_RATE: i32 = 25;
pub const REPEAT_DELAY: i32 = 600;

// trackpad
pub const TAP_TO_CLICK: bool = true;
pub const TAP_AND_DRAG: bool = true;
pub const DRAG_LOCK: bool = true;
pub const NATURAL_SCROLLING: bool = false;
pub const DISABLE_WHILE_TYPING: bool = true;
pub const LEFT_HANDED: bool = false;
pub const MIDDLE_BUTTON_EMULATION: bool = false;
pub const ACCEL_SPEED: f64 = 0.0;
pub const ACCEL_FLAT: bool = false; // false = adaptive

// modifiers, same values as wlroots
pub const SHIFT: u32 = 1;
pub const CTRL: u32 = 4;
pub const ALT: u32 = 8;
pub const LOGO: u32 = 64;
pub const MODKEY: u32 = ALT; // LOGO for the Super key

pub fn keys() -> Vec<Key> {
    let mut keys = vec![
        Key(MODKEY, KEY_p, Spawn(&["wmenu-run"])),
        Key(MODKEY | SHIFT, KEY_Return, Spawn(&["foot"])),
        Key(MODKEY, KEY_j, FocusStack(1)),
        Key(MODKEY, KEY_k, FocusStack(-1)),
        Key(MODKEY, KEY_i, IncNmaster(1)),
        Key(MODKEY, KEY_d, IncNmaster(-1)),
        Key(MODKEY, KEY_h, SetMfact(-0.05)),
        Key(MODKEY, KEY_l, SetMfact(0.05)),
        Key(MODKEY, KEY_Return, Zoom),
        Key(MODKEY, KEY_Tab, View(0)),
        Key(MODKEY | SHIFT, KEY_C, KillClient),
        Key(MODKEY, KEY_t, SetLayout(Some(0))),
        Key(MODKEY, KEY_f, SetLayout(Some(1))),
        Key(MODKEY, KEY_m, SetLayout(Some(2))),
        Key(MODKEY, KEY_space, SetLayout(None)),
        Key(MODKEY | SHIFT, KEY_space, ToggleFloating),
        Key(MODKEY, KEY_e, ToggleFullscreen),
        Key(MODKEY | SHIFT, KEY_P, ToggleSticky),
        Key(MODKEY, KEY_0, View(!0)),
        Key(MODKEY | SHIFT, KEY_parenright, Tag(!0)),
        Key(MODKEY, KEY_comma, FocusMon(-1)),
        Key(MODKEY, KEY_period, FocusMon(1)),
        Key(MODKEY | SHIFT, KEY_less, TagMon(-1)),
        Key(MODKEY | SHIFT, KEY_greater, TagMon(1)),
        Key(MODKEY | SHIFT, KEY_Q, Quit),
        Key(CTRL | ALT, KEY_Terminate_Server, Quit),
    ];
    let tagkeys = [
        (KEY_1, KEY_exclam), (KEY_2, KEY_at), (KEY_3, KEY_numbersign),
        (KEY_4, KEY_dollar), (KEY_5, KEY_percent), (KEY_6, KEY_asciicircum),
        (KEY_7, KEY_ampersand), (KEY_8, KEY_asterisk), (KEY_9, KEY_parenleft),
    ];
    for (i, (key, skey)) in tagkeys.into_iter().enumerate() {
        keys.extend([
            Key(MODKEY, key, View(1 << i)),
            Key(MODKEY | CTRL, key, ToggleView(1 << i)),
            Key(MODKEY | SHIFT, skey, Tag(1 << i)),
            Key(MODKEY | CTRL | SHIFT, skey, ToggleTag(1 << i)),
        ]);
    }
    // Ctrl-Alt-Fn switches VT, don't remove unless you know what a VT is
    for n in 1..=12 {
        keys.push(Key(CTRL | ALT, KEY_XF86Switch_VT_1 + n - 1, Chvt(n as i32)));
    }
    keys
}

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

pub const BUTTONS: &[(u32, u32, crate::wm::Action)] = &[
    (MODKEY, BTN_LEFT, MoveResize(false)),
    (MODKEY, BTN_MIDDLE, ToggleFloating),
    (MODKEY, BTN_RIGHT, MoveResize(true)),
];
