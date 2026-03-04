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
    Connection, QueueHandle,
};

const MIN_HEIGHT: u32 = 36;
const PADDING_X: i32 = 16;
const PADDING_Y: i32 = 8;
const OVERLAY_WIDTH_FRAC: f64 = 0.618;
const CHAR_WIDTH_RATIO: f32 = 0.47;
const CORNER_RADIUS: f32 = 12.0;
const FADE_DURATION_MS: f32 = 150.0;
const DOT_RADIUS: f32 = 4.0;

pub enum Command {
    Show,
    Hide,
    SetText(String),
    SetPending(String),
}

#[derive(Clone)]
pub struct Handle {
    tx: calloop::channel::Sender<Command>,
}

impl Handle {
    pub fn show(&self) {
        let _ = self.tx.send(Command::Show);
    }

    pub fn hide(&self) {
        let _ = self.tx.send(Command::Hide);
    }

    pub fn set_text(&self, text: String) {
        let _ = self.tx.send(Command::SetText(text));
    }

    pub fn set_pending(&self, text: String) {
        let _ = self.tx.send(Command::SetPending(text));
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
        font_name: font_name.to_string(),
        text: String::new(),
        pending: String::new(),
        listening: false,
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

            // Pulse animation for pending text and listening dot
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
                    state.text.clear();
                    state.pending.clear();
                    state.fade_target = 1.0;
                    state.resize_and_redraw();
                }
                Command::Hide => {
                    state.listening = false;
                    state.fade_target = 0.0;
                }
                Command::SetText(text) => {
                    state.listening = false;
                    state.text = text;
                    state.pending.clear();
                    if state.visible {
                        state.resize_and_redraw();
                    }
                }
                Command::SetPending(text) => {
                    state.listening = false;
                    state.pending = text;
                    if state.visible {
                        state.resize_and_redraw();
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
    font_name: String,
    text: String,
    pending: String,
    listening: bool,
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

        let mut pixmap = tiny_skia::Pixmap::new(pw, ph).expect("pixmap");

        // Background with rounded corners and fade alpha
        let bg_alpha = (0xB0 as f32 * self.fade_alpha) as u8;
        let r = CORNER_RADIUS * sf;
        let rrect = {
            let mut pb = tiny_skia::PathBuilder::new();
            // Top-left
            pb.move_to(r, 0.0);
            pb.line_to(pw as f32 - r, 0.0);
            pb.quad_to(pw as f32, 0.0, pw as f32, r);
            pb.line_to(pw as f32, ph as f32);
            pb.line_to(0.0, ph as f32);
            pb.line_to(0.0, r);
            pb.quad_to(0.0, 0.0, r, 0.0);
            pb.close();
            pb.finish().unwrap()
        };
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba8(0x1a, 0x1a, 0x2e, bg_alpha));
        paint.anti_alias = true;
        pixmap.fill_path(&rrect, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);

        // Subtle top border
        let mut border_paint = tiny_skia::Paint::default();
        border_paint.set_color(tiny_skia::Color::from_rgba8(0xFF, 0xFF, 0xFF, (0x15 as f32 * self.fade_alpha) as u8));
        border_paint.anti_alias = true;
        let mut stroke = tiny_skia::Stroke::default();
        stroke.width = sf;
        pixmap.stroke_path(&rrect, &border_paint, &stroke, tiny_skia::Transform::identity(), None);

        let text_alpha = (0xFF as f32 * self.fade_alpha) as u8;

        if self.listening {
            // Pulsing red dot + "Listening..."
            let dot_r = DOT_RADIUS * sf;
            let dot_x = PADDING_X as f32 * sf + dot_r;
            let dot_y = ph as f32 / 2.0;
            let pulse = (self.anim_phase.sin() * 0.5 + 0.5) * 0.4 + 0.6; // 0.6 - 1.0
            let dot_alpha = (pulse * text_alpha as f32) as u8;

            let dot_path = {
                let mut pb = tiny_skia::PathBuilder::new();
                pb.push_circle(dot_x, dot_y, dot_r);
                pb.finish().unwrap()
            };
            let mut dot_paint = tiny_skia::Paint::default();
            dot_paint.set_color(tiny_skia::Color::from_rgba8(0xE0, 0x40, 0x40, dot_alpha));
            dot_paint.anti_alias = true;
            pixmap.fill_path(&dot_path, &dot_paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);

            // "Listening..." text
            let font_family = Family::Name(&self.font_name);
            self.text_buffer.set_metrics(
                &mut self.font_system,
                Metrics::new(self.font_size * sf, self.line_height * sf),
            );
            self.text_buffer.set_text(
                &mut self.font_system,
                "Listening...",
                &Attrs::new().family(font_family),
                Shaping::Advanced,
            );
            self.text_buffer.set_size(
                &mut self.font_system,
                Some((pw as i32 - PADDING_X * s * 2) as f32 - dot_r * 2.0 - 8.0 * sf),
                None,
            );
            self.text_buffer
                .shape_until_scroll(&mut self.font_system, false);

            let cw = pw as i32;
            let ch = ph as i32;
            let text_offset_x = PADDING_X * s + (dot_r * 2.0 + 8.0 * sf) as i32;
            let pad_y = (ch - self.line_height as i32 * s) / 2;
            let pixels = pixmap.pixels_mut();

            self.text_buffer.draw(
                &mut self.font_system,
                &mut self.swash_cache,
                Color::rgba(0x99, 0x99, 0x99, text_alpha),
                |x, y, w, h, color| {
                    let x = x + text_offset_x;
                    let y = y + pad_y;
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
        } else {
            // Normal text rendering
            let final_text = self.text.clone();
            let pending_str = if self.pending.is_empty() {
                String::new()
            } else if self.text.is_empty() {
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
            let pad_x = PADDING_X * s;
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

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}
