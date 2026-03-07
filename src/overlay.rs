use anyhow::Result;
use femtovg::{Baseline, Canvas, Color, FontId, ImageFlags, ImageId, Paint, Path};
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

// Phosphor Icons (MIT) — Check Bold, green to match done glow
const CHECK_BOLD_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" fill="#33b34d"><path d="M232.49,80.49l-128,128a12,12,0,0,1-17,0l-56-56a12,12,0,1,1,17-17L96,183,215.51,63.51a12,12,0,0,1,17,17Z"/></svg>"##;

pub enum Command {
    Show,
    SetText(String),
    SetPending(String),
    Processing,
    Copied,
    Correcting,
    SetInfo(String, String),
    SetFont(String),
}

#[derive(Clone)]
pub struct Handle {
    tx: calloop::channel::Sender<Command>,
    audio_level: Arc<AtomicU32>,
}

impl Handle {
    pub fn show(&self) {
        let _ = self.tx.send(Command::Show);
    }

    pub fn set_text(&self, text: String) {
        let _ = self.tx.send(Command::SetText(text));
    }

    pub fn set_pending(&self, text: String) {
        let _ = self.tx.send(Command::SetPending(text));
    }

    pub fn processing(&self) {
        let _ = self.tx.send(Command::Processing);
    }

    pub fn copied(&self) {
        let _ = self.tx.send(Command::Copied);
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
        Some("dictate-overlay"),
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

    let check_icon = render_svg_to_image(&mut canvas, CHECK_BOLD_SVG, 64);

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
        listening: false,
        processing: false,
        correcting: false,
        done: false,
        shrink_t: 0.0,
        shrink_target: 0.0,
        pill_countdown: 0.0,
        content_pw: 0.0,
        render_w: 0.0,
        anim_phase: 0.0,
        audio_level,
        audio_peak: 0.05,
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
        check_icon,
        correct_fade: 1.0,
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
                Command::Show => {
                    state.visible = true;
                    state.listening = true;
                    state.processing = false;
                    state.correcting = false;
                    state.done = false;
                    state.shrink_t = 1.0;
                    state.shrink_target = 1.0;
                    state.pill_countdown = 0.0;
                    state.text.clear();
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    state.fade_alpha = 0.0;
                    state.fade_target = 1.0;
                    let pill_w = PILL_SIZE as f32 * state.scale as f32;
                    state.content_pw = pill_w;
                    state.render_w = pill_w * 0.7;
                    let init_h = (PILL_SIZE + SHADOW_PAD + SHADOW_PAD_BOT) as f32;
                    state.height = init_h;
                    state.target_height = init_h;
                    state.committed_layer_h = init_h as u32;
                    state.layer.set_size(0, init_h as u32);
                    state.last_tick = std::time::Instant::now();
                    state.resize_and_redraw();
                }
                Command::SetText(text) => {
                    if state.listening || state.processing {
                        state.shrink_target = 0.0;
                    }
                    state.listening = false;
                    state.processing = false;
                    if state.correcting && state.text != text {
                        // Corrected text arrived — crossfade from 0
                        state.correct_fade = 0.0;
                    }
                    state.text = text;
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    if state.visible {
                        state.resize_and_redraw();
                    }
                }
                Command::SetPending(text) => {
                    if state.listening || state.processing {
                        state.shrink_target = 0.0;
                    }
                    state.listening = false;
                    state.processing = false;
                    state.pending = text;
                    state.wrapped_dirty = true;
                    if state.visible {
                        state.resize_and_redraw();
                    }
                }
                Command::Processing => {
                    state.listening = false;
                    state.processing = true;
                    state.shrink_target = 1.0;
                    state.resize_and_redraw();
                }
                Command::Copied => {
                    state.listening = false;
                    state.processing = false;
                    state.correcting = false;
                    if state.text.is_empty() {
                        state.fade_target = 0.0;
                    } else {
                        state.done = true;
                        state.shrink_target = 1.0;
                        state.resize_and_redraw();
                    }
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
    listening: bool,
    processing: bool,
    correcting: bool,
    done: bool,
    shrink_t: f32,
    shrink_target: f32,
    pill_countdown: f32,
    content_pw: f32,
    render_w: f32,
    anim_phase: f32,
    audio_level: Arc<AtomicU32>,
    audio_peak: f32, // rolling peak for normalization
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
    check_icon: ImageId,
    correct_fade: f32,  // text crossfade during correction: 0=invisible, 1=visible
    exit: bool,
}

fn render_svg_to_image(canvas: &mut Canvas<femtovg::renderer::OpenGl>, svg: &str, size: u32) -> ImageId {
    let tree = resvg::usvg::Tree::from_str(svg, &resvg::usvg::Options::default())
        .expect("embedded SVG is valid");
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).unwrap();
    let sx = size as f32 / tree.size().width();
    let sy = size as f32 / tree.size().height();
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    // tiny-skia pixels are RGBA premultiplied — convert to femtovg RGBA8
    let pixels: Vec<femtovg::rgb::RGBA8> = pixmap.pixels().iter().map(|p| {
        femtovg::rgb::RGBA8::new(p.red(), p.green(), p.blue(), p.alpha())
    }).collect();
    let img = femtovg::imgref::ImgRef::new(&pixels, size as usize, size as usize);
    canvas.create_image(img, ImageFlags::GENERATE_MIPMAPS | ImageFlags::PREMULTIPLIED).unwrap()
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
            || self.pill_countdown > 0.0
            || (self.render_w - self.content_pw).abs() > 1.0
            || (self.height - self.target_height).abs() > 0.5
            || self.listening
            || self.processing
            || self.correcting
            || self.done
            || self.correct_fade < 0.99
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
        // Trigger pill hold when shrink-to-pill completes
        if self.shrink_t >= 1.0 && self.pill_countdown <= 0.0
            && !self.listening && !self.processing && self.fade_target > 0.0
        {
            self.pill_countdown = 1.2;
            needs_redraw = true;
        }

        // "Copied" pill hold → fade
        if self.pill_countdown > 0.0 {
            self.pill_countdown -= dt / 1000.0;
            if self.pill_countdown <= 0.0 {
                self.pill_countdown = 0.0;
                self.fade_target = 0.0;
            }
            needs_redraw = true;
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

        // Pulse animation for listening/processing/correcting indicator
        if self.listening || self.processing || self.correcting || self.done {
            self.anim_phase += std::f32::consts::TAU * dt / 1000.0;
            needs_redraw = true;
        }

        // Text crossfade during correction
        if self.correct_fade < 0.99 {
            chase(&mut self.correct_fade, 1.0, 60.0, dt, 0.01);
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
            let taus = [12.0, 20.0, 16.0, 25.0];
            let vary = [1.0, 0.75, 0.9, 0.65];
            for i in 0..4 {
                let target = level * vary[i];
                chase(&mut self.bar_levels[i], target, taus[i], dt, 0.005);
            }
            needs_redraw = true;
        } else if self.bar_levels.iter().any(|&l| l > 0.01) {
            for l in &mut self.bar_levels {
                chase(l, 0.0, 30.0, dt, 0.005);
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
        if self.listening || self.processing {
            return String::new();
        }
        let mut full = self.text.clone();
        if !self.pending.is_empty() {
            if !full.is_empty() && !full.ends_with(' ') {
                full.push(' ');
            }
            full.push_str(&self.pending);
        }
        full
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

    fn compute_height(&mut self) -> u32 {
        if self.listening || self.processing {
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

        // Shrink animation ease
        let ease_t = if self.shrink_t > 0.0 {
            let t = self.shrink_t.min(1.0);
            1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
        } else {
            0.0
        };

        let bg_alpha = 0.7 * self.fade_alpha;
        let target_h = PILL_SIZE as f32 * sf;

        // Pill shrinks to a circle for both recording and copied icons
        let is_recording_pill = self.listening || self.shrink_target < 0.5;
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

        // Colored glow: red for recording, yellow for correcting, green for done
        {
            let (gr, gg, gb, ga) = if self.listening {
                (0.8, 0.1, 0.08, 0.35)
            } else if self.correcting {
                (0.85, 0.65, 0.1, 0.3)
            } else if self.done {
                (0.2, 0.7, 0.3, 0.3)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            if ga > 0.0 {
                let glow_pulse = (self.anim_phase * 0.8).sin() * 0.15 + 0.85;
                let mut glow_path = make_bg_path(SHADOW_FEATHER * sf);
                let glow_paint = Paint::box_gradient(
                    rx, ry,
                    rw, rh,
                    if is_circle { rh / 2.0 } else { r },
                    SHADOW_FEATHER * sf * 1.2,
                    Color::rgbaf(gr, gg, gb, ga * self.fade_alpha * glow_pulse),
                    Color::rgbaf(gr, gg, gb, 0.0),
                );
                self.canvas.fill_path(&glow_path, &glow_paint);
            }
        }

        // Audio-reactive glow — intensity follows waveform
        {
            let avg_level = self.bar_levels.iter().sum::<f32>() / 4.0;
            let in_pill = (self.listening || self.processing) && ease_t > 0.3;
            if avg_level > 0.005 && !in_pill && self.fade_alpha > 0.01 {
                let glow_path = make_bg_path(SHADOW_FEATHER * sf);
                let glow_paint = Paint::box_gradient(
                    rx, ry, rw, rh,
                    if is_circle { rh / 2.0 } else { r },
                    SHADOW_FEATHER * sf * (0.6 + avg_level * 0.6),
                    Color::rgbaf(0.8, 0.12, 0.08, 0.4 * avg_level * self.fade_alpha),
                    Color::rgbaf(0.8, 0.12, 0.08, 0.0),
                );
                self.canvas.fill_path(&glow_path, &glow_paint);
            }
        }

        // Background
        {
            let bg_path = make_bg_path(0.0);
            let bg_paint = Paint::color(Color::rgbaf(0.0, 0.0, 0.0, bg_alpha));
            self.canvas.fill_path(&bg_path, &bg_paint);

            // Top edge highlight
            if !is_circle {
                let hl_y = ry + sf * 0.5;
                let hl_inset = r * 0.6;
                let mut hl = Path::new();
                hl.move_to(rx + hl_inset, hl_y);
                hl.line_to(rx + rw - hl_inset, hl_y);
                let hl_alpha = if self.listening || self.correcting { 0.12 } else { 0.07 };
                let mut hl_paint = Paint::color(Color::rgbaf(1.0, 1.0, 1.0, hl_alpha * self.fade_alpha));
                hl_paint.set_line_width(sf);
                hl_paint.set_line_cap(femtovg::LineCap::Round);
                self.canvas.stroke_path(&hl, &hl_paint);
            }
        }

        // Text opacity (includes correction crossfade)
        let text_opacity = (1.0 - ease_t * 3.0).max(0.0);
        let text_alpha = self.fade_alpha * text_opacity * self.correct_fade;
        let pill_label_opacity = if ease_t > 0.3 { ((ease_t - 0.3) * (1.0 / 0.7)).min(1.0) } else { 0.0 };
        let pill_alpha = self.fade_alpha * pill_label_opacity;

        let show_pill = (self.listening || self.processing || self.pill_countdown > 0.0 || pill_alpha > 0.0) && ease_t > 0.3;

        // Pill icons: waveform for recording, pulsing dot for processing, check for done
        if show_pill {
            let cx = rx + rw / 2.0;
            let cy = ry + rh / 2.0;
            let icon_area = rh * (1.0 - PILL_ICON_PAD * 2.0); // content area after padding
            let pulse = (self.anim_phase.sin() * 0.5 + 0.5) * 0.4 + 0.6;

            if self.processing {
                let a = pulse * pill_alpha;
                let radius = icon_area * 0.25;
                let mut circle = Path::new();
                circle.circle(cx, cy, radius);
                let paint = Paint::color(Color::rgbaf(0.85, 0.85, 0.85, a));
                self.canvas.fill_path(&circle, &paint);
            } else if is_recording_pill {
                let a = pulse * pill_alpha;
                let show_label = self.listening && !self.lang.is_empty() && self.lang != "auto";
                let bar_cy = if show_label { cy - icon_area * 0.08 } else { cy };
                self.draw_waveform_bars(cx, bar_cy, icon_area, Color::rgbaf(0.85, 0.25, 0.2, a));
                if show_label {
                    let label = self.lang.to_uppercase();
                    let label_size = rh * 0.19;
                    let mut paint = Paint::color(Color::rgbaf(1.0, 1.0, 1.0, pill_alpha * 0.45));
                    paint.set_font(&[self.font_id]);
                    paint.set_font_size(label_size);
                    paint.set_text_baseline(Baseline::Middle);
                    let tw = self.measure_text_width(&label, label_size);
                    let _ = self.canvas.fill_text(cx - tw / 2.0, cy + icon_area * 0.55, &label, &paint);
                }
            } else {
                // Check icon from SVG texture — sized to fill icon_area
                let icon_sz = icon_area;
                let paint = Paint::image(self.check_icon, cx - icon_sz / 2.0, cy - icon_sz / 2.0, icon_sz, icon_sz, 0.0, pill_alpha);
                let mut path = Path::new();
                path.rect(cx - icon_sz / 2.0, cy - icon_sz / 2.0, icon_sz, icon_sz);
                self.canvas.fill_path(&path, &paint);
            }
        }

        // Normal transcript text
        if text_alpha > 0.01 && !show_pill {
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

            // Render each line, coloring final vs pending portions.
            // Track position in the display string by finding each wrapped line's
            // start via str::find, which is correct for any UTF-8 content.
            let split_at = final_with_sep.len();
            let base_y = pad + ph - PADDING_Y * sf - total_text_h + scroll_y;
            let mut display_pos = 0usize;

            let mut final_paint = Paint::color(Color::rgbaf(1.0, 1.0, 1.0, text_alpha));
            final_paint.set_font(&[self.font_id]);
            final_paint.set_font_size(font_sz);
            final_paint.set_text_baseline(Baseline::Middle);

            let mut pend_paint = Paint::color(Color::rgbaf(0.6, 0.6, 0.6, pending_alpha));
            pend_paint.set_font(&[self.font_id]);
            pend_paint.set_font_size(font_sz);
            pend_paint.set_text_baseline(Baseline::Middle);

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

                if line_end <= split_at {
                    let _ = self.canvas.fill_text(text_x, y, line, &final_paint);
                } else if line_start >= split_at {
                    let _ = self.canvas.fill_text(text_x, y, line, &pend_paint);
                } else {
                    // Line spans the boundary — split at char boundary
                    let byte_in_line = split_at - line_start;
                    // Ensure we split on a char boundary
                    let boundary = line.floor_char_boundary(byte_in_line);
                    let final_part = &line[..boundary];
                    let pending_part = &line[boundary..];

                    let _ = self.canvas.fill_text(text_x, y, final_part, &final_paint);
                    if !pending_part.is_empty() {
                        let final_w = self.measure_text_width(final_part, font_sz);
                        let _ = self.canvas.fill_text(text_x + final_w, y, pending_part, &pend_paint);
                    }
                }
            }

            self.canvas.restore();
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
            self.font_size = content_w / (100.0 * CHAR_WIDTH_RATIO);
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
