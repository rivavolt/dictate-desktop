use anyhow::Result;
use femtovg::{Baseline, Canvas, Color, FontId, Paint, Path};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use khronos_egl as egl;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_surface},
    Connection, Proxy, QueueHandle,
};

const PILL_SIZE: u32 = 56;
const PADDING_X: f32 = 16.0;
const PADDING_Y: f32 = 6.0;
const OVERLAY_WIDTH_FRAC: f64 = 0.618;
const CHAR_WIDTH_RATIO: f32 = 0.47;
const CORNER_RADIUS: f32 = 16.0;
const SHRINK_DURATION_MS: f32 = 150.0;
const FADE_TAU: f32 = 40.0;
const WIDTH_TAU: f32 = 50.0;
const HEIGHT_TAU: f32 = 50.0;
const SHADOW_FEATHER: f32 = 28.0;
const SHADOW_OFFSET_Y: f32 = 6.0;
const SHADOW_PAD: u32 = 26;
const SHADOW_PAD_BOT: u32 = 8;
const PILL_ICON_PAD: f32 = 0.30; // fraction of pill height reserved as padding on each side
const CHUNK_TAU: f32 = 80.0;
const FINALIZE_TAU: f32 = 60.0;
const CIRCLE_GAP: f32 = 12.0;          // gap between status circles
const RECORD_PULSE_MS: f32 = 1400.0;   // recording circle pulse period
const SPINNER_MS: f32 = 1100.0;        // processing spinner rotation period
const INDICATOR_FADE_MS: f32 = 180.0;  // circle fade-in / done fade-out
const DONE_HOLD_S: f32 = 0.45;         // hold the done flash + checkmark before fading out

#[derive(Clone, Copy, PartialEq)]
pub enum DoneKind {
    Delivered,
    Copied,
    Dismissed,
    Failed,
}

#[derive(Clone, Copy, PartialEq)]
enum IndicatorKind {
    Recording,
    Processing,
    Done(DoneKind),
}

/// One in-flight utterance's status circle. `phase` drives the pulse/spin, `fade` is 0→1 on
/// appearance and 1→0 once `Done` so the circle animates out before being dropped.
#[derive(Clone)]
struct Indicator {
    id: u64,
    kind: IndicatorKind,
    phase: f32,
    eta_remaining: f32,
    eta_set: bool,
    fade: f32,
}

pub enum Command {
    /// Begin a recording indicator for utterance `id`.
    Start(u64),
    /// Utterance `id` is now awaiting its transcript; the f32 is the estimated seconds until it
    /// lands (0 = unknown → plain spinner, no countdown).
    Processing(u64, f32),
    /// Utterance `id` finished — its indicator shows an outcome (checkmark/clipboard/✕) then fades.
    Done(u64, DoneKind),
    SetText(String),
    SetPending(String),
    /// Briefly show a one-line message (paste/copy feedback) for ~2.5s, regardless of mode.
    Toast(String),
    Correcting,
    SetInfo(String, String),
    SetFont(String),
    /// Status-only: show the indicators/animations but never expand into the text panel.
    SetStatusOnly(bool),
}

#[derive(Clone)]
pub struct Handle {
    tx: calloop::channel::Sender<Command>,
    audio_level: Arc<AtomicU32>,
}

impl Handle {
    pub fn start(&self, id: u64) {
        let _ = self.tx.send(Command::Start(id));
    }

    pub fn set_text(&self, text: String) {
        let _ = self.tx.send(Command::SetText(text));
    }

    pub fn set_pending(&self, text: String) {
        let _ = self.tx.send(Command::SetPending(text));
    }

    pub fn processing(&self, id: u64, eta_secs: f32) {
        let _ = self.tx.send(Command::Processing(id, eta_secs));
    }

    pub fn done(&self, id: u64, kind: DoneKind) {
        let _ = self.tx.send(Command::Done(id, kind));
    }

    pub fn toast(&self, msg: String) {
        let _ = self.tx.send(Command::Toast(msg));
    }

    pub fn correcting(&self) {
        let _ = self.tx.send(Command::Correcting);
    }

    pub fn set_info(&self, mode: String, lang: String) {
        let _ = self.tx.send(Command::SetInfo(mode, lang));
    }

    pub fn set_font(&self, name: String) {
        let _ = self.tx.send(Command::SetFont(name));
    }

    pub fn set_status_only(&self, status_only: bool) {
        let _ = self.tx.send(Command::SetStatusOnly(status_only));
    }

    pub fn audio_level(&self) -> &Arc<AtomicU32> {
        &self.audio_level
    }
}

pub fn spawn(font: String) -> Result<Handle> {
    let (tx, rx) = calloop::channel::channel::<Command>();
    let audio_level = Arc::new(AtomicU32::new(0));
    let level_clone = audio_level.clone();
    let handle = Handle { tx, audio_level };

    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            if let Err(e) = run(rx, &font, level_clone) {
                tracing::error!("overlay: {e}");
            }
        })?;

    Ok(handle)
}

fn find_font_path(font_name: &str) -> Result<String> {
    let output = std::process::Command::new("fc-match")
        .args(["--format", "%{file}", font_name])
        .output()?;
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn run(cmd_rx: calloop::channel::Channel<Command>, font_name: &str, audio_level: Arc<AtomicU32>) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("dictate-desktop-overlay"),
        None,
    );

    layer.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);

    // Empty input region: all pointer events pass through to surfaces below
    let region = compositor.wl_compositor().create_region(&qh, ());
    layer.wl_surface().set_input_region(Some(&region));

    layer.commit();

    // EGL setup
    let egl_lib = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required()? };

    let wl_display = conn.backend().display_ptr() as *mut std::ffi::c_void;
    let egl_display = unsafe {
        egl_lib.get_display(wl_display as egl::NativeDisplayType)
    }.ok_or_else(|| anyhow::anyhow!("EGL: no display"))?;
    egl_lib.initialize(egl_display)?;

    let config_attribs = [
        egl::RED_SIZE, 8,
        egl::GREEN_SIZE, 8,
        egl::BLUE_SIZE, 8,
        egl::ALPHA_SIZE, 8,
        egl::SURFACE_TYPE, egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT,
        egl::NONE,
    ];
    let egl_config = egl_lib.choose_first_config(egl_display, &config_attribs)?
        .ok_or_else(|| anyhow::anyhow!("EGL: no config"))?;

    let context_attribs = [
        egl::CONTEXT_CLIENT_VERSION, 2,
        egl::NONE,
    ];
    let egl_context = egl_lib.create_context(egl_display, egl_config, None, &context_attribs)?;

    let wl_egl_surface = wayland_egl::WlEglSurface::new(
        layer.wl_surface().id(),
        PILL_SIZE as i32,
        (PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT) as i32,
    )?;

    let egl_surface = unsafe {
        egl_lib.create_window_surface(egl_display, egl_config, wl_egl_surface.ptr() as egl::NativeWindowType, None)?
    };

    egl_lib.make_current(egl_display, Some(egl_surface), Some(egl_surface), Some(egl_context))?;
    // Disable vsync blocking — calloop timer handles frame pacing
    egl_lib.swap_interval(egl_display, 0)?;

    let renderer = unsafe {
        femtovg::renderer::OpenGl::new_from_function(|s| {
            egl_lib.get_proc_address(s).map(|f| f as *const _).unwrap_or(std::ptr::null())
        })?
    };
    let mut canvas = Canvas::new(renderer)?;

    // Load font
    let font_path = find_font_path(font_name)?;
    let font_id = canvas.add_font(&font_path)?;
    tracing::debug!("overlay: loaded font {font_name} from {font_path}");

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        egl_lib,
        egl_display,
        egl_surface,
        wl_egl_surface,
        canvas,
        font_id,
        text: String::new(),
        pending: String::new(),
        mode: String::new(),
        lang: String::new(),
        wrapped_lines: Vec::new(),
        wrapped_dirty: true,
        indicators: Vec::new(),
        correcting: false,
        status_only: false,
        shrink_t: 0.0,
        shrink_target: 0.0,
        toast_remaining: 0.0,
        content_pw: 0.0,
        render_w: 0.0,
        anim_phase: 0.0,
        audio_level,
        audio_peak: 0.05,
        smooth_audio: 0.0,
        bar_levels: [0.0; 4],
        fade_alpha: 0.0,
        fade_target: 0.0,
        visible: false,
        configured: false,
        screen_width: 0,
        width: 0,
        height: (PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT) as f32,
        target_height: (PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT) as f32,
        max_height: 400,
        font_size: 16.0,
        line_height: 24.0,
        scale: 1,
        frame_ms: 16,
        last_tick: std::time::Instant::now(),
        committed_layer_h: PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT,
        last_egl_w: 0,
        last_egl_h: 0,
        correct_fade: 1.0,
        reveal_len: 0,
        chunk_fade: 1.0,
        color_split: 0,
        finalize_fade: 1.0,
        exit: false,
    };

    let mut event_loop = calloop::EventLoop::<State>::try_new()?;
    let loop_handle = event_loop.handle();

    let wayland_source = calloop_wayland_source::WaylandSource::new(conn, event_queue);
    loop_handle
        .insert_source(wayland_source, |_, queue, state| {
            queue.dispatch_pending(state)
        })
        .map_err(|e| anyhow::anyhow!("wayland source: {e}"))?;

    loop_handle.insert_source(cmd_rx, |event, _, state| {
        if let calloop::channel::Event::Msg(cmd) = event {
            match cmd {
                Command::Start(id) => {
                    state.visible = true;
                    state.fade_target = 1.0;
                    state.correcting = false;
                    state.toast_remaining = 0.0;
                    state.text.clear();
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    state.reveal_len = 0;
                    state.chunk_fade = 1.0;
                    state.color_split = 0;
                    state.finalize_fade = 1.0;
                    state.shrink_t = 0.0;
                    state.shrink_target = 0.0;
                    if let Some(ind) = state.indicators.iter_mut().find(|i| i.id == id) {
                        ind.kind = IndicatorKind::Recording;
                        ind.eta_remaining = 0.0;
                        ind.eta_set = false;
                    } else {
                        state.indicators.push(Indicator {
                            id, kind: IndicatorKind::Recording, phase: 0.0, eta_remaining: 0.0, eta_set: false, fade: 0.0,
                        });
                    }
                    state.last_tick = std::time::Instant::now();
                    state.resize_and_redraw();
                }
                Command::SetText(text) => {
                    // Status-only shows only the circles — ignore transcript text entirely.
                    if !state.status_only {
                        state.shrink_target = 0.0;
                        if state.correcting && state.text != text {
                            // Corrected text arrived — crossfade from 0
                            state.correct_fade = 0.0;
                        }
                        state.text = text;
                        state.pending.clear();
                        state.wrapped_dirty = true;
                        state.update_reveal();
                        state.update_finalize();
                        if state.visible {
                            state.resize_and_redraw();
                        }
                    }
                }
                Command::SetPending(text) => {
                    if !state.status_only {
                        state.shrink_target = 0.0;
                        state.pending = text;
                        state.wrapped_dirty = true;
                        state.update_reveal();
                        if state.visible {
                            state.resize_and_redraw();
                        }
                    }
                }
                Command::Processing(id, eta) => {
                    if let Some(ind) = state.indicators.iter_mut().find(|i| i.id == id) {
                        ind.kind = IndicatorKind::Processing;
                        ind.eta_remaining = eta;
                        ind.eta_set = eta > 0.0;
                    } else {
                        state.indicators.push(Indicator {
                            id, kind: IndicatorKind::Processing, phase: 0.0, eta_remaining: eta, eta_set: eta > 0.0, fade: 0.0,
                        });
                    }
                    state.visible = true;
                    state.fade_target = 1.0;
                    state.resize_and_redraw();
                }
                Command::Done(id, kind) => {
                    if let Some(ind) = state.indicators.iter_mut().find(|i| i.id == id) {
                        ind.kind = IndicatorKind::Done(kind);
                        ind.phase = 0.0;
                        // Dismissed (no speech) fades out immediately; the rest hold their flash/icon.
                        ind.eta_remaining = if kind == DoneKind::Dismissed { 0.0 } else { DONE_HOLD_S };
                    }
                    // Full mode: hold the finished transcript briefly, then let it fade.
                    if !state.status_only && !state.text.is_empty() && state.toast_remaining <= 0.0 {
                        let len_s = (state.text.len() as f32 / 80.0).min(4.0) * 0.5;
                        state.toast_remaining = 1.5 + len_s;
                    }
                    state.resize_and_redraw();
                }
                Command::Toast(msg) => {
                    // Show a brief one-line message in the text panel even in status-only mode
                    // (paste/copy feedback). Auto-dismisses via toast_remaining in the tick.
                    state.visible = true;
                    state.fade_target = 1.0;
                    state.correcting = false;
                    state.shrink_target = 0.0;
                    state.text = msg;
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    state.update_reveal();
                    state.update_finalize();
                    state.toast_remaining = 2.5;
                    state.resize_and_redraw();
                }
                Command::Correcting => {
                    state.correcting = true;
                    state.correct_fade = 1.0; // text stays visible initially
                    state.resize_and_redraw();
                }
                Command::SetInfo(mode, lang) => {
                    state.mode = mode;
                    state.lang = lang;
                }
                Command::SetStatusOnly(v) => {
                    state.status_only = v;
                }
                Command::SetFont(name) => {
                    match find_font_path(&name) {
                        Ok(path) => match state.canvas.add_font(&path) {
                            Ok(id) => {
                                state.font_id = id;
                                state.wrapped_dirty = true;
                                tracing::info!("overlay: font changed to {name} ({path})");
                            }
                            Err(e) => tracing::error!("overlay: failed to load font {path}: {e}"),
                        }
                        Err(e) => tracing::error!("overlay: font lookup failed for {name}: {e}"),
                    }
                }
            }
        }
    }).map_err(|e| anyhow::anyhow!("cmd channel: {e}"))?;

    while !state.exit {
        let timeout = if state.is_animating() {
            std::time::Duration::from_millis(state.frame_ms)
        } else {
            std::time::Duration::from_secs(60)
        };
        event_loop.dispatch(timeout, &mut state)?;
        state.animation_tick();
    }

    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    egl_lib: egl::DynamicInstance<egl::EGL1_4>,
    egl_display: egl::Display,
    egl_surface: egl::Surface,
    wl_egl_surface: wayland_egl::WlEglSurface,
    canvas: Canvas<femtovg::renderer::OpenGl>,
    font_id: FontId,
    text: String,
    pending: String,
    mode: String,
    lang: String,
    wrapped_lines: Vec<String>,
    wrapped_dirty: bool,
    indicators: Vec<Indicator>,
    correcting: bool,
    status_only: bool,
    shrink_t: f32,
    shrink_target: f32,
    toast_remaining: f32,
    content_pw: f32,
    render_w: f32,
    anim_phase: f32,
    audio_level: Arc<AtomicU32>,
    audio_peak: f32,  // rolling peak for normalization
    smooth_audio: f32, // low-pass filtered audio level for bar targets
    bar_levels: [f32; 4],
    fade_alpha: f32,
    fade_target: f32,
    visible: bool,
    configured: bool,
    screen_width: u32,
    width: u32,
    height: f32,
    target_height: f32,
    max_height: u32,
    font_size: f32,
    line_height: f32,
    scale: i32,
    frame_ms: u64,
    last_tick: std::time::Instant,
    committed_layer_h: u32,
    last_egl_w: i32,
    last_egl_h: i32,
    correct_fade: f32,  // text crossfade during correction: 0=invisible, 1=visible
    reveal_len: usize,  // bytes of display text fully visible
    chunk_fade: f32,    // 0→1 fade for text after reveal_len
    color_split: usize, // bytes of confirmed-white final text
    finalize_fade: f32, // 0→1 grey→white for newly finalized text
    exit: bool,
}

fn chase(current: &mut f32, target: f32, tau: f32, dt: f32, epsilon: f32) -> bool {
    let diff = target - *current;
    if diff.abs() <= epsilon {
        *current = target;
        return false;
    }
    *current += diff * (1.0 - (-dt / tau).exp());
    true
}

impl State {
    fn is_animating(&self) -> bool {
        self.visible && (
            (self.fade_alpha - self.fade_target).abs() > 0.01
            || (self.shrink_t - self.shrink_target).abs() > 0.01
            || (self.render_w - self.content_pw).abs() > 1.0
            || (self.height - self.target_height).abs() > 0.5
            || !self.indicators.is_empty()
            || self.correcting
            || self.correct_fade < 0.99
            || self.chunk_fade < 0.99
            || self.finalize_fade < 0.99
            || f32::from_bits(self.audio_level.load(Ordering::Relaxed)) > 0.001
        )
    }

    fn animation_tick(&mut self) {
        if !self.visible {
            return;
        }
        let raw_dt = self.last_tick.elapsed().as_secs_f32() * 1000.0;
        self.last_tick = std::time::Instant::now();
        let dt = raw_dt.min(self.frame_ms as f32 * 2.0);
        let mut needs_redraw = false;

        // Fade animation (exponential chase)
        if chase(&mut self.fade_alpha, self.fade_target, FADE_TAU, dt, 0.01) {
            needs_redraw = true;
        }

        // Pill ↔ full morph animation (linear, cubic ease applied in render)
        if (self.shrink_t - self.shrink_target).abs() > 0.01 {
            let step = dt / SHRINK_DURATION_MS;
            if self.shrink_target > self.shrink_t {
                self.shrink_t = (self.shrink_t + step).min(1.0);
            } else {
                self.shrink_t = (self.shrink_t - step).max(0.0);
            }
            needs_redraw = true;
        } else {
            self.shrink_t = self.shrink_target;
        }
        // Per-indicator animation: advance pulse/spin phase, count down ETA, fade in, and fade out
        // + drop finished (Done) circles.
        self.indicators.retain_mut(|ind| {
            ind.phase += std::f32::consts::TAU * dt / 1000.0;
            match ind.kind {
                // Hold the done circle (flash + icon) for DONE_HOLD_S, then fade it out.
                IndicatorKind::Done(_) => {
                    if ind.eta_remaining > 0.0 {
                        ind.eta_remaining = (ind.eta_remaining - dt / 1000.0).max(0.0);
                    } else {
                        ind.fade = (ind.fade - dt / INDICATOR_FADE_MS).max(0.0);
                    }
                }
                _ => ind.fade = (ind.fade + dt / INDICATOR_FADE_MS).min(1.0),
            }
            if let IndicatorKind::Processing = ind.kind {
                if ind.eta_remaining > 0.0 {
                    ind.eta_remaining = (ind.eta_remaining - dt / 1000.0).max(0.0);
                }
            }
            !(matches!(ind.kind, IndicatorKind::Done(_)) && ind.fade <= 0.0)
        });
        if !self.indicators.is_empty() {
            needs_redraw = true;
        }
        // Nothing in flight and no text/toast/correction to show → fade the overlay out.
        if self.indicators.is_empty() && self.toast_remaining <= 0.0
            && self.text.is_empty() && self.pending.is_empty() && !self.correcting
        {
            self.fade_target = 0.0;
        }

        // Width animation (exponential chase)
        if chase(&mut self.render_w, self.content_pw, WIDTH_TAU, dt, 1.0) {
            needs_redraw = true;
        }

        // Height animation (exponential chase, only set_size when integer height changes)
        if chase(&mut self.height, self.target_height, HEIGHT_TAU, dt, 0.5) {
            let layer_h = self.height.round() as u32;
            if layer_h != self.committed_layer_h {
                self.committed_layer_h = layer_h;
                self.layer.set_size(0, layer_h);
            }
            needs_redraw = true;
        }

        if self.toast_remaining > 0.0 {
            self.toast_remaining -= dt / 1000.0;
            if self.toast_remaining <= 0.0 {
                self.toast_remaining = 0.0;
                self.text.clear();
                self.pending.clear();
                self.wrapped_dirty = true;
                // Drop back to the circle-row height; the fade-out condition above handles
                // hiding the overlay only when nothing is in flight.
                self.target_height = (self.compute_height() + SHADOW_PAD + SHADOW_PAD_BOT) as f32;
            }
        }
        if self.correcting {
            self.anim_phase += std::f32::consts::TAU * dt / 1000.0;
            needs_redraw = true;
        }

        // Text crossfade during correction
        if self.correct_fade < 0.99 {
            chase(&mut self.correct_fade, 1.0, 60.0, dt, 0.01);
            needs_redraw = true;
        }

        // Finalize fade (pending grey → final white)
        if self.finalize_fade < 0.99 {
            chase(&mut self.finalize_fade, 1.0, FINALIZE_TAU, dt, 0.01);
            if self.finalize_fade >= 0.99 {
                let sep = if !self.text.is_empty() && !self.pending.is_empty() && !self.text.ends_with(' ') { 1 } else { 0 };
                self.color_split = self.text.len() + sep;
            }
            needs_redraw = true;
        }

        // Chunk reveal fade
        if self.chunk_fade < 0.99 {
            chase(&mut self.chunk_fade, 1.0, CHUNK_TAU, dt, 0.01);
            if self.chunk_fade >= 0.99 {
                self.reveal_len = self.display_text().len();
            }
            needs_redraw = true;
        }

        // Audio-reactive bar levels (drive whenever mic is active)
        let raw_audio_level = f32::from_bits(self.audio_level.load(Ordering::Relaxed));
        if raw_audio_level > 0.001 {
            // Rolling peak: fast attack, slow decay — adapts to volume changes
            if raw_audio_level > self.audio_peak {
                self.audio_peak = self.audio_peak + (raw_audio_level - self.audio_peak) * 0.3;
            } else {
                self.audio_peak = self.audio_peak * (1.0 - dt / 3000.0); // decay over ~3s
            }
            self.audio_peak = self.audio_peak.max(0.005); // floor to avoid division issues
            let level = (raw_audio_level / self.audio_peak).clamp(0.0, 1.0);
            // Smooth the level before deriving bar targets (moderate attack, slow release)
            let smooth_tau = if level > self.smooth_audio { 50.0 } else { 120.0 };
            chase(&mut self.smooth_audio, level, smooth_tau, dt, 0.005);
            let vary = [1.0, 0.75, 0.9, 0.65];
            for i in 0..4 {
                let target = self.smooth_audio * vary[i];
                chase(&mut self.bar_levels[i], target, 30.0, dt, 0.005);
            }
            needs_redraw = true;
        } else if self.bar_levels.iter().any(|&l| l > 0.01) {
            for l in &mut self.bar_levels {
                chase(l, 0.0, 80.0, dt, 0.005);
            }
            needs_redraw = true;
        }

        if needs_redraw {
            self.redraw();
        }
    }

    fn update_refresh(&mut self, output: &wl_output::WlOutput) {
        if let Some(info) = self.output_state.info(output) {
            if let Some(mode) = info.modes.iter().find(|m| m.current) {
                if mode.refresh_rate > 0 {
                    self.frame_ms = (1000 / (mode.refresh_rate / 1000) as u64).max(1);
                }
            }
        }
    }

    fn display_text(&self) -> String {
        let mut full = self.text.clone();
        if !self.pending.is_empty() {
            if !full.is_empty() && !full.ends_with(' ') {
                full.push(' ');
            }
            full.push_str(&self.pending);
        }
        full
    }

    fn update_reveal(&mut self) {
        if self.correcting {
            self.reveal_len = self.display_text().len();
            self.chunk_fade = 1.0;
            return;
        }
        let new_len = self.display_text().len();
        if new_len < self.reveal_len {
            self.reveal_len = new_len;
        }
        if new_len > self.reveal_len && self.chunk_fade >= 0.99 {
            self.chunk_fade = 0.0;
        }
    }

    fn update_finalize(&mut self) {
        if self.correcting {
            self.color_split = self.text.len();
            self.finalize_fade = 1.0;
            return;
        }
        let new_split = self.text.len(); // pending is cleared in SetText
        if new_split > self.color_split && self.finalize_fade >= 0.99 {
            self.finalize_fade = 0.0;
        }
        if new_split < self.color_split {
            self.color_split = new_split;
        }
    }

    fn measure_text_width(&self, text: &str, font_size: f32) -> f32 {
        let mut paint = Paint::default();
        paint.set_font(&[self.font_id]);
        paint.set_font_size(font_size);
        self.canvas.measure_text(0.0, 0.0, text, &paint)
            .map(|m| m.width())
            .unwrap_or(0.0)
    }

    fn wrap_text(&self, text: &str, max_width: f32, font_size: f32) -> Vec<String> {
        let mut lines = Vec::new();
        let mut current = String::new();
        for word in text.split_whitespace() {
            let test = if current.is_empty() {
                word.to_string()
            } else {
                format!("{current} {word}")
            };
            let w = self.measure_text_width(&test, font_size);
            if w > max_width && !current.is_empty() {
                lines.push(current);
                current = word.to_string();
            } else {
                current = test;
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
        lines
    }

    fn draw_waveform_bars(&mut self, cx: f32, cy: f32, size: f32, color: Color) {
        let bar_count = 4usize;
        let bar_w = size / 6.0;
        let gap = size / 5.0;
        let total_w = bar_count as f32 * bar_w + (bar_count - 1) as f32 * gap;
        let start_x = cx - total_w / 2.0;
        let max_h = size * 0.85;
        let min_h = size * 0.15;
        let paint = Paint::color(color);
        for i in 0..bar_count {
            let t = self.bar_levels[i].clamp(0.0, 1.0);
            let h = (min_h + t * (max_h - min_h)).max(min_h);
            let x = start_x + i as f32 * (bar_w + gap);
            let y = cy - h / 2.0;
            let mut path = Path::new();
            path.rounded_rect(x, y, bar_w, h, bar_w / 2.0);
            self.canvas.fill_path(&path, &paint);
        }
    }

    /// Draw the status circles — one per in-flight utterance — as a centered horizontal row.
    fn draw_indicators(&mut self, pw: f32, ph: f32, pad: f32, sf: f32) {
        let inds = self.indicators.clone();
        if inds.is_empty() {
            return;
        }
        let n = inds.len() as f32;
        let gap = CIRCLE_GAP * sf;
        let base_d = PILL_SIZE as f32 * sf;
        let max_total = pw * 0.95;
        let mut d = base_d;
        if n * d + (n - 1.0) * gap > max_total {
            d = ((max_total - (n - 1.0) * gap) / n).max(base_d * 0.4);
        }
        let row_w = n * d + (n - 1.0) * gap;
        let start_x = (pw - row_w) / 2.0;
        let cy = pad + ph / 2.0;
        let radius = d / 2.0;
        for (i, ind) in inds.iter().enumerate() {
            let a = self.fade_alpha * ind.fade.clamp(0.0, 1.0);
            if a <= 0.001 {
                continue;
            }
            let cx = start_x + i as f32 * (d + gap) + radius;
            let mut bg = Path::new();
            bg.circle(cx, cy, radius);
            self.canvas.fill_path(&bg, &Paint::color(Color::rgbaf(0.0, 0.0, 0.0, 0.92 * a)));
            let mut border = Paint::color(Color::rgbaf(1.0, 1.0, 1.0, 0.12 * a));
            border.set_line_width(sf);
            self.canvas.stroke_path(&bg, &border);
            let icon_area = d * (1.0 - PILL_ICON_PAD * 2.0);
            match ind.kind {
                IndicatorKind::Recording => {
                    let pulse = (ind.phase * (1000.0 / RECORD_PULSE_MS)).sin() * 0.25 + 0.75;
                    self.draw_waveform_bars(cx, cy, icon_area, Color::rgbaf(0.9, 0.27, 0.22, pulse * a));
                }
                IndicatorKind::Processing => {
                    // Amber once the request overruns its (known) ETA, neutral otherwise.
                    let overrun = ind.eta_set && ind.eta_remaining <= 0.0;
                    let (cr, cg, cb) = if overrun { (0.95, 0.7, 0.2) } else { (0.92, 0.92, 0.92) };
                    let a0 = ind.phase * (1000.0 / SPINNER_MS);
                    let a1 = a0 + std::f32::consts::PI * 1.3;
                    let mut arc = Path::new();
                    arc.arc(cx, cy, radius * 0.62, a0, a1, femtovg::Solidity::Hole);
                    let mut arc_paint = Paint::color(Color::rgbaf(cr, cg, cb, 0.9 * a));
                    arc_paint.set_line_width(2.5 * sf);
                    self.canvas.stroke_path(&arc, &arc_paint);
                    // Countdown number while ticking; "…" once it overruns; nothing if ETA unknown.
                    let label = if ind.eta_remaining > 0.0 {
                        Some(format!("{}", ind.eta_remaining.ceil() as u32))
                    } else if ind.eta_set {
                        Some("…".to_string())
                    } else {
                        None
                    };
                    if let Some(label) = label {
                        let label_size = d * 0.34;
                        let mut paint = Paint::color(Color::rgbaf(cr, cg, cb, a));
                        paint.set_font(&[self.font_id]);
                        paint.set_font_size(label_size);
                        paint.set_text_baseline(Baseline::Middle);
                        let tw = self.measure_text_width(&label, label_size);
                        let _ = self.canvas.fill_text(cx - tw / 2.0, cy, &label, &paint);
                    }
                }
                IndicatorKind::Done(kind) => {
                    // Flash at completion, decaying over ~0.18s (phase resets to 0 on Done and
                    // advances in radians, so TAU*0.18 is roughly one flash's worth).
                    let flash = (1.0 - ind.phase / (std::f32::consts::TAU * 0.18)).clamp(0.0, 1.0);
                    let cs = icon_area * 0.28;
                    match kind {
                        DoneKind::Dismissed => {} // no speech — no icon, fades out immediately
                        DoneKind::Delivered => {
                            if flash > 0.0 {
                                let mut fl = Path::new();
                                fl.circle(cx, cy, radius);
                                self.canvas.fill_path(&fl, &Paint::color(Color::rgbaf(0.3, 0.8, 0.42, 0.55 * flash * a)));
                            }
                            let mut check = Path::new();
                            check.move_to(cx - cs, cy);
                            check.line_to(cx - cs * 0.25, cy + cs * 0.7);
                            check.line_to(cx + cs, cy - cs * 0.6);
                            let mut paint = Paint::color(Color::rgbaf(0.5, 0.92, 0.55, a));
                            paint.set_line_width(3.0 * sf);
                            self.canvas.stroke_path(&check, &paint);
                        }
                        DoneKind::Copied => {
                            if flash > 0.0 {
                                let mut fl = Path::new();
                                fl.circle(cx, cy, radius);
                                self.canvas.fill_path(&fl, &Paint::color(Color::rgbaf(0.6, 0.62, 0.7, 0.4 * flash * a)));
                            }
                            // Clipboard glyph: body + a small tab on top.
                            let bw = cs * 1.4;
                            let bh = cs * 1.8;
                            let top = cy - bh / 2.0;
                            let mut paint = Paint::color(Color::rgbaf(0.9, 0.9, 0.96, a));
                            paint.set_line_width(2.2 * sf);
                            let mut body = Path::new();
                            body.rounded_rect(cx - bw / 2.0, top, bw, bh, cs * 0.22);
                            self.canvas.stroke_path(&body, &paint);
                            let tw = bw * 0.5;
                            let th = cs * 0.45;
                            let mut tab = Path::new();
                            tab.rounded_rect(cx - tw / 2.0, top - th * 0.55, tw, th, th * 0.3);
                            self.canvas.stroke_path(&tab, &paint);
                        }
                        DoneKind::Failed => {
                            if flash > 0.0 {
                                let mut fl = Path::new();
                                fl.circle(cx, cy, radius);
                                self.canvas.fill_path(&fl, &Paint::color(Color::rgbaf(0.85, 0.25, 0.22, 0.6 * flash * a)));
                            }
                            let mut x = Path::new();
                            x.move_to(cx - cs * 0.7, cy - cs * 0.7);
                            x.line_to(cx + cs * 0.7, cy + cs * 0.7);
                            x.move_to(cx + cs * 0.7, cy - cs * 0.7);
                            x.line_to(cx - cs * 0.7, cy + cs * 0.7);
                            let mut paint = Paint::color(Color::rgbaf(0.95, 0.5, 0.45, a));
                            paint.set_line_width(3.0 * sf);
                            self.canvas.stroke_path(&x, &paint);
                        }
                    }
                }
            }
        }
    }

    fn rewrap_if_dirty(&mut self) {
        if !self.wrapped_dirty {
            return;
        }
        self.wrapped_dirty = false;
        let sf = self.scale as f32;
        let pw = (self.width * self.scale as u32) as f32;
        let display = self.display_text();
        let font_sz = self.font_size * sf;
        let max_text_w = pw - PADDING_X * sf * 2.0;
        self.wrapped_lines = self.wrap_text(&display, max_text_w, font_sz);
    }

    /// Whether the text panel (transcript / toast / correction) should be shown. Otherwise the
    /// status circles are the display.
    fn text_active(&self) -> bool {
        self.toast_remaining > 0.0
            || (!self.status_only && (!self.text.is_empty() || !self.pending.is_empty() || self.correcting))
    }

    fn compute_height(&mut self) -> u32 {
        if !self.text_active() {
            return PILL_SIZE;
        }
        let sf = self.scale as f32;
        let pw = (self.width * self.scale as u32) as f32;
        let font_sz = self.font_size * sf;

        self.rewrap_if_dirty();
        let num_lines = self.wrapped_lines.len().max(1) as u32;

        // Content width: for single line, fit to text; for multi-line, full width
        self.content_pw = if num_lines > 1 {
            pw
        } else {
            let widest = self.wrapped_lines.iter()
                .map(|l| self.measure_text_width(l, font_sz))
                .fold(0.0f32, f32::max);
            (widest + PADDING_X * sf * 2.0).min(pw)
        };

        let h = PADDING_Y as u32 * 2 + num_lines * self.line_height as u32;
        let h = if self.shrink_target > 0.5 { h.max(PILL_SIZE) } else { h };
        h.min(self.max_height)
    }

    fn resize_and_redraw(&mut self) {
        if !self.configured || self.width == 0 {
            return;
        }
        let h = (self.compute_height() + SHADOW_PAD + SHADOW_PAD_BOT) as f32;
        self.target_height = h;
        self.redraw();
    }

    fn redraw(&mut self) {
        if self.width == 0 || !self.configured {
            return;
        }

        let s = self.scale;
        let sf = s as f32;
        let pw = (self.width * s as u32) as f32;
        let layer_h = self.height * sf;
        let pad = SHADOW_PAD as f32 * sf;
        let pad_bot = SHADOW_PAD_BOT as f32 * sf;
        let ph = (layer_h - pad - pad_bot).max(1.0);

        // Resize EGL surface only when pixel dimensions change
        let egl_w = pw as i32;
        let egl_h = layer_h as i32;
        if egl_w != self.last_egl_w || egl_h != self.last_egl_h {
            self.wl_egl_surface.resize(egl_w, egl_h, 0, 0);
            self.canvas.set_size(egl_w as u32, egl_h as u32, 1.0);
            self.last_egl_w = egl_w;
            self.last_egl_h = egl_h;
        }
        self.canvas.clear_rect(0, 0, egl_w as u32, egl_h as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));

        // Fully faded out — just clear and mark invisible
        if self.fade_alpha <= 0.01 && self.fade_target <= 0.0 {
            self.canvas.flush();
            self.egl_lib.swap_buffers(self.egl_display, self.egl_surface).ok();
            self.visible = false;
            return;
        }

        // Status circles (one per in-flight utterance) are the default display; the text panel is
        // only drawn when there's a transcript / toast / correction to show.
        if !self.text_active() {
            self.draw_indicators(pw, ph, pad, sf);
            self.canvas.flush();
            self.egl_lib.swap_buffers(self.egl_display, self.egl_surface).ok();
            self.layer.wl_surface().set_buffer_scale(s);
            return;
        }

        // Shrink animation ease
        let ease_t = if self.shrink_t > 0.0 {
            let t = self.shrink_t.min(1.0);
            1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
        } else {
            0.0
        };

        let bg_alpha = 0.92 * self.fade_alpha;
        let target_h = PILL_SIZE as f32 * sf;

        let target_w = target_h;

        // Background rect geometry (content area starts at y=pad)
        let base_w = self.render_w.min(pw).max(target_h);
        let (rx, ry, rw, rh) = if ease_t > 0.0 {
            let rw = base_w + (target_w - base_w) * ease_t;
            let rh = ph + (target_h - ph) * ease_t;
            let rx = (pw - rw) / 2.0;
            let ry = pad + ph - rh;
            (rx, ry, rw, rh)
        } else {
            let rx = (pw - base_w) / 2.0;
            (rx, pad, base_w, ph)
        };

        let r = CORNER_RADIUS * sf;
        let is_circle = ease_t > 0.99;

        // Helper: build the bg shape path
        let cx = rx + rw / 2.0;
        let cy = ry + rh / 2.0;
        let make_bg_path = |inflate: f32| -> Path {
            let mut p = Path::new();
            if is_circle {
                p.circle(cx, cy + inflate * 0.5, rh / 2.0 + inflate);
            } else {
                let ri = r + inflate;
                p.rounded_rect(rx - inflate, ry - inflate, rw + inflate * 2.0, rh + inflate * 2.0, ri);
            }
            p
        };

        // Drop shadow
        {
            let shadow_expand = SHADOW_FEATHER * sf;
            let shadow_path = make_bg_path(shadow_expand);
            let shadow_paint = Paint::box_gradient(
                rx, ry + SHADOW_OFFSET_Y * sf,
                rw, rh,
                if is_circle { rh / 2.0 } else { r },
                SHADOW_FEATHER * sf,
                Color::rgbaf(0.0, 0.0, 0.0, 0.2 * self.fade_alpha),
                Color::rgbaf(0.0, 0.0, 0.0, 0.0),
            );
            self.canvas.fill_path(&shadow_path, &shadow_paint);
        }

        // Background
        {
            let bg_path = make_bg_path(0.0);
            let bg_paint = Paint::color(Color::rgbaf(0.0, 0.0, 0.0, bg_alpha));
            self.canvas.fill_path(&bg_path, &bg_paint);

            // Border (warm color during correction, subtle white otherwise)
            let border_path = make_bg_path(0.0);
            let (br, bg, bb, ba, bw) = if self.correcting {
                let pulse = (self.anim_phase * 1.5).sin() * 0.08 + 0.92;
                (0.9, 0.45, 0.15, 0.7 * pulse * self.fade_alpha, 1.5 * sf)
            } else {
                (1.0, 1.0, 1.0, 0.12 * self.fade_alpha, sf)
            };
            let mut border_paint = Paint::color(Color::rgbaf(br, bg, bb, ba));
            border_paint.set_line_width(bw);
            self.canvas.stroke_path(&border_path, &border_paint);

            // Correction progress sweep (indeterminate bar at bottom)
            if self.correcting && !is_circle {
                let bar_h = 3.0 * sf;
                let bar_w = rw * 0.3;
                let inset = r + 2.0 * sf;
                let t = (self.anim_phase * 0.5).sin() * 0.5 + 0.5;
                let bar_x = rx + inset + t * (rw - 2.0 * inset - bar_w);
                let bar_y = ry + rh - bar_h - sf;
                let mut bar_path = Path::new();
                bar_path.rounded_rect(bar_x, bar_y, bar_w, bar_h, bar_h / 2.0);
                let bar_paint = Paint::color(Color::rgbaf(0.9, 0.45, 0.15, 0.7 * self.fade_alpha));
                self.canvas.fill_path(&bar_path, &bar_paint);
            }
        }

        // Text opacity (includes correction crossfade)
        let text_opacity = (1.0 - ease_t * 3.0).max(0.0);
        let text_alpha = self.fade_alpha * text_opacity * self.correct_fade;
        // Normal transcript text
        if text_alpha > 0.01 {
            let font_sz = self.font_size * sf;

            // Position text for target width so it doesn't shift during box animation
            let content_w = self.content_pw.min(pw).max(PILL_SIZE as f32 * sf);
            let content_rx = (pw - content_w) / 2.0;
            let text_x = content_rx + PADDING_X * sf;

            self.rewrap_if_dirty();
            let display = self.display_text();
            let num_lines = self.wrapped_lines.len();
            let total_text_h = num_lines as f32 * self.line_height * sf;
            let visible_h = ph - PADDING_Y * sf * 2.0;
            let scroll_y = (total_text_h - visible_h).max(0.0);

            // Clip to target content area (not animated box) so text is visible during width growth
            self.canvas.save();
            self.canvas.intersect_scissor(content_rx, ry, content_w, rh);

            // Determine where final text ends and pending starts
            let separator = if !self.text.is_empty() && !self.pending.is_empty() && !self.text.ends_with(' ') {
                " "
            } else {
                ""
            };
            let final_with_sep = format!("{}{separator}", self.text);

            let pending_alpha = text_alpha * 0.5;

            let split_at = final_with_sep.len();
            let color_at = self.color_split;
            let reveal_at = self.reveal_len;

            // Interpolated finalize color (pending grey → final white)
            let fin_c = 0.6 + 0.4 * self.finalize_fade;
            let fin_a = pending_alpha + (text_alpha - pending_alpha) * self.finalize_fade;

            let base_y = pad + ph - PADDING_Y * sf - total_text_h + scroll_y;
            let mut display_pos = 0usize;

            let make_paint = |r: f32, g: f32, b: f32, a: f32| -> Paint {
                let mut p = Paint::color(Color::rgbaf(r, g, b, a));
                p.set_font(&[self.font_id]);
                p.set_font_size(font_sz);
                p.set_text_baseline(Baseline::Middle);
                p
            };
            let paints = [
                make_paint(1.0, 1.0, 1.0, text_alpha),                    // 0: confirmed final, revealed
                make_paint(fin_c, fin_c, fin_c, fin_a),                    // 1: finalizing, revealed
                make_paint(0.6, 0.6, 0.6, pending_alpha),                 // 2: pending, revealed
                make_paint(1.0, 1.0, 1.0, text_alpha * self.chunk_fade),  // 3: confirmed final, new
                make_paint(fin_c, fin_c, fin_c, fin_a * self.chunk_fade), // 4: finalizing, new
                make_paint(0.6, 0.6, 0.6, pending_alpha * self.chunk_fade), // 5: pending, new
            ];

            for (i, line) in self.wrapped_lines.iter().enumerate() {
                let y = base_y + (i as f32 + 0.5) * self.line_height * sf - scroll_y;

                // Advance display_pos to where this line's content starts
                if let Some(offset) = display[display_pos..].find(line.as_str()) {
                    display_pos += offset;
                }
                let line_start = display_pos;
                let line_end = display_pos + line.len();
                display_pos = line_end;

                if y < -self.line_height * sf || y > layer_h + self.line_height * sf {
                    continue;
                }

                // Three split points within this line (clamped, snapped to char boundaries)
                let cs = line.floor_char_boundary((color_at.saturating_sub(line_start)).min(line.len()));
                let sp = line.floor_char_boundary((split_at.saturating_sub(line_start)).min(line.len()));
                let rp = line.floor_char_boundary((reveal_at.saturating_sub(line_start)).min(line.len()));

                let mut cuts = [0, cs, sp, rp, line.len()];
                cuts.sort();

                let mut x_off = 0.0f32;
                for pair in cuts.windows(2) {
                    let (s, e) = (pair[0], pair[1]);
                    if s >= e { continue; }
                    let segment = &line[s..e];
                    let byte_pos = line_start + s;
                    let type_idx = if byte_pos < color_at { 0 }
                        else if byte_pos < split_at { 1 }
                        else { 2 };
                    let reveal_off = if byte_pos < reveal_at { 0 } else { 3 };
                    let _ = self.canvas.fill_text(text_x + x_off, y, segment, &paints[type_idx + reveal_off]);
                    x_off += self.measure_text_width(segment, font_sz);
                }
            }

            self.canvas.restore();

            // Waveform bars at bottom of text box (always drawn, min_h shows as dots when silent)
            {
                let bar_size = self.line_height * sf * 0.6;
                let bar_y = ry + rh - PADDING_Y * sf * 0.5;
                let bar_cx = rx + rw / 2.0;
                self.draw_waveform_bars(bar_cx, bar_y, bar_size, Color::rgbaf(0.85, 0.25, 0.2, 0.5 * self.fade_alpha));
            }
        }

        self.canvas.flush();
        self.egl_lib.swap_buffers(self.egl_display, self.egl_surface).ok();
        self.layer.wl_surface().set_buffer_scale(s);
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor;
        surface.set_buffer_scale(new_factor);
        self.redraw();
    }
    fn transform_changed(
        &mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {}
    fn surface_leave(
        &mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.update_refresh(&output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.update_refresh(&output);
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface,
        configure: LayerSurfaceConfigure, _: u32,
    ) {
        let w = configure.new_size.0.max(1);

        if self.screen_width == 0 || w > self.width + 100 {
            self.screen_width = w;
            let overlay_w = (w as f64 * OVERLAY_WIDTH_FRAC) as u32;
            let margin_h = ((w - overlay_w) / 2) as i32;
            let margin_b = 32;
            let content_w = overlay_w as f32 - PADDING_X * 2.0;
            self.font_size = content_w / (70.0 * CHAR_WIDTH_RATIO);
            self.line_height = self.font_size * 1.5;
            let golden_h = (overlay_w as f32 / 1.618) as u32;
            let max_lines = ((golden_h as f32 - PADDING_Y * 2.0) / self.line_height).floor().max(1.0) as u32;
            self.max_height = PADDING_Y as u32 * 2 + max_lines * self.line_height as u32 + SHADOW_PAD + SHADOW_PAD_BOT;
            self.layer.set_margin(0, margin_h, margin_b, margin_h);
            self.width = overlay_w;
        } else {
            self.width = w;
        }

        if configure.new_size.1 > 0 && (self.height - self.target_height).abs() <= 0.5 {
            let h = configure.new_size.1 as f32;
            self.height = h;
            self.target_height = h;
        }
        self.configured = true;
        self.redraw();
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);

impl wayland_client::Dispatch<wl_region::WlRegion, ()> for State {
    fn event(
        _: &mut Self, _: &wl_region::WlRegion, _: wl_region::Event,
        _: &(), _: &Connection, _: &QueueHandle<Self>,
    ) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
