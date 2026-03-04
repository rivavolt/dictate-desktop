use anyhow::Result;
use femtovg::{Baseline, Canvas, Color, FontId, Paint, Path};
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
    protocol::{wl_output, wl_surface},
    Connection, Proxy, QueueHandle,
};

const PILL_SIZE: u32 = 56;
const PADDING_X: f32 = 16.0;
const PADDING_Y: f32 = 8.0;
const OVERLAY_WIDTH_FRAC: f64 = 0.618;
const CHAR_WIDTH_RATIO: f32 = 0.47;
const CORNER_RADIUS: f32 = 12.0;
const FADE_DURATION_MS: f32 = 150.0;
const SHRINK_DURATION_MS: f32 = 150.0;
const WIDTH_ANIM_MS: f32 = 100.0;
const SHADOW_FEATHER: f32 = 20.0;
const SHADOW_OFFSET_Y: f32 = 4.0;

pub enum Command {
    Show,
    SetText(String),
    SetPending(String),
    Copied,
    SetFont(String),
}

#[derive(Clone)]
pub struct Handle {
    tx: calloop::channel::Sender<Command>,
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

    pub fn copied(&self) {
        let _ = self.tx.send(Command::Copied);
    }

    pub fn set_font(&self, name: String) {
        let _ = self.tx.send(Command::SetFont(name));
    }
}

pub fn spawn(font: String) -> Result<Handle> {
    let (tx, rx) = calloop::channel::channel::<Command>();
    let handle = Handle { tx };

    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            if let Err(e) = run(rx, &font) {
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

fn run(cmd_rx: calloop::channel::Channel<Command>, font_name: &str) -> Result<()> {
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
    layer.set_size(0, PILL_SIZE);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
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
        PILL_SIZE as i32,
    )?;

    let egl_surface = unsafe {
        egl_lib.create_window_surface(egl_display, egl_config, wl_egl_surface.ptr() as egl::NativeWindowType, None)?
    };

    egl_lib.make_current(egl_display, Some(egl_surface), Some(egl_surface), Some(egl_context))?;

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
        wrapped_lines: Vec::new(),
        wrapped_dirty: true,
        listening: false,
        shrink_t: 0.0,
        shrink_target: 0.0,
        pill_countdown: 0.0,
        content_pw: 0.0,
        render_w: 0.0,
        anim_phase: 0.0,
        fade_alpha: 0.0,
        fade_target: 0.0,
        visible: false,
        configured: false,
        screen_width: 0,
        width: 0,
        height: PILL_SIZE,
        max_height: 400,
        font_size: 16.0,
        line_height: 24.0,
        scale: 1,
        frame_ms: 16,
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
                    state.shrink_t = 1.0;
                    state.shrink_target = 1.0;
                    state.pill_countdown = 0.0;
                    state.text.clear();
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    state.fade_alpha = 1.0;
                    state.fade_target = 1.0;
                    let pill_w = PILL_SIZE as f32 * state.scale as f32;
                    state.content_pw = pill_w;
                    state.render_w = pill_w;
                    state.resize_and_redraw();
                }
                Command::SetText(text) => {
                    if state.listening {
                        state.shrink_target = 0.0;
                    }
                    state.listening = false;
                    state.text = text;
                    state.pending.clear();
                    state.wrapped_dirty = true;
                    if state.visible {
                        state.resize_and_redraw();
                    }
                }
                Command::SetPending(text) => {
                    if state.listening {
                        state.shrink_target = 0.0;
                    }
                    state.listening = false;
                    state.pending = text;
                    state.wrapped_dirty = true;
                    if state.visible {
                        state.resize_and_redraw();
                    }
                }
                Command::Copied => {
                    state.listening = false;
                    if state.text.is_empty() {
                        state.fade_target = 0.0;
                    } else {
                        state.shrink_target = 1.0;
                        state.redraw();
                    }
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
    wrapped_lines: Vec<String>,
    wrapped_dirty: bool,
    listening: bool,
    shrink_t: f32,
    shrink_target: f32,
    pill_countdown: f32,
    content_pw: f32,
    render_w: f32,
    anim_phase: f32,
    fade_alpha: f32,
    fade_target: f32,
    visible: bool,
    configured: bool,
    screen_width: u32,
    width: u32,
    height: u32,
    max_height: u32,
    font_size: f32,
    line_height: f32,
    scale: i32,
    frame_ms: u64,
    exit: bool,
}

impl State {
    fn is_animating(&self) -> bool {
        self.visible && (
            (self.fade_alpha - self.fade_target).abs() > 0.01
            || (self.shrink_t - self.shrink_target).abs() > 0.01
            || self.pill_countdown > 0.0
            || (self.render_w - self.content_pw).abs() > 1.0
            || self.listening
        )
    }

    fn animation_tick(&mut self) {
        if !self.visible {
            return;
        }
        let mut needs_redraw = false;

        // Fade animation
        if (self.fade_alpha - self.fade_target).abs() > 0.01 {
            let step = self.frame_ms as f32 / FADE_DURATION_MS;
            if self.fade_target > self.fade_alpha {
                self.fade_alpha = (self.fade_alpha + step).min(1.0);
            } else {
                self.fade_alpha = (self.fade_alpha - step).max(0.0);
            }
            needs_redraw = true;
        }

        // Pill ↔ full morph animation
        if (self.shrink_t - self.shrink_target).abs() > 0.01 {
            let step = self.frame_ms as f32 / SHRINK_DURATION_MS;
            if self.shrink_target > self.shrink_t {
                self.shrink_t = (self.shrink_t + step).min(1.0);
            } else {
                self.shrink_t = (self.shrink_t - step).max(0.0);
            }
            if self.shrink_t >= 1.0 && !self.listening {
                self.pill_countdown = 0.6;
            }
            needs_redraw = true;
        }

        // "Copied" pill hold → fade
        if self.pill_countdown > 0.0 {
            self.pill_countdown -= self.frame_ms as f32 / 1000.0;
            if self.pill_countdown <= 0.0 {
                self.pill_countdown = 0.0;
                self.fade_target = 0.0;
            }
            needs_redraw = true;
        }

        // Width animation (compact mode)
        if (self.render_w - self.content_pw).abs() > 1.0 {
            let step = (self.content_pw - self.render_w) * (self.frame_ms as f32 / WIDTH_ANIM_MS).min(1.0);
            self.render_w += step;
            needs_redraw = true;
        } else if self.content_pw > 0.0 {
            self.render_w = self.content_pw;
        }

        // Pulse animation for listening dot
        if self.listening {
            self.anim_phase += std::f32::consts::TAU * self.frame_ms as f32 / 1500.0;
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
        if self.listening {
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

    fn draw_mic_icon(&mut self, cx: f32, cy: f32, size: f32, color: Color) {
        let s = size / 12.0;
        let lw = 1.5 * s;
        let mut paint = Paint::color(color);
        paint.set_line_width(lw);
        paint.set_line_cap(femtovg::LineCap::Round);

        // Mic body capsule
        let mut body = Path::new();
        body.rounded_rect(cx - 3.0 * s, cy - 6.0 * s, 6.0 * s, 10.0 * s, 3.0 * s);
        self.canvas.stroke_path(&body, &paint);

        // U-shape stand
        let mut stand = Path::new();
        stand.move_to(cx - 7.0 * s, cy - 1.0 * s);
        stand.bezier_to(cx - 7.0 * s, cy + 6.5 * s, cx - 3.5 * s, cy + 9.0 * s, cx, cy + 9.0 * s);
        stand.bezier_to(cx + 3.5 * s, cy + 9.0 * s, cx + 7.0 * s, cy + 6.5 * s, cx + 7.0 * s, cy - 1.0 * s);
        self.canvas.stroke_path(&stand, &paint);

        // Stem
        let mut stem = Path::new();
        stem.move_to(cx, cy + 9.0 * s);
        stem.line_to(cx, cy + 12.0 * s);
        self.canvas.stroke_path(&stem, &paint);
    }

    fn draw_check_icon(&mut self, cx: f32, cy: f32, size: f32, color: Color) {
        let s = size / 12.0;
        let mut check = Path::new();
        check.move_to(cx - 8.0 * s, cy);
        check.line_to(cx - 3.0 * s, cy + 5.5 * s);
        check.line_to(cx + 8.0 * s, cy - 6.0 * s);
        let mut paint = Paint::color(color);
        paint.set_line_width(2.0 * s);
        paint.set_line_cap(femtovg::LineCap::Round);
        paint.set_line_join(femtovg::LineJoin::Round);
        self.canvas.stroke_path(&check, &paint);
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
        if self.listening {
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
        h.min(self.max_height)
    }

    fn resize_and_redraw(&mut self) {
        if !self.configured || self.width == 0 {
            return;
        }
        let h = self.compute_height();
        if h != self.height {
            self.height = h;
            self.layer.set_size(0, h);
        }
        self.redraw();
    }

    fn redraw(&mut self) {
        if self.width == 0 || !self.configured {
            return;
        }

        let s = self.scale;
        let sf = s as f32;
        let pw = (self.width * s as u32) as f32;
        let ph = (self.height * s as u32) as f32;

        // Resize EGL surface
        self.wl_egl_surface.resize(pw as i32, ph as i32, 0, 0);
        self.canvas.set_size(pw as u32, ph as u32, 1.0);
        self.canvas.clear_rect(0, 0, pw as u32, ph as u32, Color::rgbaf(0.0, 0.0, 0.0, 0.0));

        // Fully faded out — just clear and mark invisible
        if self.fade_alpha <= 0.01 && self.fade_target <= 0.0 {
            self.canvas.flush();
            self.egl_lib.swap_buffers(self.egl_display, self.egl_surface).ok();
            if self.visible {
                self.visible = false;
                self.height = PILL_SIZE;
                self.layer.set_size(0, PILL_SIZE);
            }
            return;
        }

        // Shrink animation ease
        let ease_t = if self.shrink_t > 0.0 {
            let t = self.shrink_t.min(1.0);
            1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t)
        } else {
            0.0
        };

        let bg_alpha = 0.8 * self.fade_alpha;
        let target_h = PILL_SIZE as f32 * sf;

        // Pill shrinks to a circle for both recording and copied icons
        let is_recording_pill = self.listening || self.shrink_target < 0.5;
        let target_w = target_h;

        // Background rect geometry
        let base_w = self.render_w.min(pw).max(target_h);
        let (rx, ry, rw, rh) = if ease_t > 0.0 {
            let rw = base_w + (target_w - base_w) * ease_t;
            let rh = ph + (target_h - ph) * ease_t;
            let rx = (pw - rw) / 2.0;
            let ry = ph - rh;
            (rx, ry, rw, rh)
        } else {
            let rx = (pw - base_w) / 2.0;
            (rx, 0.0, base_w, ph)
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
                Color::rgbaf(0.0, 0.0, 0.0, 0.3 * self.fade_alpha),
                Color::rgbaf(0.0, 0.0, 0.0, 0.0),
            );
            self.canvas.fill_path(&shadow_path, &shadow_paint);
        }

        // Background
        {
            let bg_path = make_bg_path(0.0);
            let bg_paint = Paint::color(Color::rgbaf(0.0, 0.0, 0.0, bg_alpha));
            self.canvas.fill_path(&bg_path, &bg_paint);

            // Border
            let mut border_paint = Paint::color(Color::rgbaf(1.0, 1.0, 1.0, 0.25 * self.fade_alpha));
            border_paint.set_line_width(sf);
            self.canvas.stroke_path(&bg_path, &border_paint);
        }

        // Text opacity
        let text_opacity = (1.0 - ease_t * 4.0).max(0.0);
        let text_alpha = self.fade_alpha * text_opacity;
        let pill_label_opacity = if ease_t > 0.5 { ((ease_t - 0.5) * 2.0).min(1.0) } else { 0.0 };
        let pill_alpha = self.fade_alpha * pill_label_opacity;

        let show_pill = (self.listening || self.pill_countdown > 0.0 || pill_alpha > 0.0) && ease_t > 0.5;

        // Pill icons: microphone for recording, checkmark for copied
        if show_pill {
            let cx = rx + rw / 2.0;
            let cy = ry + rh / 2.0;
            let icon_s = rh * 0.5;
            if is_recording_pill {
                let pulse = (self.anim_phase.sin() * 0.5 + 0.5) * 0.4 + 0.6;
                let a = pulse * pill_alpha;
                self.draw_mic_icon(cx, cy, icon_s, Color::rgbaf(0.88, 0.25, 0.25, a));
            } else {
                self.draw_check_icon(cx, cy, icon_s, Color::rgbaf(0.8, 0.8, 0.8, pill_alpha));
            }
        }

        // Normal transcript text
        if text_alpha > 0.01 && !show_pill {
            let font_sz = self.font_size * sf;
            let text_x = rx + PADDING_X * sf;

            self.rewrap_if_dirty();
            let display = self.display_text();
            let num_lines = self.wrapped_lines.len();
            let total_text_h = num_lines as f32 * self.line_height * sf;
            let visible_h = ph - PADDING_Y * sf * 2.0;
            let scroll_y = (total_text_h - visible_h).max(0.0);

            // Clip to background rect
            self.canvas.save();
            let mut clip = Path::new();
            clip.rounded_rect(rx, ry, rw, rh, r);
            self.canvas.intersect_scissor(rx, ry, rw, rh);

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
            let base_y = ph - PADDING_Y * sf - total_text_h + scroll_y;
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

                if y < -self.line_height * sf || y > ph + self.line_height * sf {
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
            self.max_height = PADDING_Y as u32 * 2 + max_lines * self.line_height as u32;
            self.layer.set_margin(0, margin_h, margin_b, margin_h);
            self.width = overlay_w;
        } else {
            self.width = w;
        }

        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
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

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
