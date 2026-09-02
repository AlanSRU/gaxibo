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
fn build(base_uri: &str, width: i32, height: i32) -> Result<(gst::Pipeline, gst::Element)> {
    // No `fullscreen=true` on the sink.  Setting it at construction asserts
    // `gst_wl_window_ensure_fullscreen: assertion 'self' failed` because the
    // Wayland window does not exist yet, and the sink then never consumes past
    // the first buffer -- which shows as WPE's blank white pre-paint frame
    // frozen on screen, with the web process idling at 0.09 cores.  cage
    // fullscreens its single client itself, so the property was never needed.
    //
    // A queue decouples WPE's render thread from the sink, so a slow present
    // cannot stall page rendering.
    let desc = format!(
        "wpevideosrc name=src location={base_uri}{SPLASH} \
         ! video/x-raw,width={width},height={height},framerate=60/1 \
         ! queue max-size-buffers=3 leaky=downstream \
         ! videoconvert \
         ! waylandsink name=sink"
    );
    log::debug!("pipeline: {desc}");
    let pipeline = gst::parse::launch(&desc)
        .context("building the GStreamer pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("parse::launch did not return a pipeline"))?;
    let src = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow!("pipeline has no element named 'src'"))?;
    Ok((pipeline, src))
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

pub fn run(
    settings: PlayerSettings,
    _screen: String,
    _inspect: bool,
    _debug: bool,
    togui: Receiver<ToGui>,
    fromgui: Sender<FromGui>,
    bridge: Receiver<BridgeMsg>,
) -> Result<()> {
    gst::init().context("initialising GStreamer")?;

    let base_uri = format!("http://localhost:{}/", settings.embedded_server_port);
    // Zero means "use the screen size" in the Xibo display profile, and that is
    // the setting that selects a fullscreen surface.  See the output contract:
    // leaving these at 0 is what keeps the frame unresampled.
    let width = if settings.size_x > 0 { settings.size_x as i32 } else { 1920 };
    let height = if settings.size_y > 0 { settings.size_y as i32 } else { 1080 };

    let (pipeline, src) = build(&base_uri, width, height)?;
    log::info!("renderer: wpe (GStreamer), surface {width}x{height}");
    log::warn!("renderer: wpe does not implement screenshots or audio yet");

    let schedule = Arc::new(Mutex::new(Schedule::<LayoutId>::default()));

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
        std::thread::spawn(move || {
            for msg in bridge {
                match msg {
                    BridgeMsg::LayoutInit { id, width, height } => {
                        log::info!("layout {id} initialized ({width}x{height})");
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
                log::error!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
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
