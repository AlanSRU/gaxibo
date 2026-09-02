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
use gst::prelude::*;
use parking_lot::Mutex;

use crate::bridge::BridgeMsg;
use crate::config::PlayerSettings;
use crate::gui::Schedule;
use crate::mainloop::{FromGui, Kill, ToGui};
use crate::resource::LayoutId;

/// The splash layout, served built-in by the embedded webserver.
const SPLASH: &str = "0.xlf.html";

/// Frame rate asked of WPE for the page layer.
///
/// Not 60. A signage page is static almost all of the time, and 1920x1080
/// BGRA at 60fps is about 500 MB/s of memory writes for frames that are
/// identical -- on RK3399 that bandwidth is contended with the VPU and the
/// display controller, and it showed up as waylandsink dropping video buffers
/// while the CPU sat at 0.86 of 6 cores. Compute was never the limit.
///
/// 15 is enough for the animation a Xibo layout actually does (a ticker
/// scrolling, a fade) and a quarter of the bandwidth.
const PAGE_FPS: i32 = 15;

/// Builds the pipeline.
///
/// `wpevideosrc` is asked for plain `video/x-raw` rather than GLMemory: the
/// GL path needs a GL context per element and buys nothing here, since
/// `glvideomixer` measured no faster than the CPU compositor on this board.
///
/// The caps carry the *screen* size, not the layout size.  The generated HTML
/// positions everything in absolute pixels from the top left, so rendering it
/// into a larger viewport leaves the artwork unscaled with black around it --
/// which is exactly the output contract.  Scaling anything here would break
/// 1:1 mapping on an LED wall, which is the fault this whole pipeline exists
/// to avoid.
fn build(base_uri: &str, width: i32, height: i32)
    -> Result<(gst::Pipeline, gst::Element, gst::Element)> {
    // No `fullscreen=true` on the sink.  Setting it at construction asserts
    // `gst_wl_window_ensure_fullscreen: assertion 'self' failed` because the
    // Wayland window does not exist yet, and the sink then never consumes past
    // the first buffer -- which shows as WPE's blank white pre-paint frame
    // frozen on screen, with the web process idling at 0.09 cores.  cage
    // fullscreens its single client itself, so the property was never needed.
    //
    // A queue decouples WPE's render thread from the sink, so a slow present
    // cannot stall page rendering.
    // An input-selector, not a compositor.
    //
    // Blending was the whole problem. Measured on the player: hardware video
    // straight to waylandsink costs **0.43 cores and drops nothing** and is
    // smooth, while the same clip blended with a full-screen WPE layer through
    // `compositor` cost 2.4 cores and the sink reported "a lot of buffers are
    // being dropped ... this computer is too slow" -- visibly worse than the
    // software decode it was meant to replace.
    //
    // A full-screen video covers the page completely, so there is nothing to
    // blend: switching the sink's input is equivalent on screen and costs
    // almost nothing. videoconvert stays because WPE hands over BGRA while the
    // decoder hands over NV12, which waylandsink takes natively -- so it is
    // passthrough for the video and only converts the page.
    //
    // `sync-streams=false` matters. By default input-selector synchronises the
    // *inactive* streams to the running time of the active one. wpevideosrc is
    // live and never stops producing, so with the file branch active the
    // selector was reconciling a live stream against a non-live one and the
    // sink dropped the file's buffers as too late -- at 0.71 of 6 cores, so
    // never a throughput problem. Nothing needs the inactive stream's timing:
    // it is not on screen.
    let desc = format!(
        "input-selector name=sel sync-streams=false \
         ! videoconvert ! queue max-size-buffers=3 ! waylandsink name=sink \
         wpevideosrc name=src location={base_uri}{SPLASH} \
         ! video/x-raw,width={width},height={height},framerate={PAGE_FPS}/1 \
         ! queue max-size-buffers=3 leaky=downstream ! sel.sink_0"
    );
    log::debug!("pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .context("building the GStreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("parse::launch did not return a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow!("pipeline has no element named 'src'"))?;
    let sel = pipeline
        .by_name("sel")
        .ok_or_else(|| anyhow!("pipeline has no element named 'sel'"))?;
    Ok((pipeline, src, sel))
}

/// Points the WebView at a layout.
///
/// Navigation is a property change on a **live** element, never a pipeline
/// rebuild.  Verified on the hardware: one pipeline navigated four times, by
/// `location`, by `run-javascript` and by `load-bytes`, with every transition
/// landing on its cue.
fn navigate(src: &gst::Element, base_uri: &str, id: LayoutId) {
    let url = format!("{base_uri}{id}.xlf.html");
    log::debug!("navigating to {url}");
    src.set_property("location", url);
}

/// Runs a script inside the page.
///
/// This is the host-to-page half of the bridge.  Unlike the page-to-host half
/// -- which needs the HTTP shim in `crate::bridge`, because `qrc://` resolves
/// in nothing but Qt -- WPE provides this direction itself.
fn run_js(src: &gst::Element, script: &str) {
    src.emit_by_name::<()>("run-javascript", &[&script.to_string()]);
}

/// A video widget currently being decoded outside the page.
struct Active {
    bin: gst::Element,
    pad: gst::Pad,
}

/// Does this widget cover the whole *layout*?
///
/// Only such a video can take the switching path, because switching replaces
/// the page rather than compositing over it.  Every layout Pixelmabob
/// publishes is exactly this shape: one region at 0,0 covering the canvas.
///
/// Compared against the layout, not the surface.  A 1920x1024 canvas on a
/// 1920x1080 screen is the normal case on the bench, and comparing against the
/// screen rejected it -- the video is still the entire content of the layout.
/// waylandsink keeps `force-aspect-ratio` on by default, so a 1920x1024 frame
/// on a 1920x1080 output is centred **unscaled** with black bars, which is the
/// output contract rather than a compromise: content in a rectangle, pure black
/// around it.
fn covers_layout(x: i32, y: i32, w: i32, h: i32, lw: i32, lh: i32) -> bool {
    x <= 0 && y <= 0 && w >= lw && h >= lh
}

/// Starts decoding a clip and composites it over the page.
///
/// `decodebin` rather than an explicit `v4l2slh264dec`: the hardware decoder
/// already outranks the software one (257 against 256, verified on the
/// shipping package set), so autoplug picks the VPU for H.264 by itself and
/// still handles a codec the VPU cannot do. Naming the element explicitly
/// would turn an unsupported codec into a hard failure.
///
/// Nothing scales: no videoscale, and the sink is handed the clip at its
/// native size.  Resampling is the one thing this whole pipeline exists to
/// avoid on an LED wall, so a clip whose size does not match the surface is
/// better wrong-sized and sharp than fitted and soft.
fn add_video(
    pipeline: &gst::Pipeline,
    sel: &gst::Element,
    src: &gst::Element,
    res_dir: &Path,
    mid: &str,
    uri: &str,
) -> Result<Active> {
    // The page reports a basename, and the file lives in the resource cache.
    // Reject anything with a path separator rather than trusting the page:
    // the shim is ours, but the page it runs in renders CMS-authored content.
    if uri.contains('/') || uri.contains('\\') || uri.contains("..") || uri.is_empty() {
        return Err(anyhow!("refusing suspicious video uri {uri:?}"));
    }
    let path: PathBuf = res_dir.join(uri);
    if !path.is_file() {
        return Err(anyhow!("video {} is not in the resource cache", path.display()));
    }

    // Built element by element rather than with parse, because the audio
    // stream has to be dealt with explicitly.
    //
    // The obvious shortcut -- `decodebin caps=video/x-raw
    // expose-all-streams=false` -- is wrong: it stops decodebin finding a
    // decoder at all, failing with "Missing decoder: H.264 (High Profile)" on
    // a clip that plain decodebin handles. Verified both ways on the hardware.
    //
    // So decodebin stays unconstrained, and `pad-added` routes the video to
    // the converter and terminates everything else in a fakesink. Without
    // that, the unlinked audio pad stalls the demuxer with
    // "streaming stopped, reason not-linked (-1)" and the clip never plays.
    // The host cannot route audio yet, so it is discarded: the clip plays
    // silently rather than not at all.
    let bin = gst::Bin::with_name(&format!("video-{mid}"));
    let filesrc = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().ok_or_else(|| anyhow!("non-UTF8 path"))?)
        .build()
        .context("creating filesrc")?;
    let dec = gst::ElementFactory::make("decodebin").build().context("creating decodebin")?;
    let conv = gst::ElementFactory::make("videoconvert").build().context("creating videoconvert")?;
    let q = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 3u32)
        .build()
        .context("creating queue")?;
    bin.add_many([&filesrc, &dec, &conv, &q]).context("assembling the video branch")?;
    filesrc.link(&dec).context("linking filesrc to decodebin")?;
    conv.link(&q).context("linking videoconvert to queue")?;

    {
        let conv = conv.clone();
        let bin_weak = bin.downgrade();
        let mid_for_log = mid.to_string();
        dec.connect_pad_added(move |_, pad| {
            let name = pad
                .current_caps()
                .or_else(|| Some(pad.query_caps(None)))
                .and_then(|c| c.structure(0).map(|st| st.name().to_string()))
                .unwrap_or_default();
            if name.starts_with("video/") {
                if let Some(sink) = conv.static_pad("sink") {
                    if let Err(e) = pad.link(&sink) {
                        log::warn!("video {mid_for_log}: could not link video pad: {e}");
                    }
                }
            } else {
                log::debug!("video {mid_for_log}: discarding a {name} stream");
                if let Some(bin) = bin_weak.upgrade() {
                    match gst::ElementFactory::make("fakesink")
                        .property("async", false)
                        .property("sync", false)
                        .build()
                    {
                        Ok(fs) => {
                            let _ = bin.add(&fs);
                            let _ = fs.sync_state_with_parent();
                            if let Some(sp) = fs.static_pad("sink") {
                                let _ = pad.link(&sp);
                            }
                        }
                        Err(e) => log::warn!("video {mid_for_log}: no fakesink: {e}"),
                    }
                }
            }
        });
    }

    // Ghost the queue's output so the bin can be linked to the compositor.
    let qsrc = q.static_pad("src").ok_or_else(|| anyhow!("queue has no src pad"))?;
    let ghost = gst::GhostPad::with_target(&qsrc).context("ghosting the branch output")?;
    bin.add_pad(&ghost).context("adding the ghost pad")?;
    let bin = bin.upcast::<gst::Element>();

    pipeline.add(&bin).context("adding the video branch")?;

    let pad = sel
        .request_pad_simple("sink_%u")
        .ok_or_else(|| anyhow!("input-selector refused a new pad"))?;

    let srcpad = bin
        .static_pad("src")
        .ok_or_else(|| anyhow!("video branch has no src pad"))?;

    // Tell the page when the clip ends, so its own sequencing advances exactly
    // as it would have done had the page decoded the video itself.  The EOS is
    // dropped rather than forwarded: an EOS'd compositor pad would freeze the
    // clip's last frame on screen, and the page tears the branch down for us
    // by calling pause() on the way out of the widget.
    let js_src = src.clone();
    let mid_owned = mid.to_string();
    srcpad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
        if let Some(gst::PadProbeData::Event(ref ev)) = info.data {
            if ev.type_() == gst::EventType::Eos {
                log::info!("video {mid_owned} reached its end");
                run_js(&js_src, &format!("window.__gaxiboVideoEnded({mid_owned:?});"));
                return gst::PadProbeReturn::Drop;
            }
        }
        gst::PadProbeReturn::Ok
    });

    srcpad.link(&pad).context("linking the video branch to the selector")?;

    // Shift the clip's timestamps so they start *now*.
    //
    // This is the fix for playback racing, and the symptom was the opposite of
    // what it looked like: the 60-second clip finished in 17 seconds -- 3.6x
    // too fast, which is exactly the VPU's decode speed. The sink was not
    // pacing it at all, and the "a lot of buffers are being dropped" warnings
    // were the consequence, not the cause. At 0.86 of 6 cores it was never
    // too slow; it was far too fast.
    //
    // Why: the branch's buffers start at PTS 0, and waylandsink renders at
    // `its own base time + PTS`. The sink sits in the main pipeline, not in
    // this bin, and the pipeline is live because wpevideosrc is -- so the
    // sink's base time is from when the *player* started. PTS 0 therefore maps
    // to minutes ago, every buffer is late, and the sink renders them all
    // immediately.
    //
    // Setting the base time on the bin, which is what I tried first, cannot
    // work: base time belongs to the element doing the syncing, and that
    // element is outside the bin.
    //
    // A pad offset is the mechanism meant for this -- it shifts timestamps as
    // they cross the pad, so PTS 0 becomes "now" for everything downstream.
    let offset = pipeline
        .current_running_time()
        .map(|t| t.nseconds() as i64)
        .unwrap_or(0);
    srcpad.set_offset(offset);
    log::debug!("video {mid}: timestamps offset by {offset} ns");

    bin.sync_state_with_parent().context("starting the video branch")?;

    // Show it.  Until this, the page is still on screen.
    sel.set_property("active-pad", &pad);
    log::info!("video {mid}: decoding {} (VPU), sink switched to it", path.display());
    Ok(Active { bin, pad })
}

/// Tears a clip's branch down.  Order matters: stop the bin before unlinking,
/// or a buffer in flight can outlive the pad it was going to.
fn remove_video(pipeline: &gst::Pipeline, sel: &gst::Element, mid: &str, a: Active) {
    // Put the page back on screen *before* tearing the branch down, or the
    // sink is left with no input and the last frame freezes.
    if let Some(page) = sel.static_pad("sink_0") {
        sel.set_property("active-pad", &page);
    }
    let _ = a.bin.set_state(gst::State::Null);
    if let Some(srcpad) = a.bin.static_pad("src") {
        let _ = srcpad.unlink(&a.pad);
    }
    if let Err(e) = pipeline.remove(&a.bin) {
        log::warn!("video {mid}: could not remove branch: {e}");
    }
    sel.release_request_pad(&a.pad);
    log::info!("video {mid}: stopped");
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
    // Zero means "use the screen size" in the Xibo display profile, and that is
    // the setting that selects a fullscreen surface.  See the output contract:
    // leaving these at 0 is what keeps the frame unresampled.
    let width = if settings.size_x > 0 { settings.size_x as i32 } else { 1920 };
    let height = if settings.size_y > 0 { settings.size_y as i32 } else { 1080 };

    let (pipeline, src, sel) = build(&base_uri, width, height)?;
    log::info!("renderer: wpe (GStreamer), surface {width}x{height}");
    log::warn!("renderer: wpe does not implement screenshots or audio yet");

    let schedule = Arc::new(Mutex::new(Schedule::<LayoutId>::default()));
    // Shared because two threads touch it: the bridge thread adds and removes
    // branches on the page's instruction, and the bus loop removes one whose
    // decoder has failed.
    let active: Arc<Mutex<HashMap<String, Active>>> = Arc::new(Mutex::new(HashMap::new()));
    // The page reports its own size on init, and that -- not the screen -- is
    // what a video has to cover to qualify for the switching path.
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
                            navigate(&src, &base, id);
                        }
                    }
                    ToGui::WebHook(code) => {
                        run_js(&src, &format!("window.arexibo.trigger({code:?});"));
                    }
                    ToGui::Settings(s) => {
                        // Size and title do not apply: the surface is
                        // fullscreen and has no decoration to title.  Logged
                        // rather than dropped silently, so a profile change
                        // that the Qt path would have honoured is visible.
                        log::info!("settings updated (display name {:?}); \
                                    the wpe surface stays fullscreen", s.display_name);
                    }
                    ToGui::Screenshot => {
                        log::warn!("screenshot requested, but the wpe renderer \
                                    cannot take one yet -- ignoring");
                    }
                }
            }
        });
    }

    // Calls from the page, arriving over the HTTP bridge.  These mirror the Qt
    // callbacks one for one; see `crate::gui::callback`.
    {
        let src = src.clone();
        let base = base_uri.clone();
        let schedule = schedule.clone();
        let fromgui = fromgui.clone();
        let pipeline = pipeline.clone();
        let sel = sel.clone();
        let active_branches = active.clone();
        let layout_size = layout_size.clone();
        std::thread::spawn(move || {
            let active = active_branches;
            for msg in bridge {
                match msg {
                    BridgeMsg::LayoutInit { id, width, height } => {
                        log::info!("layout {id} initialized ({width}x{height})");
                        if width > 0 && height > 0 {
                            *layout_size.lock() = (width, height);
                        }
                        // The splash screen is id 0 and is not announced: the
                        // CMS must not be told we are showing a layout that is
                        // not one of its own.
                        if id > 0 {
                            let _ = fromgui.send(FromGui::Showing(id));
                        }
                    }
                    BridgeMsg::LayoutDone { .. } => {
                        let mut schedule = schedule.lock();
                        if let Some(id) = schedule.next() {
                            log::info!("showing next layout: {id}");
                            navigate(&src, &base, id);
                        } else {
                            schedule.mark_done();
                        }
                    }
                    BridgeMsg::LayoutPrev { .. } => {
                        if let Some(id) = schedule.lock().prev() {
                            log::info!("showing previous layout: {id}");
                            navigate(&src, &base, id);
                        }
                    }
                    BridgeMsg::LayoutJump { target, .. } => {
                        log::info!("jumping to layout: {target}");
                        navigate(&src, &base, target);
                    }
                    BridgeMsg::Command(cmd) => {
                        let _ = fromgui.send(FromGui::Command(cmd));
                    }
                    BridgeMsg::Shell(cmd, with_shell) => {
                        let _ = fromgui.send(FromGui::Shell(cmd, with_shell));
                    }
                    BridgeMsg::VideoPlay { mid, uri, x, y, w, h, muted: _ } => {
                        // Switching replaces the page rather than compositing
                        // over it, so only a video that covers the whole
                        // surface can take this path.  Blending the two was
                        // measured and is not viable: 2.4 cores with the sink
                        // dropping buffers, against 0.43 and smooth for video
                        // straight to the sink.
                        let (lw, lh) = *layout_size.lock();
                        if !covers_layout(x, y, w, h, lw, lh) {
                            log::warn!("video {mid} is {w}x{h} at +{x}+{y}, which does \
                                        not cover the {lw}x{lh} layout; leaving it to \
                                        the page, which cannot use the VPU");
                            run_js(&src, &format!(
                                "(function(){{var e=document.getElementById({mid:?});\
                                  if(e){{e.style.opacity='1';}}}})();"));
                            continue;
                        }
                        // A repeat play for the same widget means the page is
                        // looping it: tear the old branch down first, or two
                        // decoders composite over each other.
                        if let Some(old) = active.lock().remove(&mid) {
                            remove_video(&pipeline, &sel, &mid, old);
                        }
                        match add_video(&pipeline, &sel, &src, &res_dir, &mid, &uri) {
                            Ok(a) => { active.lock().insert(mid, a); }
                            Err(e) => {
                                // Fall back to letting the page try: a hole on
                                // screen is worse than a software-decoded clip.
                                log::warn!("video {mid}: {e:#}; leaving it to the page");
                                run_js(&src, &format!(
                                    "(function(){{var e=document.getElementById({mid:?});\
                                      if(e){{e.style.opacity='1';}}}})();"));
                            }
                        }
                    }
                    BridgeMsg::VideoStop { mid } => {
                        if let Some(a) = active.lock().remove(&mid) {
                            remove_video(&pipeline, &sel, &mid, a);
                        }
                    }
                    BridgeMsg::StopShell(mode) => {
                        let kill = match mode {
                            0 => Kill::No,
                            1 => Kill::Terminate,
                            _ => Kill::Kill,
                        };
                        let _ = fromgui.send(FromGui::StopShell(kill));
                    }
                }
            }
        });
    }

    pipeline
        .set_state(gst::State::Playing)
        .context("starting the pipeline")?;

    // The bus loop is this renderer's equivalent of the Qt event loop: it owns
    // the calling thread for the life of the player.
    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                let from = err.src().map(|s| s.path_string().to_string()).unwrap_or_default();
                log::error!("pipeline error from {from}: {} ({:?})", err.error(), err.debug());
                // A clip that will not decode must not take the sign off the
                // air.  Only the page and the output are fatal; a video branch
                // is torn down and the layout carries on without it, which is
                // a hole rather than a black screen and a restart loop.
                if from.contains("video-") {
                    let mid = from
                        .split("video-")
                        .nth(1)
                        .and_then(|r| r.split('/').next())
                        .unwrap_or("")
                        .to_string();
                    log::warn!("video {mid}: dropping the branch, layout continues");
                    if let Some(a) = active.lock().remove(&mid) {
                        remove_video(&pipeline, &sel, &mid, a);
                    }
                    run_js(&src, &format!("window.__gaxiboVideoEnded({mid:?});"));
                    continue;
                }
                break;
            }
            MessageView::StateChanged(sc) => {
                // Only the pipeline's own transitions, not every element's.
                if sc.src().map(|s| s == &pipeline).unwrap_or(false) {
                    log::info!("pipeline state: {:?} -> {:?}", sc.old(), sc.current());
                }
            }
            MessageView::Warning(w) => {
                log::warn!("pipeline warning: {} ({:?})", w.error(), w.debug());
            }
            MessageView::Eos(..) => {
                // wpevideosrc is a live source, so EOS means the WebView went
                // away rather than "the content finished".
                log::error!("pipeline reached end of stream unexpectedly");
                break;
            }
            _ => {}
        }
    }

    let _ = pipeline.set_state(gst::State::Null);
    Err(anyhow!("the wpe renderer pipeline stopped"))
}
