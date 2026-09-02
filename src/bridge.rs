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
