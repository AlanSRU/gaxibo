// Gaxibo: HTTP bridge between the rendered page and the player.
// Licensed under the GNU AGPL, version 3 or later.

//! The page-to-host half of the renderer bridge.
//!
//! The Qt renderer talks to its page over QWebChannel, whose transport is
//! `qrc:///qtwebchannel/qwebchannel.js` -- a Qt resource URL that resolves in
//! nothing but Qt.  The WPE renderer therefore needs its own channel.
//!
//! It uses the embedded webserver that already exists to serve the layout, for
//! two reasons: it needs no WebKit bindings at all, and it can be exercised
//! with `curl` without a display.  The page gets a shim that presents the same
//! `window.arexiboGui` object the generated HTML expects, whose methods POST
//! here.
//!
//! The host-to-page direction does not come through here: WPE provides it as
//! the `run-javascript` action signal on the source element.

use std::str::FromStr;

/// A call made by the page.  Mirrors the Qt callback ids in `qt_binding`.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeMsg {
    /// The page has mounted a layout and reports its id and natural size.
    LayoutInit { id: i64, width: i32, height: i32 },
    /// Every region of this layout has finished; advance the schedule.
    LayoutDone { id: i64 },
    /// Go back one layout.
    LayoutPrev { id: i64 },
    /// Jump to a specific layout.
    LayoutJump { id: i64, target: i64 },
    /// Run a player command.
    Command(String),
    /// Run a shell command, optionally through a shell.
    Shell(String, bool),
    /// Stop a running shell command.
    StopShell(u8),
    /// A video widget wants to play.  The renderer decodes it outside the page.
    VideoPlay {
        mid: String,
        uri: String,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        muted: bool,
    },
    /// A video widget was hidden or paused.
    VideoStop { mid: String },
    /// A line of diagnostics from the page.
    ///
    /// The page runs inside WPE with no console anyone can read, so without
    /// this a page that silently fails to start is invisible: the first
    /// symptom is a blank screen and no log at all.
    Log(String),
}

/// The URL prefix the shim posts to.  Kept distinct from any layout filename.
pub const BRIDGE_PREFIX: &str = "/arexibo/";
/// Where the shim itself is served from.
pub const BRIDGE_SCRIPT_PATH: &str = "/arexibo-bridge.js";
/// The Qt transport the generated HTML asks for, which we substitute.
pub const QT_TRANSPORT_SRC: &str = "qrc:///qtwebchannel/qwebchannel.js";

/// Parse a bridge call.  `path` is the full request URL, `body` its body.
///
/// Arguments are sent as newline-separated values rather than JSON so that no
/// JSON dependency is needed on either side and the format stays greppable in
/// a packet capture.
pub fn parse(path: &str, body: &str) -> Option<BridgeMsg> {
    let method = path.strip_prefix(BRIDGE_PREFIX)?;
    let args: Vec<&str> = if body.is_empty() { vec![] } else { body.split('\n').collect() };
    let num = |i: usize| -> Option<i64> { args.get(i).and_then(|s| i64::from_str(s.trim()).ok()) };

    Some(match method {
        "layoutInit" => BridgeMsg::LayoutInit {
            id: num(0)?,
            width: num(1).unwrap_or(0) as i32,
            height: num(2).unwrap_or(0) as i32,
        },
        "layoutDone" => BridgeMsg::LayoutDone { id: num(0)? },
        "layoutPrev" => BridgeMsg::LayoutPrev { id: num(0)? },
        "layoutJump" => BridgeMsg::LayoutJump { id: num(0)?, target: num(1)? },
        "command" => BridgeMsg::Command(args.first()?.to_string()),
        // A missing second argument means "no shell", matching the Qt side,
        // where the flag arrives as an integer that is zero when absent.
        "shell" => BridgeMsg::Shell(
            args.first()?.to_string(),
            args.get(1).map(|s| s.trim() == "1" || s.trim() == "true").unwrap_or(false),
        ),
        "stopShell" => BridgeMsg::StopShell(num(0).unwrap_or(0).clamp(0, 2) as u8),
        // mid, uri, x, y, w, h, muted
        "videoPlay" => BridgeMsg::VideoPlay {
            mid: args.first()?.to_string(),
            uri: args.get(1)?.to_string(),
            x: num(2).unwrap_or(0) as i32,
            y: num(3).unwrap_or(0) as i32,
            w: num(4).unwrap_or(0) as i32,
            h: num(5).unwrap_or(0) as i32,
            muted: args.get(6).map(|s| s.trim() == "1").unwrap_or(true),
        },
        "videoStop" => BridgeMsg::VideoStop { mid: args.first()?.to_string() },
        "log" => BridgeMsg::Log(args.join(" ")),
        _ => return None,
    })
}

/// Rewrite generated layout HTML so it loads our shim instead of Qt's
/// transport.
///
/// Done here rather than in `layout.rs` on purpose: the same file on disk then
/// serves both renderers, the translator's output (and its version marker,
/// which forces re-translation when it changes) is untouched, and the change
/// stays out of the one file we have a pull request open against upstream for.
pub fn rewrite_html(html: &str) -> String {
    html.replace(QT_TRANSPORT_SRC, BRIDGE_SCRIPT_PATH)
}

/// The shim served at [`BRIDGE_SCRIPT_PATH`].
///
/// `QWebChannel`'s real callback is asynchronous, and the generated HTML
/// defines `window.arexibo` *after* the script block that constructs the
/// channel.  A synchronous callback therefore throws
/// "Cannot read properties of undefined (reading 'id')" and the layout never
/// starts.  Hence the `setTimeout`.
pub const BRIDGE_SCRIPT: &str = r#"
// Gaxibo bridge shim -- stands in for qwebchannel.js under the WPE renderer.
(function () {
  function post(method, args) {
    try {
      var body = (args || []).map(function (a) { return String(a); }).join("\n");
      // keepalive so a navigation triggered by this very call cannot cancel it
      fetch("/arexibo/" + method, { method: "POST", body: body, keepalive: true })
        .catch(function (e) { console.warn("gaxibo bridge " + method + ": " + e); });
    } catch (e) {
      console.warn("gaxibo bridge " + method + " threw: " + e);
    }
  }
  var gui = {
    jsLayoutInit: function (id, w, h) { post("layoutInit", [id, w, h]); },
    jsLayoutDone: function (id) { post("layoutDone", [id]); },
    jsLayoutPrev: function (id) { post("layoutPrev", [id]); },
    jsLayoutJump: function (id, target) { post("layoutJump", [id, target]); },
    jsCommand: function (code) { post("command", [code]); },
    jsShell: function (cmd, withShell) { post("shell", [cmd, withShell ? 1 : 0]); },
    jsStopShell: function (kill) { post("stopShell", [kill]); }
  };
  window.qt = { webChannelTransport: {} };
  window.QWebChannel = function (transport, callback) {
    // deferred: window.arexibo is defined by a later script block
    setTimeout(function () {
      try {
        callback({ objects: { arexibo: gui } });
      } catch (e) {
        console.error("gaxibo bridge init failed: " + e);
      }
    }, 0);
  };
  window.arexiboGui = gui;

  // ---- video: decoded by the host, not by the page ----
  //
  // QtWebEngine cannot reach the VPU, and neither can WPE's media stack here.
  // So a video widget's frames come from a GStreamer branch instead, and the
  // element in the page becomes a hole.
  //
  // This intercepts HTMLVideoElement.prototype.play rather than changing what
  // layout.rs emits, for three reasons: the generated HTML stays identical for
  // both renderers, the translator's version marker need not move, and the
  // sequencing logic that decides *when* a widget plays is untouched -- it
  // still calls play(), and still waits for `ended`.
  //
  // Deliberately HTMLVideoElement, not HTMLMediaElement: Arexibo has an audio
  // widget type, and the host cannot route audio yet, so <audio> must keep
  // working the way it does today.
  gui.log = function (msg) { post("log", [msg]); };

  // Stop the page fetching the media at all, and do it *before* the fetch
  // starts.
  //
  // The host decodes the clip, so the element only needs to exist as a
  // placeholder. Two things go wrong if it is left alone:
  //
  //  - WPE downloads the file itself. For the 228 MB stress clip that
  //    saturated the loopback server for about a minute, and the bridge POST
  //    announcing the video queued behind it -- so the clip started roughly
  //    60 seconds late, every time, with nothing in any log to say why.
  //  - Removing the `src` attribute afterwards does not abort a media load
  //    that is already in flight, so cleaning up on DOMContentLoaded is too
  //    late to help.
  //
  // Hence a MutationObserver installed from the document head, before the body
  // is parsed: each <video> is stripped as it appears, so no fetch is ever
  // started. The DOMContentLoaded sweep stays as a backstop for anything the
  // observer misses.
  function strip(v) {
    if (!v || v.dataset.gaxiboSrc) return false;
    var s = v.getAttribute("src") || "";
    if (s) {
      v.dataset.gaxiboSrc = s;
      v.removeAttribute("src");
      // load() makes the element forget the resource selection it had already
      // begun, which removeAttribute alone does not.
      try { v.load(); } catch (e) {}
    }
    v.preload = "none";
    return true;
  }
  try {
    new MutationObserver(function (recs) {
      for (var i = 0; i < recs.length; i++) {
        var added = recs[i].addedNodes;
        for (var j = 0; j < added.length; j++) {
          var n = added[j];
          if (!n.tagName) continue;
          if (n.tagName === "VIDEO") strip(n);
          else if (n.querySelectorAll) {
            var vs = n.querySelectorAll("video");
            for (var k = 0; k < vs.length; k++) strip(vs[k]);
          }
        }
      }
    }).observe(document.documentElement, { childList: true, subtree: true });
  } catch (e) {
    post("log", ["MutationObserver unavailable: " + e]);
  }
  function neutralise() {
    var vids = document.querySelectorAll("video"), n = 0;
    for (var i = 0; i < vids.length; i++) if (strip(vids[i])) n++;
    post("log", ["swept " + vids.length + " video element(s), " + n + " newly stripped"]);
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", neutralise);
  } else {
    neutralise();
  }
  window.addEventListener("load", function () { post("log", ["window load fired"]); });

  var origPlay = HTMLVideoElement.prototype.play;
  var origPause = HTMLVideoElement.prototype.pause;
  HTMLVideoElement.prototype.play = function () {
    post("log", ["play() intercepted for " + this.id]);
    try {
      var r = this.getBoundingClientRect();
      // The host needs a stable name for the file; the page's src is relative
      // to the embedded server, so the basename is what identifies it.
      var src = (this.dataset.gaxiboSrc || this.getAttribute("src") ||
                 this.currentSrc || "").split("/").pop();
      // Make the element a hole so the host's frames show through. Opacity
      // rather than visibility: the sequencing code sets visibility itself,
      // and fighting it would mean tracking its state.
      this.style.opacity = "0";
      post("videoPlay", [this.id, src,
                         Math.round(r.left), Math.round(r.top),
                         Math.round(r.width), Math.round(r.height),
                         this.muted ? 1 : 0]);
      window.__gaxiboHostVideo = window.__gaxiboHostVideo || {};
      window.__gaxiboHostVideo[this.id] = true;
      return Promise.resolve();
    } catch (e) {
      console.warn("gaxibo videoPlay failed, falling back to the page: " + e);
      return origPlay.call(this);
    }
  };
  HTMLVideoElement.prototype.pause = function () {
    try {
      if (window.__gaxiboHostVideo && window.__gaxiboHostVideo[this.id]) {
        delete window.__gaxiboHostVideo[this.id];
        post("videoStop", [this.id]);
        return;
      }
    } catch (e) {
      console.warn("gaxibo videoStop failed: " + e);
    }
    return origPause.call(this);
  };

  // Called by the host when its decoder reaches the end of the clip, so the
  // page's own sequencing (which waits for `ended`) advances exactly as it
  // would have if the page had decoded the video itself.
  window.__gaxiboVideoEnded = function (mid) {
    var el = document.getElementById(mid);
    if (el) el.dispatchEvent(new Event("ended"));
  };
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_method() {
        assert_eq!(parse("/arexibo/layoutInit", "14\n1920\n1024"),
                   Some(BridgeMsg::LayoutInit { id: 14, width: 1920, height: 1024 }));
        assert_eq!(parse("/arexibo/layoutDone", "14"), Some(BridgeMsg::LayoutDone { id: 14 }));
        assert_eq!(parse("/arexibo/layoutPrev", "14"), Some(BridgeMsg::LayoutPrev { id: 14 }));
        assert_eq!(parse("/arexibo/layoutJump", "14\n24"),
                   Some(BridgeMsg::LayoutJump { id: 14, target: 24 }));
        assert_eq!(parse("/arexibo/command", "reboot"),
                   Some(BridgeMsg::Command("reboot".into())));
        assert_eq!(parse("/arexibo/shell", "ls\n1"),
                   Some(BridgeMsg::Shell("ls".into(), true)));
        assert_eq!(parse("/arexibo/shell", "ls\n0"),
                   Some(BridgeMsg::Shell("ls".into(), false)));
        assert_eq!(parse("/arexibo/stopShell", "2"), Some(BridgeMsg::StopShell(2)));
    }

    #[test]
    fn parses_video_calls_and_intercepts_only_video() {
        assert_eq!(parse("/arexibo/videoPlay", "m5\n41.mp4\n0\n0\n1920\n1024\n1"),
                   Some(BridgeMsg::VideoPlay { mid: "m5".into(), uri: "41.mp4".into(),
                                               x: 0, y: 0, w: 1920, h: 1024, muted: true }));
        assert_eq!(parse("/arexibo/videoStop", "m5"),
                   Some(BridgeMsg::VideoStop { mid: "m5".into() }));
        // an unmuted clip must not be forced silent
        match parse("/arexibo/videoPlay", "m5\n41.mp4\n0\n0\n16\n9\n0") {
            Some(BridgeMsg::VideoPlay { muted, .. }) => assert!(!muted),
            other => panic!("expected VideoPlay, got {other:?}"),
        }
        // Arexibo has an audio widget type and the host cannot route audio, so
        // the shim must leave <audio> to the page.
        assert!(BRIDGE_SCRIPT.contains("HTMLVideoElement.prototype.play"));
        assert!(!BRIDGE_SCRIPT.contains("HTMLMediaElement.prototype.play"));
        // and the host must be able to end a clip the page is waiting on
        assert!(BRIDGE_SCRIPT.contains("__gaxiboVideoEnded"));
        // The observer is what stops WPE fetching the media at all. Stripping
        // on DOMContentLoaded is too late: the parser has already started the
        // load, and that delayed the first clip by ~60s.
        assert!(BRIDGE_SCRIPT.contains("MutationObserver"),
                "video elements must be stripped before their fetch starts");
    }

    #[test]
    fn rejects_junk_without_panicking() {
        // not our prefix, unknown method, missing required argument
        assert_eq!(parse("/14.xlf.html", ""), None);
        assert_eq!(parse("/arexibo/nonsense", "1"), None);
        assert_eq!(parse("/arexibo/layoutDone", ""), None);
        assert_eq!(parse("/arexibo/layoutJump", "14"), None);
        // out-of-range kill mode is clamped, not panicked on
        assert_eq!(parse("/arexibo/stopShell", "99"), Some(BridgeMsg::StopShell(2)));
    }

    #[test]
    fn rewrites_only_the_transport_and_defers_the_callback() {
        let html = "<script src='qrc:///qtwebchannel/qwebchannel.js'></script><img src='42.png'>";
        let out = rewrite_html(html);
        assert!(out.contains("/arexibo-bridge.js"));
        assert!(!out.contains("qrc:///"));
        assert!(out.contains("42.png"), "must not disturb the rest of the document");
        // the shim's deferral is what makes the generated HTML work at all
        assert!(BRIDGE_SCRIPT.contains("setTimeout"));
    }
}
