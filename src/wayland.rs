// Gaxibo: the Wayland surface every sink draws into.
// Licensed under the GNU AGPL, version 3 or later.

//! One toplevel, shared by the page and by every clip.
//!
//! This exists so video can be shown **in a region** rather than only
//! full-screen.  Before it, each `waylandsink` created its own toplevel, cage
//! showed exactly one of them, and the only way to put a clip on screen was to
//! pause the page -- so a layout with a video beside anything else could not be
//! rendered at all.
//!
//! ## Why this does not reintroduce the copy
//!
//! The measured constraint in `PLAN.md` is that nothing may sit between
//! `v4l2slh264dec` and `waylandsink`, because a `compositor`,
//! `input-selector` or `videoconvert` copies 1920x1024 at 60fps through system
//! memory and that copy alone drops frames.  That is a constraint on
//! compositing **inside the pipeline, on the CPU**.
//!
//! Compositing outside it is free by comparison, and -- measured on the player
//! on 2026-09-02 -- was already happening: with a clip playing full-screen the
//! scanned-out framebuffer is `allocated by = cage`, so wlroots was already
//! importing the decoder's dmabuf, sampling it on the Mali, and compositing it
//! into its own buffer, at 0.27 of 6 cores with zero dropped buffers.  There
//! never was a direct-scanout path to protect.  Handing the sinks a shared
//! surface therefore costs one more layer in a composite that happens anyway.
//!
//! ## The shape
//!
//! ```text
//! xdg_toplevel  <- ours: one opaque black buffer, committed once
//!   +-- area_surface + video_surface   <- created by the page's sink
//!   +-- area_surface + video_surface   <- created by a clip's sink, above
//! ```
//!
//! We create **no subsurfaces ourselves**.  `waylandsink`, given a foreign
//! surface, makes its own pair inside it and positions them from the render
//! rectangle, so a sink handed our surface plus a rectangle lands exactly where
//! a layout wants it.  Sibling subsurfaces stack in creation order, so the page
//! sink -- built first, at startup -- ends up underneath the clips.
//!
//! The toplevel needs a buffer of its own: a subsurface is only shown once its
//! parent is mapped, and a parent is mapped by having a buffer.  Ours is
//! full-size, zero-filled `XRGB8888`, which is opaque black -- the same black
//! the output contract wants wherever a design does not reach -- and it is
//! attached once and never touched again.  It is also declared opaque, so
//! wlroots can skip whatever it covers.
//!
//! ## The trap that cost the design a rewrite
//!
//! `wayland-client` must use the **system backend**, sharing libwayland-client
//! with GStreamer.  With the pure-Rust backend the pointer out of
//! `ObjectId::as_ptr()` is not a `wl_surface*` the sink's C code can use, and
//! nothing says so: the sink would take the pointer and misbehave later.

use std::fs::OpenOptions;
use std::os::fd::AsFd;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_region::WlRegion,
    wl_registry,
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

/// The globals we need, plus what the dispatch thread has to answer.
struct State {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    /// Held so a later configure can be acknowledged and committed.
    surface: Option<WlSurface>,
    closed: bool,
}

/// The surface the sinks share.
///
/// `Send + Sync` comes for free -- a `Connection` and a proxy are both -- which
/// is what allows a clip's sink to be placed from the bridge thread while the
/// dispatch thread is blocked reading events. That is also the pattern the C
/// embedding relies on: libwayland is thread-safe for one display dispatched
/// through several queues, and GStreamer's sinks make their own.
pub struct Wayland {
    conn: Connection,
    surface: WlSurface,
}

impl Wayland {
    /// Connects, maps one fullscreen toplevel, and leaves a thread dispatching.
    pub fn new(width: i32, height: i32) -> Result<Arc<Self>> {
        let conn = Connection::connect_to_env()
            .context("connecting to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut state = State {
            compositor: None,
            shm: None,
            wm_base: None,
            surface: None,
            closed: false,
        };
        // Two round trips: the first delivers the globals, the second the
        // events from binding them.
        queue.roundtrip(&mut state).context("the initial Wayland round trip")?;
        queue.roundtrip(&mut state).context("the second Wayland round trip")?;

        let compositor = state.compositor.clone().ok_or_else(|| anyhow!("no wl_compositor"))?;
        let shm = state.shm.clone().ok_or_else(|| anyhow!("no wl_shm"))?;
        let wm_base = state.wm_base.clone().ok_or_else(|| anyhow!("no xdg_wm_base"))?;

        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title("Gaxibo".into());
        toplevel.set_app_id("gaxibo".into());
        surface.commit();

        // Nothing may be asked of the toplevel until the compositor has
        // configured it and the configure has been acknowledged. Requesting
        // fullscreen before this first round trip is what makes wlroots log
        // "a configure is scheduled for an uninitialized xdg_surface" -- which
        // is harmless, and worth not printing, because an error nobody acts on
        // teaches everyone to skip the errors that matter.
        state.surface = Some(surface.clone());
        queue.roundtrip(&mut state).context("waiting for the first xdg configure")?;

        // cage fullscreens whatever it is given, but saying so means the same
        // binary behaves the same under a compositor that does not.
        toplevel.set_fullscreen(None);

        let buffer = black_buffer(&shm, &qh, width, height)
            .context("creating the background buffer")?;
        let region = compositor.create_region(&qh, ());
        region.add(0, 0, width, height);
        surface.set_opaque_region(Some(&region));
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        queue.roundtrip(&mut state).context("committing the background buffer")?;

        log::info!("wayland: one {width}x{height} toplevel, shared by every sink");

        std::thread::Builder::new()
            .name("wayland".into())
            .spawn(move || loop {
                if let Err(e) = queue.blocking_dispatch(&mut state) {
                    log::error!("wayland: the connection failed: {e}");
                    return;
                }
                if state.closed {
                    log::error!("wayland: the compositor closed our toplevel");
                    return;
                }
            })
            .context("starting the Wayland dispatch thread")?;

        Ok(Arc::new(Wayland { conn, surface }))
    }

    /// The `wl_surface*` a sink is given as its window handle.
    pub fn surface_handle(&self) -> usize {
        self.surface.id().as_ptr() as usize
    }

    /// The `wl_display*` a sink is given through its `GstContext`.
    pub fn display_handle(&self) -> *mut std::ffi::c_void {
        self.conn.backend().display_ptr() as *mut std::ffi::c_void
    }
}

/// A full-size opaque black buffer for the toplevel.
///
/// Zero-filled `XRGB8888` *is* opaque black, so the mapping is never written
/// to: the file is created, sized, unlinked, and handed over. It lives in
/// `/dev/shm` because that is the one place guaranteed to be a tmpfs, which
/// `wl_shm` requires.
fn black_buffer(
    shm: &WlShm,
    qh: &QueueHandle<State>,
    width: i32,
    height: i32,
) -> Result<WlBuffer> {
    let stride = width * 4;
    let len = (stride * height) as u64;
    let path = format!("/dev/shm/gaxibo-bg-{}", std::process::id());
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {path}"))?;
    // Unlink immediately: the fd keeps it alive, and a crash then leaves
    // nothing behind in /dev/shm to collide with the next start.
    std::fs::remove_file(&path).with_context(|| format!("unlinking {path}"))?;
    file.set_len(len).context("sizing the background buffer")?;

    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(0, width, height, stride, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    Ok(buffer)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match &interface[..] {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Unanswered, the compositor is entitled to consider us hung.
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            // An acknowledgement only takes effect on the next commit.
            if let Some(surface) = &state.surface {
                surface.commit();
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The size is not honoured: the surface is the size of the output
            // by contract, and a player that resized itself would break the
            // 1:1 mapping the whole pipeline exists to keep.
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if width > 0 && height > 0 {
                    log::debug!("wayland: compositor configured {width}x{height}");
                }
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

delegate_noop!(State: WlCompositor);
delegate_noop!(State: WlRegion);
delegate_noop!(State: WlShmPool);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlBuffer);
