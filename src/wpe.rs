// Gaxibo: the GStreamer renderer.
// Licensed under the GNU AGPL, version 3 or later.

//! Renders layouts through GStreamer instead of QtWebEngine.
//!
//! The point is video: QtWebEngine reaches neither VA-API nor V4L2 on RK3399
//! and there is no VA-API driver for Rockchip, so the hardware decoder cannot
//! be used from inside the browser.  Measured on the player, one 1920x1024
//! 60fps 30Mbps clip costs **4.24 of 6 cores** and drops frames visibly, while
//! the same clip through `v4l2slh264dec` costs **0.27**.
//!
//! HTML still renders: `wpevideosrc` wraps WPE WebKit as a GStreamer source,
//! and it renders Arexibo's own generated layouts correctly -- verified
//! including the CMS's 2.3MB JS bundle, jQuery, Handlebars widget templates,
//! iframes and webfonts.
//!
//! This module deliberately runs **beside** the Qt renderer rather than
//! replacing it, selected by `--renderer`.  Qt stays the default, so a
//! regression here is one flag away from being ruled out instead of needing a
//! bisect.
//!
//! What is not done yet, and will report itself rather than fail silently:
//!   - video widgets still render inside WPE, so the VPU is not yet used.
//!     That is the next step and the whole point; this module is the vehicle.
//!   - screenshots.  The Qt path renders the webview; here it needs a `tee`
//!     into `pngenc`.  The CMS asks for them, so this is a real gap.
//!   - audio.  `wpevideosrc` exposes audio pads and Arexibo has an `audio`
//!     widget type; nothing routes them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use gstreamer as gst;
use gst::glib;
use gst::prelude::*;
use gstreamer_video as gst_video;
use gst_video::prelude::*;
use parking_lot::Mutex;

use crate::bridge::BridgeMsg;
use crate::config::PlayerSettings;
use crate::gui::Schedule;
use crate::mainloop::{FromGui, Kill, ToGui};
use crate::resource::LayoutId;
use crate::wayland::Wayland;

/// The splash layout, served built-in by the embedded webserver.
const SPLASH: &str = "0.xlf.html";

/// Frame rate asked of WPE for the page.
///
/// Not 60. A signage page is static almost all of the time, and 1920x1080
/// BGRA at 60fps is around 500 MB/s of memory writes for identical frames --
/// bandwidth contended with the VPU and the display controller. 15 covers the
/// animation a Xibo layout actually does.
const PAGE_FPS: i32 = 15;

/// A pipeline per surface, sharing one toplevel.
///
/// This is the whole architecture and it was arrived at by measurement, after
/// three wrong turns. On the player, with the VPU decoding:
///
/// | path | cores | drops |
/// |---|---|---|
/// | video + page through `compositor` | 2.4 | many |
/// | video + page through `input-selector` | 0.9 | 33-45 |
/// | **video alone, its own sink** | **0.43** | **0** |
///
/// The reason is dmabuf. `v4l2slh264dec` hands frames over as dmabuf and
/// `waylandsink` imports them zero-copy; putting *anything* in between -- a
/// compositor, a selector, a videoconvert -- forces a copy into system memory
/// of 1920x1024 at 60fps. `waylandsink` also advertises NV12 but cannot
/// actually negotiate it, so there is no format that lets a converter be
/// cheap. The video simply has to reach the sink untouched.
///
/// That constraint is about compositing **inside the pipeline, on the CPU**.
/// Compositing outside it costs nothing extra, because -- measured 2026-09-02
/// -- it was already happening: with a clip playing full-screen the
/// scanned-out framebuffer is `allocated by = cage`, so wlroots has always
/// been importing the decoder's dmabuf and sampling it on the Mali. There was
/// no direct-scanout path to protect. See [`crate::wayland`].
///
/// So the page and each clip get a pipeline each, and every sink is pointed at
/// **one shared toplevel** with a render rectangle. That is what allows a clip
/// to play in a region beside other content, and several clips at once.
///
/// The page still goes to **PAUSED** while a clip *covers the whole layout*,
/// which is now an optimisation rather than the only way to show video: there
/// is no point rendering a page nothing can see. It is never put in READY --
/// measured on the hardware, WPE keeps running the page's JavaScript at full
/// rate while PAUSED (2.0 ticks/second against 2.0 while PLAYING) and stops
/// dead in READY (0.0), and the page is what calls `play()`, waits for `ended`
/// and advances regions, so killing its timers would stop the schedule.
///
/// Confirmed end to end on the panel: page, then a 45-second clip with zero
/// drop warnings, then the page again.
struct Renderer {
    page: gst::Pipeline,
    src: gst::Element,
    /// Keyed by widget id: a layout may play several clips at once.
    videos: Mutex<HashMap<String, gst::Pipeline>>,
    wl: Arc<Wayland>,
}

/// Does this widget cover the whole *layout*?
///
/// This used to decide whether a clip could use the VPU **at all**; it now only
/// decides whether the page is worth rendering underneath it. Compared against
/// the layout, not the screen: a 1920x1024 canvas on a 1920x1080 output is the
/// normal case, and comparing against the screen rejected exactly the case this
/// exists for.
fn covers_layout(x: i32, y: i32, w: i32, h: i32, lw: i32, lh: i32) -> bool {
    x <= 0 && y <= 0 && w >= lw && h >= lh
}

/// The `GstContext` that hands a sink our `wl_display`.
///
/// It must reach the sink **before** `set_window_handle`, and is set
/// proactively rather than in answer to a `need-context` message for that
/// reason. The sink checks: given a foreign surface while holding a display it
/// opened itself, it raises `RESOURCE, OPEN_READ_WRITE` -- "Application did not
/// provide a wayland display handle" -- rather than drawing into the wrong
/// connection. So a mistake here is loud, and that message is the one to look
/// for if a surface ever fails to appear.
fn wayland_context(wl: &Wayland) -> gst::Context {
    let mut context = gst::Context::new("GstWaylandDisplayHandleContextType", true);
    // The field name and its `G_TYPE_POINTER` type are the ones
    // `gst_wayland_display_handle_context_get_handle` reads back with
    // `gst_structure_get (s, "handle", G_TYPE_POINTER, ...)`.
    context
        .get_mut()
        .expect("a fresh context is writable")
        .structure_mut()
        .set("handle", DisplayPtr(wl.display_handle()));
    context
}

/// A `wl_display*` on its way into a `GstStructure`.
///
/// It needs a wrapper for one reason: a structure's setter requires `Send`,
/// because a structure can cross threads, and a raw pointer is not. Asserting
/// it for this pointer is sound and is the basis of the whole embedding --
/// libwayland is thread-safe for one display dispatched through several
/// queues, which is exactly what GStreamer's sinks and [`crate::wayland`] do.
/// The bindings cannot express a `G_TYPE_POINTER` field, so this is also where
/// the value is built by hand.
struct DisplayPtr(*mut std::ffi::c_void);

unsafe impl Send for DisplayPtr {}

impl From<DisplayPtr> for glib::Value {
    fn from(ptr: DisplayPtr) -> Self {
        use glib::translate::ToGlibPtrMut;
        let mut value = glib::Value::from_type(glib::Type::POINTER);
        unsafe {
            glib::gobject_ffi::g_value_set_pointer(value.to_glib_none_mut().0, ptr.0);
        }
        value
    }
}

/// Points a pipeline's sink at our surface, at the given rectangle.
///
/// The rectangle is in layout coordinates, which are the surface's: the layout
/// is rendered at the top left by the output contract, so no offset is applied.
///
/// Order matters. The context has to be set before the state change that
/// creates the window, and `set_window_handle` before `set_render_rectangle`:
/// a rectangle set with no window is **discarded with a warning**, which is
/// how the first attempt at this silently played full-screen.
fn place_sink(pipeline: &gst::Pipeline, wl: &Wayland, x: i32, y: i32, w: i32, h: i32)
    -> Result<()>
{
    let sink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("pipeline has no element named 'sink'"))?;
    sink.set_context(&wayland_context(wl));
    let overlay = sink
        .dynamic_cast::<gst_video::VideoOverlay>()
        .map_err(|_| anyhow!("waylandsink is not a GstVideoOverlay"))?;
    unsafe {
        overlay.set_window_handle(wl.surface_handle());
    }
    overlay
        .set_render_rectangle(x, y, w, h)
        .map_err(|e| anyhow!("setting the render rectangle to {w}x{h}+{x}+{y}: {e}"))?;
    Ok(())
}

/// Builds the page pipeline.
///
/// No videoconvert and no selector: nothing shares this sink, so there is
/// nothing to reconcile formats with.
fn build_page(base_uri: &str, width: i32, height: i32) -> Result<(gst::Pipeline, gst::Element)> {
    let desc = format!(
        "wpevideosrc name=src location={base_uri}{SPLASH} \
         ! video/x-raw,width={width},height={height},framerate={PAGE_FPS}/1 \
         ! queue max-size-buffers=3 leaky=downstream \
         ! waylandsink name=sink"
    );
    log::info!("page pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .context("building the page pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("parse::launch did not return a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow!("page pipeline has no element named 'src'"))?;
    Ok((pipeline, src))
}

/// Builds a pipeline for one clip: decoder straight into its own sink.
///
/// The chain is explicit -- `qtdemux ! h264parse ! v4l2slh264dec !
/// waylandsink` -- and every alternative was measured on the hardware:
///
/// | pipeline | negotiates | drops |
/// |---|---|---|
/// | `decodebin ! waylandsink` | **no** | -- |
/// | `decodebin ! videoconvert ! waylandsink` | yes | **14** |
/// | **`h264parse ! v4l2slh264dec ! waylandsink`** | **yes** | **0** |
/// | `h264parse ! decodebin ! waylandsink` | **no** | -- |
///
/// `decodebin` cannot negotiate with `waylandsink` at all: the decoder's
/// output is dmabuf and decodebin's autoplug works from static caps, which do
/// not advertise it. Adding a `videoconvert` makes it negotiate by copying
/// every frame out of dmabuf into system memory -- 1920x1024 at 60fps -- and
/// that copy alone drops frames even in a pipeline with nothing else in it.
/// Linking the decoder to the sink directly lets them agree on dmabuf and
/// import zero-copy.
///
/// The cost is generality: this handles H.264 in MP4, which is what the CMS
/// publishes for these walls, and nothing else. Anything else fails to build
/// and is handed back to the page, which is visible in the log rather than
/// silent.
///
/// `qtdemux`'s pads are dynamic, and its audio pad must be terminated: left
/// unlinked it stalls the demuxer with "streaming stopped, reason
/// not-linked". Audio is discarded because the host cannot route it yet, so
/// the clip plays silently rather than not at all.
fn build_video(res_dir: &Path, mid: &str, uri: &str) -> Result<gst::Pipeline> {
    // The page reports a basename and the file lives in the resource cache.
    // Reject a path rather than trusting the page: the shim is ours, but the
    // document it runs in renders CMS-authored content.
    if uri.is_empty() || uri.contains('/') || uri.contains('\\') || uri.contains("..") {
        return Err(anyhow!("refusing suspicious video uri {uri:?}"));
    }
    let path: PathBuf = res_dir.join(uri);
    if !path.is_file() {
        return Err(anyhow!("video {} is not in the resource cache", path.display()));
    }

    let pipeline = gst::Pipeline::with_name(&format!("video-{mid}"));
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().ok_or_else(|| anyhow!("non-UTF8 path"))?)
        .build()
        .context("creating filesrc")?;
    let demux = gst::ElementFactory::make("qtdemux").build().context("creating qtdemux")?;
    let parse = gst::ElementFactory::make("h264parse").build().context("creating h264parse")?;
    let dec = gst::ElementFactory::make("v4l2slh264dec")
        .build()
        .context("creating v4l2slh264dec (is gstreamer1.0-plugins-bad installed?)")?;
    let sink = gst::ElementFactory::make("waylandsink")
        .name("sink")
        .build()
        .context("creating waylandsink")?;
    pipeline
        .add_many([&filesrc, &demux, &parse, &dec, &sink])
        .context("assembling the video pipeline")?;
    filesrc.link(&demux).context("linking filesrc to qtdemux")?;
    gst::Element::link_many([&parse, &dec, &sink])
        .context("linking the decode chain to the sink")?;

    let pipeline_weak = pipeline.downgrade();
    let parse_c = parse.clone();
    let mid_log = mid.to_string();
    demux.connect_pad_added(move |_, pad| {
        let name = pad
            .current_caps()
            .or_else(|| Some(pad.query_caps(None)))
            .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
            .unwrap_or_default();
        if name.starts_with("video/x-h264") {
            match parse_c.static_pad("sink") {
                Some(sp) if !sp.is_linked() => {
                    if let Err(e) = pad.link(&sp) {
                        log::warn!("video {mid_log}: could not link the video pad: {e}");
                    }
                }
                _ => log::warn!("video {mid_log}: h264parse sink already linked"),
            }
        } else {
            log::debug!("video {mid_log}: discarding a {name} stream");
            if let Some(pl) = pipeline_weak.upgrade() {
                match gst::ElementFactory::make("fakesink")
                    .property("async", false)
                    .property("sync", false)
                    .build()
                {
                    Ok(fs) => {
                        let _ = pl.add(&fs);
                        let _ = fs.sync_state_with_parent();
                        if let Some(sp) = fs.static_pad("sink") {
                            let _ = pad.link(&sp);
                        }
                    }
                    Err(e) => log::warn!("video {mid_log}: no fakesink: {e}"),
                }
            }
        }
    });
    Ok(pipeline)
}

/// Runs a script inside the page.
///
/// The host-to-page half of the bridge. Unlike page-to-host -- which needs the
/// HTTP shim in [`crate::bridge`], because `qrc://` resolves in nothing but Qt
/// -- WPE provides this direction itself. It works while the page pipeline is
/// PAUSED, because the WebView lives in its own process.
fn run_js(src: &gst::Element, script: &str) {
    src.emit_by_name::<()>("run-javascript", &[&script.to_string()]);
}

impl Renderer {
    /// Tears one clip down, and resumes the page if it was the last.
    fn stop_video(&self, mid: &str) {
        let (taken, remaining) = {
            let mut videos = self.videos.lock();
            let taken = videos.remove(mid);
            (taken, videos.len())
        };
        if let Some(vp) = taken {
            let _ = vp.set_state(gst::State::Null);
            log::info!("video {mid}: stopped");
        }
        // Only the covering case pauses the page, but resuming unconditionally
        // is right: a page already PLAYING is not disturbed by being asked
        // again, and tracking which clip did the pausing would go wrong the
        // first time two clips overlapped.
        if remaining == 0 {
            if let Err(e) = self.page.set_state(gst::State::Playing) {
                log::warn!("could not resume the page: {e}");
            }
        }
    }
}

pub fn run(
    settings: PlayerSettings,
    _screen: String,
    _inspect: bool,
    _debug: bool,
    togui: Receiver<ToGui>,
    fromgui: Sender<FromGui>,
    bridge: Receiver<BridgeMsg>,
    res_dir: PathBuf,
) -> Result<()> {
    gst::init().context("initialising GStreamer")?;

    let base_uri = format!("http://localhost:{}/", settings.embedded_server_port);
    // Zero means "use the screen size" in the Xibo display profile, and that
    // is the setting that selects a fullscreen surface.
    let width = if settings.size_x > 0 { settings.size_x as i32 } else { 1920 };
    let height = if settings.size_y > 0 { settings.size_y as i32 } else { 1080 };

    // Ours, not the sinks': one toplevel they all draw into.
    let wl = Wayland::new(width, height).context("setting up the Wayland surface")?;

    let (page, src) = build_page(&base_uri, width, height)?;
    place_sink(&page, &wl, 0, 0, width, height).context("placing the page's sink")?;
    log::info!("renderer: wpe (GStreamer), page surface {width}x{height}");
    log::warn!("renderer: wpe does not implement screenshots or audio yet");

    let r = Arc::new(Renderer {
        page: page.clone(),
        src: src.clone(),
        videos: Mutex::new(HashMap::new()),
        wl: wl.clone(),
    });
    let schedule = Arc::new(Mutex::new(Schedule::<LayoutId>::default()));
    // The page reports its own size on init, and that -- not the screen -- is
    // what a video must cover to qualify for the switching path.
    let layout_size = Arc::new(Mutex::new((width, height)));

    // Messages from the backend: schedule changes, settings, webhooks.
    {
        let src = src.clone();
        let base = base_uri.clone();
        let schedule = schedule.clone();
        std::thread::spawn(move || {
            for msg in togui {
                match msg {
                    ToGui::Layouts(new_layouts) => {
                        if let Some(id) = schedule.lock().update(new_layouts) {
                            log::info!("new schedule, showing layout: {id}");
                            src.set_property("location", format!("{base}{id}.xlf.html"));
                        }
                    }
                    ToGui::WebHook(code) => {
                        run_js(&src, &format!("window.arexibo.trigger({code:?});"));
                    }
                    ToGui::Settings(s) => {
                        log::info!("settings updated (display name {:?}); the wpe \
                                    surface stays fullscreen", s.display_name);
                    }
                    ToGui::Screenshot => {
                        log::warn!("screenshot requested, but the wpe renderer cannot \
                                    take one yet -- ignoring");
                    }
                }
            }
        });
    }

    // Calls from the page, over the HTTP bridge. These mirror the Qt callbacks
    // one for one; see `crate::gui::callback`.
    {
        let r = r.clone();
        let base = base_uri.clone();
        let schedule = schedule.clone();
        let layout_size = layout_size.clone();
        std::thread::spawn(move || {
            for msg in bridge {
                match msg {
                    BridgeMsg::LayoutInit { id, width, height } => {
                        log::info!("layout {id} initialized ({width}x{height})");
                        if width > 0 && height > 0 {
                            *layout_size.lock() = (width, height);
                        }
                        // The splash is id 0 and is not announced: the CMS must
                        // not be told we are showing a layout that is not its.
                        if id > 0 {
                            let _ = fromgui.send(FromGui::Showing(id));
                        }
                    }
                    BridgeMsg::LayoutDone { .. } => {
                        let mut schedule = schedule.lock();
                        if let Some(id) = schedule.next() {
                            log::info!("showing next layout: {id}");
                            r.src.set_property("location", format!("{base}{id}.xlf.html"));
                        } else {
                            schedule.mark_done();
                        }
                    }
                    BridgeMsg::LayoutPrev { .. } => {
                        if let Some(id) = schedule.lock().prev() {
                            log::info!("showing previous layout: {id}");
                            r.src.set_property("location", format!("{base}{id}.xlf.html"));
                        }
                    }
                    BridgeMsg::LayoutJump { target, .. } => {
                        log::info!("jumping to layout: {target}");
                        r.src.set_property("location", format!("{base}{target}.xlf.html"));
                    }
                    BridgeMsg::Command(cmd) => { let _ = fromgui.send(FromGui::Command(cmd)); }
                    BridgeMsg::Shell(cmd, sh) => { let _ = fromgui.send(FromGui::Shell(cmd, sh)); }
                    BridgeMsg::StopShell(mode) => {
                        let kill = match mode {
                            0 => Kill::No,
                            1 => Kill::Terminate,
                            _ => Kill::Kill,
                        };
                        let _ = fromgui.send(FromGui::StopShell(kill));
                    }
                    BridgeMsg::VideoPlay { mid, uri, x, y, w, h, muted: _ } => {
                        let (lw, lh) = *layout_size.lock();
                        if w <= 0 || h <= 0 {
                            log::warn!("video {mid} has no area ({w}x{h}); leaving it to \
                                        the page");
                            run_js(&r.src, &format!(
                                "(function(){{var e=document.getElementById({mid:?});\
                                  if(e){{e.style.opacity='1';}}}})();"));
                            continue;
                        }
                        let covers = covers_layout(x, y, w, h, lw, lh);
                        // A repeat play for the same widget means the page is
                        // looping it; drop the old pipeline first.
                        r.stop_video(&mid);
                        match build_video(&res_dir, &mid, &uri) {
                            Ok(vp) => {
                                if let Err(e) = place_sink(&vp, &r.wl, x, y, w, h) {
                                    log::warn!("video {mid}: {e:#}; leaving it to the page");
                                    run_js(&r.src, &format!(
                                        "(function(){{var e=document.getElementById({mid:?});\
                                          if(e){{e.style.opacity='1';}}}})();"));
                                    continue;
                                }
                                // Nothing of the page would be visible under a
                                // clip that covers the layout, so stop paying
                                // to render it. Anything smaller leaves the
                                // page up: that is the point of this path.
                                if covers {
                                    if let Err(e) = r.page.set_state(gst::State::Paused) {
                                        log::warn!("could not pause the page: {e}");
                                    }
                                }
                                if let Err(e) = vp.set_state(gst::State::Playing) {
                                    log::warn!("video {mid}: will not start: {e}");
                                    r.stop_video(&mid);
                                    continue;
                                }
                                log::info!("video {mid}: playing {uri} on the VPU at \
                                            {w}x{h}+{x}+{y}{}", if covers {
                                                ", page paused" } else { "" });
                                r.videos.lock().insert(mid.clone(), vp.clone());

                                // Watch this clip's own bus. On its end, tell
                                // the page so its sequencing advances exactly
                                // as it would have done had the page decoded
                                // the clip itself -- and put the page back up,
                                // rather than leaving the last frame frozen.
                                let r2 = r.clone();
                                let mid2 = mid.clone();
                                std::thread::spawn(move || {
                                    let bus = match vp.bus() {
                                        Some(b) => b,
                                        None => return,
                                    };
                                    for m in bus.iter_timed(gst::ClockTime::NONE) {
                                        use gst::MessageView;
                                        match m.view() {
                                            MessageView::Eos(..) => {
                                                log::info!("video {mid2} reached its end");
                                                break;
                                            }
                                            MessageView::Error(e) => {
                                                log::error!(
                                                    "video {mid2}: {} ({:?})",
                                                    e.error(), e.debug());
                                                break;
                                            }
                                            _ => {}
                                        }
                                    }
                                    // Only act if this clip is still the
                                    // current one; a newer play may have
                                    // replaced it while we were waiting.
                                    let still_ours = r2
                                        .videos
                                        .lock()
                                        .get(&mid2)
                                        .map(|p| p == &vp)
                                        .unwrap_or(false);
                                    if still_ours {
                                        r2.stop_video(&mid2);
                                        run_js(&r2.src,
                                               &format!("window.__gaxiboVideoEnded({mid2:?});"));
                                    }
                                });
                            }
                            Err(e) => {
                                // A hole on screen is worse than a
                                // software-decoded clip, so hand it back.
                                log::warn!("video {mid}: {e:#}; leaving it to the page");
                                run_js(&r.src, &format!(
                                    "(function(){{var e=document.getElementById({mid:?});\
                                      if(e){{e.style.opacity='1';}}}})();"));
                            }
                        }
                    }
                    BridgeMsg::VideoStop { mid } => r.stop_video(&mid),
                    BridgeMsg::Log(line) => log::info!("page: {line}"),
                }
            }
        });
    }

    page.set_state(gst::State::Playing).context("starting the page pipeline")?;

    // The page's bus loop owns this thread for the life of the player, the way
    // the Qt event loop does. A video's bus is watched by its own thread.
    let bus = page.bus().ok_or_else(|| anyhow!("page pipeline has no bus"))?;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                log::error!(
                    "page pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            MessageView::Warning(w) => {
                log::warn!("page pipeline warning: {} ({:?})", w.error(), w.debug());
            }
            MessageView::StateChanged(sc) => {
                if sc.src().map(|s| s == &page).unwrap_or(false) {
                    log::info!("page pipeline: {:?} -> {:?}", sc.old(), sc.current());
                }
            }
            MessageView::Eos(..) => {
                // wpevideosrc is live, so EOS means the WebView went away
                // rather than "the content finished".
                log::error!("page pipeline reached end of stream unexpectedly");
                break;
            }
            _ => {}
        }
    }

    let _ = page.set_state(gst::State::Null);
    Err(anyhow!("the wpe renderer's page pipeline stopped"))
}
