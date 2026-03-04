use anyhow::Result;
use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache};
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
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols_plasma::blur::client::{
    org_kde_kwin_blur, org_kde_kwin_blur_manager,
};

const MIN_HEIGHT: u32 = 36;
const PADDING_X: i32 = 16;
const PADDING_Y: i32 = 8;
const OVERLAY_WIDTH_FRAC: f64 = 0.618;
const CHAR_WIDTH_RATIO: f32 = 0.47;
const CORNER_RADIUS: f32 = 12.0;
const FADE_DURATION_MS: f32 = 150.0;
const SHRINK_DURATION_MS: f32 = 150.0;
const WIDTH_ANIM_MS: f32 = 100.0;
const DOT_RADIUS: f32 = 4.0;

pub enum Command {
    Show,
    SetText(String),
    SetPending(String),
    Copied,
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
}

pub fn spawn(font: String) -> Result<Handle> {
    let (tx, rx) = calloop::channel::channel::<Command>();
    let handle = Handle { tx };

    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            if let Err(e) = run(rx, &font) {
                tracing::error!("overlay thread: {e}");
            }
        })?;

    Ok(handle)
}

fn blend_over(
    dst: tiny_skia::PremultipliedColorU8,
    src: tiny_skia::PremultipliedColorU8,
) -> tiny_skia::PremultipliedColorU8 {
    let sa = src.alpha() as u32;
    let inv = 255 - sa;
    tiny_skia::PremultipliedColorU8::from_rgba(
        ((src.red() as u32 * 255 + dst.red() as u32 * inv) / 255) as u8,
        ((src.green() as u32 * 255 + dst.green() as u32 * inv) / 255) as u8,
        ((src.blue() as u32 * 255 + dst.blue() as u32 * inv) / 255) as u8,
        ((sa * 255 + dst.alpha() as u32 * inv) / 255) as u8,
    )
    .unwrap()
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
    layer.set_size(0, MIN_HEIGHT);
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    // Request background blur from compositor (KDE blur protocol, supported by Hyprland)
    if let Ok(blur_mgr) = globals.bind::<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, _, _>(&qh, 1..=1, ()) {
        let blur = blur_mgr.create(layer.wl_surface(), &qh, ());
        blur.set_region(None); // blur entire surface
        blur.commit();
    }

    let pool = SlotPool::new(256, &shm)?;
    let mut font_system = FontSystem::new();
    let swash_cache = SwashCache::new();
    let text_buffer = Buffer::new(&mut font_system, Metrics::new(16.0, 24.0));

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        font_system,
        swash_cache,
        text_buffer,
        pixmap: None,
        pixmap_w: 0,
        pixmap_h: 0,
        font_name: font_name.to_string(),
        text: String::new(),
        pending: String::new(),
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
        height: MIN_HEIGHT,
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

    let anim_timer = calloop::timer::Timer::immediate();
    loop_handle
        .insert_source(anim_timer, |_, _, state| {
            let mut needs_redraw = false;

            // Fade animation
            if (state.fade_alpha - state.fade_target).abs() > 0.01 {
                let step = state.frame_ms as f32 / FADE_DURATION_MS;
                if state.fade_target > state.fade_alpha {
                    state.fade_alpha = (state.fade_alpha + step).min(1.0);
                } else {
                    state.fade_alpha = (state.fade_alpha - step).max(0.0);
                }
                needs_redraw = true;
            }

            // Pill ↔ full morph animation
            if (state.shrink_t - state.shrink_target).abs() > 0.01 {
                let step = state.frame_ms as f32 / SHRINK_DURATION_MS;
                if state.shrink_target > state.shrink_t {
                    state.shrink_t = (state.shrink_t + step).min(1.0);
                } else {
                    state.shrink_t = (state.shrink_t - step).max(0.0);
                }
                if state.shrink_t >= 1.0 && !state.listening {
                    state.pill_countdown = 0.6;
                }
                needs_redraw = true;
            }

            // "Copied" pill hold → fade
            if state.pill_countdown > 0.0 {
                state.pill_countdown -= state.frame_ms as f32 / 1000.0;
                if state.pill_countdown <= 0.0 {
                    state.pill_countdown = 0.0;
                    state.fade_target = 0.0;
                }
                needs_redraw = true;
            }

            // Width animation (compact mode)
            if (state.render_w - state.content_pw).abs() > 1.0 {
                let step = (state.content_pw - state.render_w) * (state.frame_ms as f32 / WIDTH_ANIM_MS).min(1.0);
                state.render_w += step;
                needs_redraw = true;
            } else if state.content_pw > 0.0 {
                state.render_w = state.content_pw;
            }

            // Pulse animation for listening dot, pending text
            if state.visible && (state.listening || !state.pending.is_empty()) {
                state.anim_phase += std::f32::consts::TAU * state.frame_ms as f32 / 1500.0;
                needs_redraw = true;
            }

            if needs_redraw {
                state.redraw();
            }
            calloop::timer::TimeoutAction::ToDuration(std::time::Duration::from_millis(state.frame_ms))
        })
        .map_err(|e| anyhow::anyhow!("anim timer: {e}"))?;

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
                    state.fade_alpha = 1.0;
                    state.fade_target = 1.0;
                    // Start render_w at pill size — will be computed properly in redraw
                    let pill_w = MIN_HEIGHT as f32 * state.scale as f32;
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
            }
        }
    }).map_err(|e| anyhow::anyhow!("cmd channel: {e}"))?;

    while !state.exit {
        event_loop.dispatch(std::time::Duration::from_millis(100), &mut state)?;
    }

    Ok(())
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    font_system: FontSystem,
    swash_cache: SwashCache,
    text_buffer: Buffer,
    pixmap: Option<tiny_skia::Pixmap>,
    pixmap_w: u32,
    pixmap_h: u32,
    font_name: String,
    text: String,
    pending: String,
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

    fn compute_height(&mut self) -> u32 {
        if self.listening {
            return MIN_HEIGHT;
        }
        let s = self.scale as f32;
        let display = self.display_text();
        let font_family = Family::Name(&self.font_name);
        let pw = self.width * self.scale as u32;

        self.text_buffer.set_metrics(
            &mut self.font_system,
            Metrics::new(self.font_size * s, self.line_height * s),
        );
        self.text_buffer.set_text(
            &mut self.font_system,
            &display,
            &Attrs::new().family(font_family),
            Shaping::Advanced,
        );
        self.text_buffer.set_size(
            &mut self.font_system,
            Some((pw as i32 - PADDING_X * self.scale * 2) as f32),
            None,
        );
        self.text_buffer
            .shape_until_scroll(&mut self.font_system, false);

        let lines = self.text_buffer.layout_runs().count().max(1) as u32;
        self.content_pw = if lines > 1 {
            pw as f32
        } else {
            let widest = self.text_buffer.layout_runs()
                .map(|run| run.line_w)
                .fold(0.0f32, f32::max);
            (widest + PADDING_X as f32 * s * 2.0)
                .max(MIN_HEIGHT as f32 * s)
                .min(pw as f32)
        };
        let h = PADDING_Y as u32 * 2 + lines * self.line_height as u32;
        h.max(MIN_HEIGHT).min(self.max_height)
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

        // Fully faded out — clear and mark invisible
        if self.fade_alpha <= 0.01 && self.fade_target <= 0.0 {
            if self.visible {
                self.visible = false;
                self.height = MIN_HEIGHT;
                self.layer.set_size(0, MIN_HEIGHT);

                let s = self.scale;
                let pw = self.width * s as u32;
                let ph = MIN_HEIGHT * s as u32;
                let stride = pw as i32 * 4;
                let (buffer, canvas) = self
                    .pool
                    .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888)
                    .expect("create buffer");
                canvas.fill(0);
                self.layer.wl_surface().set_buffer_scale(s);
                self.layer.wl_surface().damage_buffer(0, 0, pw as i32, ph as i32);
                buffer.attach_to(self.layer.wl_surface()).expect("attach");
                self.layer.commit();
            }
            return;
        }

        let s = self.scale;
        let sf = s as f32;
        let pw = self.width * s as u32;
        let ph = self.height * s as u32;
        let stride = pw as i32 * 4;

        let (buffer, canvas) = self
            .pool
            .create_buffer(pw as i32, ph as i32, stride, wl_shm::Format::Argb8888)
            .expect("create buffer");

        // Reuse pixmap if same size, otherwise allocate
        if self.pixmap_w != pw || self.pixmap_h != ph {
            self.pixmap = Some(tiny_skia::Pixmap::new(pw, ph).expect("pixmap"));
            self.pixmap_w = pw;
            self.pixmap_h = ph;
        }
        let pixmap = self.pixmap.as_mut().unwrap();
        pixmap.data_mut().fill(0);

        // Background rect — shrinks to a circle during copied animation
        let ease_t = if self.shrink_t > 0.0 {
            let t = self.shrink_t.min(1.0);
            1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t) // cubic ease-out
        } else {
            0.0
        };

        let bg_alpha = (0x99 as f32 * self.fade_alpha) as u8;
        let target_h = MIN_HEIGHT as f32 * sf;
        // Measure pill label width via layout
        let pill_label = if self.listening || self.shrink_target < 0.5 {
            "Recording"
        } else {
            "Copied"
        };
        let target_w = {
            let font_family = Family::Name(&self.font_name);
            self.text_buffer.set_metrics(
                &mut self.font_system,
                Metrics::new(self.font_size * sf, self.line_height * sf),
            );
            self.text_buffer.set_text(
                &mut self.font_system,
                pill_label,
                &Attrs::new().family(font_family),
                Shaping::Advanced,
            );
            self.text_buffer.set_size(&mut self.font_system, None, None);
            self.text_buffer.shape_until_scroll(&mut self.font_system, false);
            let w = self.text_buffer.layout_runs()
                .map(|run| run.line_w)
                .fold(0.0f32, f32::max);
            // Extra space for red dot indicator when recording
            let dot_space = if self.listening || self.shrink_target < 0.5 {
                DOT_RADIUS * sf * 2.0 + 8.0 * sf
            } else {
                0.0
            };
            w + dot_space + PADDING_X as f32 * sf * 2.0
        }.max(target_h);

        let base_w = self.render_w.min(pw as f32).max(target_h);
        let (rx, ry, rw, rh) = if ease_t > 0.0 {
            let rw = base_w + (target_w - base_w) * ease_t;
            let rh = ph as f32 + (target_h - ph as f32) * ease_t;
            let rx = (pw as f32 - rw) / 2.0;
            let ry = ph as f32 - rh;
            (rx, ry, rw, rh)
        } else {
            let rx = (pw as f32 - base_w) / 2.0;
            (rx, 0.0, base_w, ph as f32)
        };

        let r_top = if ease_t > 0.0 {
            CORNER_RADIUS * sf + (rh / 2.0 - CORNER_RADIUS * sf) * ease_t
        } else {
            CORNER_RADIUS * sf
        };
        let r_bot = rh / 2.0 * ease_t; // 0 at start, fully rounded at end

        let rrect = {
            let mut pb = tiny_skia::PathBuilder::new();
            pb.move_to(rx + r_top, ry);
            pb.line_to(rx + rw - r_top, ry);
            pb.quad_to(rx + rw, ry, rx + rw, ry + r_top);
            pb.line_to(rx + rw, ry + rh - r_bot);
            pb.quad_to(rx + rw, ry + rh, rx + rw - r_bot, ry + rh);
            pb.line_to(rx + r_bot, ry + rh);
            pb.quad_to(rx, ry + rh, rx, ry + rh - r_bot);
            pb.line_to(rx, ry + r_top);
            pb.quad_to(rx, ry, rx + r_top, ry);
            pb.close();
            pb.finish().unwrap()
        };
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba8(0x00, 0x00, 0x00, bg_alpha));
        paint.anti_alias = true;
        pixmap.fill_path(&rrect, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);

        let mut border_paint = tiny_skia::Paint::default();
        border_paint.set_color(tiny_skia::Color::from_rgba8(0xFF, 0xFF, 0xFF, (0x15 as f32 * self.fade_alpha) as u8));
        border_paint.anti_alias = true;
        let mut stroke = tiny_skia::Stroke::default();
        stroke.width = sf;
        pixmap.stroke_path(&rrect, &border_paint, &stroke, tiny_skia::Transform::identity(), None);

        // Text fades based on how pill-like the rect is
        let text_opacity = (1.0 - ease_t * 4.0).max(0.0);
        let text_alpha = (0xFF as f32 * self.fade_alpha * text_opacity) as u8;
        // Pill label (Recording/Copied) visible when rect is pill-shaped
        let pill_label_opacity = if ease_t > 0.5 { ((ease_t - 0.5) * 2.0).min(1.0) } else { 0.0 };
        let pill_alpha = (0xFF as f32 * self.fade_alpha * pill_label_opacity) as u8;

        let show_pill = (self.listening || self.pill_countdown > 0.0 || pill_alpha > 0) && ease_t > 0.5;

        if show_pill {
            let dot_r = DOT_RADIUS * sf;
            let is_recording = self.listening || self.shrink_target < 0.5;
            let label = if is_recording { "Recording" } else { "Copied" };
            let label_color = if is_recording {
                Color::rgba(0x99, 0x99, 0x99, pill_alpha)
            } else {
                Color::rgba(0xCC, 0xCC, 0xCC, pill_alpha)
            };

            // Red dot for recording
            if is_recording {
                let pulse = (self.anim_phase.sin() * 0.5 + 0.5) * 0.4 + 0.6;
                let dot_alpha = (pulse * pill_alpha as f32) as u8;
                let ind_x = rx + PADDING_X as f32 * sf + dot_r;
                let ind_y = ry + rh / 2.0;
                let dot_path = {
                    let mut pb = tiny_skia::PathBuilder::new();
                    pb.push_circle(ind_x, ind_y, dot_r);
                    pb.finish().unwrap()
                };
                let mut dot_paint = tiny_skia::Paint::default();
                dot_paint.set_color(tiny_skia::Color::from_rgba8(0xE0, 0x40, 0x40, dot_alpha));
                dot_paint.anti_alias = true;
                pixmap.fill_path(&dot_path, &dot_paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
            }

            let font_family = Family::Name(&self.font_name);
            self.text_buffer.set_metrics(
                &mut self.font_system,
                Metrics::new(self.font_size * sf, self.line_height * sf),
            );
            self.text_buffer.set_text(
                &mut self.font_system,
                label,
                &Attrs::new().family(font_family),
                Shaping::Advanced,
            );
            let dot_offset = if is_recording { dot_r * 2.0 + 8.0 * sf } else { 0.0 };
            self.text_buffer.set_size(
                &mut self.font_system,
                None,
                None,
            );
            self.text_buffer
                .shape_until_scroll(&mut self.font_system, false);

            let cw = pw as i32;
            let ch = ph as i32;
            let text_x = rx as i32 + (PADDING_X as f32 * sf + dot_offset) as i32;
            let text_y = ry as i32 + ((rh - self.line_height * sf) / 2.0) as i32;
            let pixels = pixmap.pixels_mut();

            self.text_buffer.draw(
                &mut self.font_system,
                &mut self.swash_cache,
                label_color,
                |x, y, w, h, color| {
                    let x = x + text_x;
                    let y = y + text_y;
                    let a = color.a();
                    if a == 0 { return; }
                    let a32 = a as u32;
                    let src = tiny_skia::PremultipliedColorU8::from_rgba(
                        ((color.r() as u32 * a32) / 255) as u8,
                        ((color.g() as u32 * a32) / 255) as u8,
                        ((color.b() as u32 * a32) / 255) as u8,
                        a,
                    ).unwrap();
                    for row in y..(y + h as i32).min(ch) {
                        if row < 0 { continue; }
                        for col in x..(x + w as i32).min(cw) {
                            if col < 0 { continue; }
                            let idx = (row * cw + col) as usize;
                            if idx < pixels.len() {
                                pixels[idx] = blend_over(pixels[idx], src);
                            }
                        }
                    }
                },
            );
        }

        if text_alpha > 0 && !show_pill {
            // Normal text rendering
            let final_text = self.text.clone();
            let pending_str = if self.pending.is_empty() {
                String::new()
            } else if self.text.is_empty() || self.text.ends_with(' ') {
                self.pending.clone()
            } else {
                format!(" {}", self.pending)
            };
            let font_family = Family::Name(&self.font_name);

            self.text_buffer.set_metrics(
                &mut self.font_system,
                Metrics::new(self.font_size * sf, self.line_height * sf),
            );

            // Pending text: softer opacity pulse (30-60%)
            let pending_alpha = if self.pending.is_empty() {
                0
            } else {
                let pulse = self.anim_phase.sin() * 0.5 + 0.5;
                ((0.3 + pulse * 0.3) * text_alpha as f32) as u8
            };

            self.text_buffer.set_rich_text(
                &mut self.font_system,
                [
                    (final_text.as_str(), Attrs::new().family(font_family).color(
                        Color::rgba(0xFF, 0xFF, 0xFF, text_alpha)
                    )),
                    (pending_str.as_str(), Attrs::new().family(font_family).color(
                        Color::rgba(0x88, 0x88, 0xAA, pending_alpha)
                    )),
                ],
                &Attrs::new().family(font_family),
                Shaping::Advanced,
                None,
            );
            self.text_buffer.set_size(
                &mut self.font_system,
                Some((pw as i32 - PADDING_X * s * 2) as f32),
                None,
            );
            self.text_buffer
                .shape_until_scroll(&mut self.font_system, false);

            let total_lines = self.text_buffer.layout_runs().count() as i32;
            let total_text_h = total_lines as f32 * self.line_height * sf;
            let visible_h = ph as f32 - (PADDING_Y * s * 2) as f32;
            let scroll_y = (total_text_h - visible_h).max(0.0) as i32;

            let cw = pw as i32;
            let ch = ph as i32;
            let pad_x = rx as i32 + PADDING_X * s;
            let text_h = total_text_h.min(visible_h) as i32;
            let pad_y = (ch - text_h) - PADDING_Y * s;
            let pixels = pixmap.pixels_mut();

            self.text_buffer.draw(
                &mut self.font_system,
                &mut self.swash_cache,
                Color::rgba(0xFF, 0xFF, 0xFF, text_alpha),
                |x, y, w, h, color| {
                    let x = x + pad_x;
                    let y = y + pad_y - scroll_y;
                    let a = color.a();
                    if a == 0 { return; }
                    let a32 = a as u32;
                    let src = tiny_skia::PremultipliedColorU8::from_rgba(
                        ((color.r() as u32 * a32) / 255) as u8,
                        ((color.g() as u32 * a32) / 255) as u8,
                        ((color.b() as u32 * a32) / 255) as u8,
                        a,
                    ).unwrap();
                    for row in y..(y + h as i32).min(ch) {
                        if row < 0 { continue; }
                        for col in x..(x + w as i32).min(cw) {
                            if col < 0 { continue; }
                            let idx = (row * cw + col) as usize;
                            if idx < pixels.len() {
                                pixels[idx] = blend_over(pixels[idx], src);
                            }
                        }
                    }
                },
            );
        }

        // Copy pixmap (RGBA) to canvas (ARGB)
        let pixmap_data = pixmap.data();
        for (chunk, rgba) in canvas.chunks_exact_mut(4).zip(pixmap_data.chunks_exact(4)) {
            chunk[0] = rgba[2]; // B
            chunk[1] = rgba[1]; // G
            chunk[2] = rgba[0]; // R
            chunk[3] = rgba[3]; // A
        }

        self.layer.wl_surface().set_buffer_scale(s);
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, pw as i32, ph as i32);
        buffer.attach_to(self.layer.wl_surface()).expect("attach");
        self.layer.commit();
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
            let content_w = overlay_w as f32 - PADDING_X as f32 * 2.0;
            self.font_size = content_w / (100.0 * CHAR_WIDTH_RATIO);
            self.line_height = self.font_size * 1.5;
            self.max_height = (overlay_w as f64 * OVERLAY_WIDTH_FRAC) as u32;
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

impl Dispatch<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, ()> for State {
    fn event(_: &mut Self, _: &org_kde_kwin_blur_manager::OrgKdeKwinBlurManager, _: <org_kde_kwin_blur_manager::OrgKdeKwinBlurManager as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<org_kde_kwin_blur::OrgKdeKwinBlur, ()> for State {
    fn event(_: &mut Self, _: &org_kde_kwin_blur::OrgKdeKwinBlur, _: <org_kde_kwin_blur::OrgKdeKwinBlur as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
