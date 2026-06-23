//! Minimal `zwp_virtual_keyboard_v1` client for the paste chord (Ctrl+Shift+V) and Enter.
//!
//! We upload one fixed keymap at startup and never touch it again — that is the whole fix for the
//! intermittent bare-"v" paste. Every keymap upload makes the compositor re-activate the layout and
//! clear the depressed modifiers, so a virtual keyboard that re-uploads the keymap on every keypress
//! (as wrtype did) races that reset against the key event and Ctrl+Shift sometimes drops, leaking a
//! bare "v". With a fixed keymap the modifier keys we hold are exactly what's live when "v" lands.
//!
//! Modifiers go through the explicit `modifiers()` request (set to Ctrl+Shift, then cleared to 0),
//! not by pressing modifier keys: wlroots doesn't reliably derive modifier state from a virtual
//! keyboard's modifier-key presses, and a press whose release is missed would leave a modifier
//! stuck. The keys we do press (V, Enter) use the *standard* evdev keycodes, which read identically
//! on the user's real (AZERTY) seat keymap — unlike wrtype's sequentially-allocated codes, which on
//! AZERTY collided with `twosuperior` and toggled the ² dropdown.

use anyhow::Result;
use std::io::Write;
use std::os::fd::AsFd;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_keyboard, wl_registry::WlRegistry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

// Linux evdev keycodes — what zwp_virtual_keyboard.key expects (the compositor adds 8 for XKB).
const EV_V: u32 = 47;
const EV_ENTER: u32 = 28;

// Depressed-modifier mask for our keymap's real modifiers: Shift = bit 0 (1), Control = bit 2 (4).
const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 4;

// Self-contained keymap: defines exactly the keys we press at their standard XKB keycodes (evdev+8),
// with a modifier_map so holding Control_L/Shift_L sets the real modifiers. Types and compat come
// from the shared "complete" includes the compositor's xkbcommon resolves (same as wrtype used).
const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "(dictate)" {
  minimum = 8;
  maximum = 255;
  <LCTL> = 37;
  <RTRN> = 36;
  <LFSH> = 50;
  <AB05> = 55;
};
xkb_types "(dictate)" { include "complete" };
xkb_compatibility "(dictate)" { include "complete" };
xkb_symbols "(dictate)" {
  key <LCTL> { [ Control_L ] };
  key <RTRN> { [ Return ] };
  key <LFSH> { [ Shift_L ] };
  key <AB05> { [ v, V ] };
  modifier_map Shift { <LFSH> };
  modifier_map Control { <LCTL> };
};
};
"#;

struct State;
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

pub struct KeyInject {
    conn: Connection,
    _queue: EventQueue<State>,
    keyboard: ZwpVirtualKeyboardV1,
    time: u32,
}

impl KeyInject {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| anyhow::anyhow!("zwp_virtual_keyboard_manager_v1 not available: {e}"))?;
        let seat: WlSeat = globals
            .bind(&qh, 1..=WlSeat::interface().version, ())
            .map_err(|e| anyhow::anyhow!("wl_seat not available: {e}"))?;
        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

        // Upload the fixed keymap exactly once (null-terminated, per XKB).
        let mut f = tempfile::tempfile()?;
        f.write_all(KEYMAP.as_bytes())?;
        f.write_all(b"\0")?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1.into(),
            f.as_fd(),
            KEYMAP.len() as u32 + 1,
        );
        conn.roundtrip()?;

        Ok(Self { conn, _queue: queue, keyboard, time: 0 })
    }

    fn key(&mut self, evdev: u32, pressed: bool) {
        self.time = self.time.wrapping_add(1);
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        self.keyboard.key(self.time, evdev, state.into());
    }

    /// Ctrl+Shift+V. Modifiers are set with the explicit `modifiers()` request and cleared right
    /// after — no modifier key is held, so none can stick — and because the keymap never re-uploads,
    /// the modifiers are still live when V lands, so it pastes the clipboard instead of a bare "v".
    pub fn paste(&mut self) {
        self.keyboard.modifiers(MOD_CTRL | MOD_SHIFT, 0, 0, 0);
        let _ = self.conn.roundtrip();
        self.key(EV_V, true);
        self.key(EV_V, false);
        self.keyboard.modifiers(0, 0, 0, 0);
        let _ = self.conn.roundtrip();
    }

    pub fn enter(&mut self) {
        self.key(EV_ENTER, true);
        self.key(EV_ENTER, false);
        let _ = self.conn.roundtrip();
    }
}
