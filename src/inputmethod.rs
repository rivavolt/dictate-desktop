//! Wayland `zwp_input_method_v2` client.
//!
//! Lets the daemon commit transcribed text straight into the focused text-input field via
//! `commit_string` + `commit`, instead of synthesizing keystrokes with wrtype. This bypasses the
//! keyboard entirely, so it produces correct output regardless of the user's keyboard layout and
//! never races with held modifiers.
//!
//! The catch is that only native Wayland apps implementing text-input-v3 participate. XWayland
//! apps, and Wayland apps that don't speak text-input-v3, never focus an input-method, so the
//! compositor never sends us `activate`. `is_active()` exposes exactly that signal: when it returns
//! false there is no field listening and a `commit()` would be silently dropped, so the caller
//! should fall back to keystroke injection.
//!
//! Architecture mirrors `overlay.rs`: a dedicated thread owns the Wayland connection and runs a
//! calloop event loop; the rest of the program holds a cheap `Handle` and talks to the thread over
//! a `calloop::channel`. Shared activation/availability/serial state lives in atomics so `Handle`
//! accessors don't need a round-trip.

use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};

enum Command {
    Commit(String),
}

/// Cloneable handle to the input-method thread.
///
/// The activation/availability/serial state it reports is updated by the Wayland thread and read
/// here lock-free, so the values are eventually-consistent snapshots — fine for the "should I use
/// the input method or fall back to keystrokes?" decision the caller makes right before committing.
#[derive(Clone)]
pub struct Handle {
    tx: calloop::channel::Sender<Command>,
    shared: Arc<Shared>,
}

struct Shared {
    /// Whether a text-input field is currently focused (between `activate` and `deactivate`,
    /// applied on `done`). When false, a commit goes nowhere.
    active: AtomicBool,
    /// True until `unavailable` fires (another input method already owns the seat) or the thread
    /// dies. While false, this input method is inert and commits do nothing.
    available: AtomicBool,
    /// Number of `done` events received. This is the value the protocol requires us to pass to the
    /// `commit` request (it must equal the count of `done` events the object has issued).
    serial: AtomicU32,
}

impl Handle {
    /// Commit `text` to the focused text-input via `commit_string(text)` + `commit(serial)`.
    ///
    /// This is fire-and-forget: it queues the work onto the Wayland thread and returns immediately.
    /// If no field is focused (`is_active()` is false) or the input method is unavailable, the
    /// compositor drops the commit, so callers should gate on `is_active()` and otherwise fall back
    /// to keystroke injection. Empty strings are skipped.
    pub fn commit(&self, text: String) {
        if text.is_empty() {
            return;
        }
        let _ = self.tx.send(Command::Commit(text));
    }

    /// Whether a text-input field is currently focused and listening.
    ///
    /// False for XWayland apps and any app without text-input-v3 support (they never trigger
    /// `activate`), and false whenever focus is on a non-text surface. Use this to decide between
    /// committing via the input method and falling back to synthetic keystrokes.
    pub fn is_active(&self) -> bool {
        self.shared.available.load(Ordering::Acquire)
            && self.shared.active.load(Ordering::Acquire)
    }

    /// False if the compositor sent `unavailable` (another input method already bound this seat) or
    /// the input-method thread failed to start / exited. When false, this input method can never
    /// become active and `commit` is a no-op.
    pub fn available(&self) -> bool {
        self.shared.available.load(Ordering::Acquire)
    }
}

/// Start the input-method thread, connect to Wayland, bind the manager + seat, create the
/// `zwp_input_method_v2`, and run its event loop. Returns immediately with a `Handle`.
///
/// Returns `Err` only if the thread itself can't be spawned. A missing
/// `zwp_input_method_manager_v2` (compositor without input-method-v2 support) or any later Wayland
/// failure is reported via tracing and flips `available()` to false rather than failing the spawn,
/// so the caller can always fall back to keystrokes.
pub fn spawn() -> Result<Handle> {
    let (tx, rx) = calloop::channel::channel::<Command>();
    let shared = Arc::new(Shared {
        active: AtomicBool::new(false),
        available: AtomicBool::new(true),
        serial: AtomicU32::new(0),
    });

    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("inputmethod".into())
        .spawn(move || {
            if let Err(e) = run(rx, &thread_shared) {
                tracing::error!("inputmethod: {e}");
            }
            // Whatever the exit reason, no input method is live anymore.
            thread_shared.available.store(false, Ordering::Release);
            thread_shared.active.store(false, Ordering::Release);
        })?;

    Ok(Handle { tx, shared })
}

fn run(cmd_rx: calloop::channel::Channel<Command>, shared: &Arc<Shared>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    // Bind the input-method manager. If the compositor doesn't advertise it, there's nothing to do.
    let manager: ZwpInputMethodManagerV2 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| anyhow::anyhow!("zwp_input_method_manager_v2 not available: {e}"))?;

    // Bind a seat. `get_input_method` needs one, and there is at most one input method per seat.
    let seat: WlSeat = globals
        .bind(&qh, 1..=WlSeat::interface().version, ())
        .map_err(|e| anyhow::anyhow!("wl_seat not available: {e}"))?;

    let input_method = manager.get_input_method(&seat, &qh, ());

    let mut state = State {
        shared: shared.clone(),
        input_method: input_method.clone(),
        // Pending activation, applied to `shared.active` on the next `done`.
        pending_active: false,
        exit: false,
    };

    let mut event_loop = calloop::EventLoop::<State>::try_new()?;
    let loop_handle = event_loop.handle();

    let wayland_source = calloop_wayland_source::WaylandSource::new(conn, event_queue);
    loop_handle
        .insert_source(wayland_source, |_, queue, state| queue.dispatch_pending(state))
        .map_err(|e| anyhow::anyhow!("wayland source: {e}"))?;

    loop_handle
        .insert_source(cmd_rx, move |event, _, state| {
            if let calloop::channel::Event::Msg(Command::Commit(text)) = event {
                state.do_commit(text);
            }
        })
        .map_err(|e| anyhow::anyhow!("cmd channel: {e}"))?;

    while !state.exit {
        event_loop.dispatch(std::time::Duration::from_secs(60), &mut state)?;
    }

    Ok(())
}

struct State {
    shared: Arc<Shared>,
    input_method: ZwpInputMethodV2,
    pending_active: bool,
    exit: bool,
}

impl State {
    fn do_commit(&self, text: String) {
        // Honor the live activation/availability snapshot: committing into an unfocused or inert
        // input method does nothing useful and (for an inert one) is explicitly ignored by the
        // compositor.
        if !self.shared.available.load(Ordering::Acquire)
            || !self.shared.active.load(Ordering::Acquire)
        {
            tracing::debug!("inputmethod: commit dropped (no active text-input)");
            return;
        }
        let serial = self.shared.serial.load(Ordering::Acquire);
        tracing::info!("inputmethod: committing {} chars (serial {serial})", text.len());
        self.input_method.commit_string(text);
        self.input_method.commit(serial);
    }
}

impl Dispatch<ZwpInputMethodV2, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_input_method_v2::Event;
        match event {
            // activate/deactivate are double-buffered: they set the pending state that the next
            // `done` applies. They also reset preedit/commit state, but we keep none between
            // commits, so there's nothing extra to clear here.
            Event::Activate => {
                tracing::info!("inputmethod: activate");
                state.pending_active = true;
            }
            Event::Deactivate => {
                tracing::info!("inputmethod: deactivate");
                state.pending_active = false;
            }
            // We don't use surrounding text or content hints — committing a finished string doesn't
            // depend on them.
            Event::SurroundingText { .. } => {}
            Event::TextChangeCause { .. } => {}
            Event::ContentType { .. } => {}
            // Atomically apply pending state and bump the serial. The protocol defines the serial we
            // must echo back in `commit` as "the number of done events already issued", so it is a
            // running count, not a value carried by any event.
            Event::Done => {
                state
                    .shared
                    .active
                    .store(state.pending_active, Ordering::Release);
                state.shared.serial.fetch_add(1, Ordering::AcqRel);
            }
            // Another input method already owns the seat (or the seat went away). The object is now
            // inert; mark unavailable and stop. Per the protocol the object should be destroyed
            // after handling deactivation — dropping the proxy on thread exit does that.
            Event::Unavailable => {
                tracing::warn!("inputmethod: unavailable (another input method bound the seat)");
                state.shared.available.store(false, Ordering::Release);
                state.shared.active.store(false, Ordering::Release);
                state.exit = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpInputMethodManagerV2, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpInputMethodManagerV2,
        _: <ZwpInputMethodManagerV2 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // zwp_input_method_manager_v2 has no events.
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We bind everything we need up front from the initial global list; dynamic
        // global add/remove events don't affect committing text, so ignore them.
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // We only need the seat as a handle for get_input_method; its capability/name events are
        // irrelevant to committing text.
    }
}
