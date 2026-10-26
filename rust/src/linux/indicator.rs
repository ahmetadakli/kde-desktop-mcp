//! The on-screen indicator, run as `desktop-mcp --indicator` by the server:
//! while the agent acts, the screen edges glow, a pill at the top says what
//! it does, and a ring marks each pointer target. Every surface is a
//! layer-shell overlay with an empty input region, so clicks pass through.
//!
//! Commands arrive on stdin, one per line:
//! - `show <label>`: glow and pill ("Claude · <label>")
//! - `point <x> <y>`: ring at that logical screen point
//! - `hide`: remove everything now; answers `hidden` on stdout once the
//!   compositor has processed it, so a screenshot taken next is clean
//!
//! Glow and pill go away 2.5 s after the last command, the ring after 0.7 s;
//! the process exits when stdin closes or after ten idle minutes.

use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    output::{OutputHandler, OutputState},
    reexports::client::{
        Connection, QueueHandle,
        globals::registry_queue_init,
        protocol::{wl_output, wl_shm, wl_surface},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};

use super::indicator_draw::{self as draw, Image, RING_SIZE};
use crate::backend::R;

const LINGER: Duration = Duration::from_millis(2500);
const RING_LINGER: Duration = Duration::from_millis(700);
const IDLE_EXIT: Duration = Duration::from_secs(600);
/// Buffer px per logical px for the pill and ring, so text stays sharp on
/// scaled outputs (the compositor scales it down).
const SHARP: u32 = 2;

enum Cmd {
    Show(String),
    Point(i32, i32),
    Hide,
}

fn parse(line: &str) -> Option<Cmd> {
    let (word, rest) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    match word {
        "show" => Some(Cmd::Show(rest.to_string())),
        "point" => {
            let mut n = rest.split_whitespace().map(|v| v.parse::<i32>());
            Some(Cmd::Point(n.next()?.ok()?, n.next()?.ok()?))
        }
        "hide" => Some(Cmd::Hide),
        _ => None,
    }
}

/// One overlay surface and the image it should show once configured.
struct Part {
    layer: LayerSurface,
    image: Image,
    /// Buffer px per logical px of `image`; 0 = size taken from configure.
    scale: u32,
    /// Kept until the compositor is done with it.
    buffer: Option<Buffer>,
}

struct State {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    font: Option<fontdue::Font>,
    glow: Option<Part>,
    pill: Option<(String, Part)>,
    ring: Option<Part>,
}

impl State {
    fn surface(&self, qh: &QueueHandle<Self>, name: &str) -> LayerSurface {
        let surface = self.compositor.create_surface(qh);
        // An empty input region: pointer events go to what is underneath.
        if let Ok(region) = Region::new(&self.compositor) {
            surface.set_input_region(Some(region.wl_region()));
        }
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some(format!("desktop-mcp-{name}")),
            None,
        );
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_exclusive_zone(-1);
        layer
    }

    fn show(&mut self, qh: &QueueHandle<Self>, label: &str) {
        if self.glow.is_none() {
            let layer = self.surface(qh, "glow");
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            layer.set_size(0, 0);
            layer.commit();
            self.glow = Some(Part {
                layer,
                image: Image {
                    width: 0,
                    height: 0,
                    data: Vec::new(),
                },
                scale: 0,
                buffer: None,
            });
        }
        let text = format!("Claude · {label}");
        if self.pill.as_ref().is_some_and(|(t, _)| *t == text) {
            return;
        }
        let image = draw::pill(&text, self.font.as_ref(), SHARP);
        let layer = self.surface(qh, "pill");
        layer.set_anchor(Anchor::TOP);
        layer.set_margin(12, 0, 0, 0);
        layer.set_size(image.width / SHARP, image.height / SHARP);
        layer.commit();
        self.pill = Some((
            text,
            Part {
                layer,
                image,
                scale: SHARP,
                buffer: None,
            },
        ));
    }

    fn point(&mut self, qh: &QueueHandle<Self>, x: i32, y: i32) {
        let half = (RING_SIZE / 2) as i32;
        let layer = self.surface(qh, "ring");
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_margin(y - half, 0, 0, x - half);
        layer.set_size(RING_SIZE, RING_SIZE);
        layer.commit();
        self.ring = Some(Part {
            layer,
            image: draw::ring(SHARP),
            scale: SHARP,
            buffer: None,
        });
    }

    fn paint(pool: &mut SlotPool, part: &mut Part) {
        let img = &part.image;
        if img.width == 0 || img.height == 0 {
            return;
        }
        let stride = img.width as i32 * 4;
        let Ok((buffer, canvas)) = pool.create_buffer(
            img.width as i32,
            img.height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            return;
        };
        canvas[..img.data.len()].copy_from_slice(&img.data);
        let surface = part.layer.wl_surface();
        surface.set_buffer_scale(part.scale.max(1) as i32);
        surface.damage_buffer(0, 0, img.width as i32, img.height as i32);
        if buffer.attach_to(surface).is_ok() {
            part.layer.commit();
            part.buffer = Some(buffer);
        }
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.glow.as_ref().is_some_and(|p| &p.layer == layer) {
            self.glow = None;
        }
        if self.pill.as_ref().is_some_and(|(_, p)| &p.layer == layer) {
            self.pill = None;
        }
        if self.ring.as_ref().is_some_and(|p| &p.layer == layer) {
            self.ring = None;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let pool = &mut self.pool;
        if let Some(p) = self.glow.as_mut().filter(|p| &p.layer == layer) {
            let (w, h) = configure.new_size;
            if (p.image.width, p.image.height) != (w, h) && w > 0 && h > 0 {
                p.image = draw::glow(w, h);
                p.scale = 1;
            }
            Self::paint(pool, p);
        } else if let Some((_, p)) = self.pill.as_mut().filter(|(_, p)| &p.layer == layer) {
            Self::paint(pool, p);
        } else if let Some(p) = self.ring.as_mut().filter(|p| &p.layer == layer) {
            Self::paint(pool, p);
        }
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_registry!(State);
smithay_client_toolkit::delegate_dispatch2!(State);

/// A bold sans-serif font from fontconfig, if there is one.
fn load_font() -> Option<fontdue::Font> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:bold"])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let bytes = std::fs::read(path.trim()).ok()?;
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).ok()
}

pub fn run() -> R<()> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).map_err(|e| format!("no layer-shell support: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let pool = SlotPool::new(1 << 20, &shm).map_err(|e| e.to_string())?;
    let mut state = State {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        font: load_font(),
        glow: None,
        pill: None,
        ring: None,
    };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Some(cmd) = parse(&line)
                && tx.send(cmd).is_err()
            {
                break;
            }
        }
    });

    let (mut last, mut ring_at) = (Instant::now(), Instant::now());
    loop {
        let mut changed = false;
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => {
                last = Instant::now();
                changed = true;
                match cmd {
                    Cmd::Show(label) => state.show(&qh, &label),
                    Cmd::Point(x, y) => {
                        state.point(&qh, x, y);
                        ring_at = Instant::now();
                    }
                    Cmd::Hide => {
                        state.glow = None;
                        state.pill = None;
                        state.ring = None;
                        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "hidden");
                        let _ = out.flush();
                        continue;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if state.ring.is_some() && ring_at.elapsed() >= RING_LINGER {
            state.ring = None;
            changed = true;
        }
        if (state.glow.is_some() || state.pill.is_some()) && last.elapsed() >= LINGER {
            state.glow = None;
            state.pill = None;
            changed = true;
        }
        if last.elapsed() >= IDLE_EXIT {
            return Ok(());
        }
        if changed {
            queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert!(matches!(parse("show typing"), Some(Cmd::Show(l)) if l == "typing"));
        assert!(
            matches!(parse("show pressing ctrl+s"), Some(Cmd::Show(l)) if l == "pressing ctrl+s")
        );
        assert!(matches!(parse("point 10 -4"), Some(Cmd::Point(10, -4))));
        assert!(matches!(parse("hide"), Some(Cmd::Hide)));
        assert!(parse("point 10").is_none());
        assert!(parse("dance").is_none());
    }
}
