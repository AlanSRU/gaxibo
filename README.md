# Gaxibo

A fork of [Arexibo](https://github.com/birkenfeld/arexibo) by Georg Brandl, which
renders **video through GStreamer** so the hardware video decoder is used, rather
than through QtWebEngine, which cannot reach it.

Upstream's README is kept as [README.upstream.md](README.upstream.md).

## Why

On the Rockchip RK3399 players this is built for, measured on a 1920x1024 60fps
30 Mbps H.264 clip:

| | CPU | Throughput |
|---|---|---|
| Software decode (`avdec_h264`) | 4.88 cores | 1.11x realtime |
| Hardware decode (`v4l2slh264dec`) | **0.27 cores** | **3.68x realtime** |

Software decode manages only 1.11x realtime, so full-screen HD at 60 fps has no
headroom. QtWebEngine reaches neither VA-API nor V4L2 on this platform, and there
is no VA-API driver for Rockchip, so the decoder has to be driven outside the
browser.

GStreamer already ranks `v4l2slh264dec` above `avdec_h264` (257 against 256), so
`decodebin` selects hardware decode with no rank override.

## Approach

Verified on the target hardware before starting (see
`LEDPlayer/gaxibo/PLAN.md` for the measurements and method):

- **HTML keeps working.** `wpevideosrc` (WPE WebKit as a GStreamer source)
  renders Arexibo's own generated layout HTML correctly, including the CMS's
  2.3 MB JS bundle, jQuery, moment.js, Handlebars widget templates, iframes and
  webfonts.
- **Video comes out of the DOM.** `video`/`localvideo` widgets become
  placeholders; the frames come from a `v4l2slh264dec` branch composited with
  `compositor`. Not `glvideomixer`, which measured no faster (1.58 against 1.55
  cores).
- **The Qt bridge is replaced.** `qrc:///qtwebchannel` resolves in nothing but
  Qt. WPE provides `run-javascript` for the host->JS direction already; only
  JS->host needs building, via a script-message handler.
- **Qt goes last**, once every widget type is served by the GStreamer path.

## Status

Nothing of the above is built yet. This fork currently contains upstream
`v0.5.1` (`d47fb25`) plus one scheduling fix.

### The carried fix

`Schedule::update()` advanced its index past a layout that had played through
without telling the caller to navigate, which left a display with a single
scheduled layout permanently ignoring schedule changes — no error, and the CMS
reporting it as up to date. Diagnosed on hardware against a Xibo 4.5.1 CMS.

Submitted upstream; carried here until it lands.

## Relationship to upstream

Gaxibo is **not** an official Arexibo release and is not endorsed by its author.
Please report Gaxibo bugs here rather than to Arexibo — the display layer is
being replaced, so most misbehaviour here will be ours.

Fixes that are **not** Gaxibo-specific go **upstream first**, and Gaxibo carries
them only until they land. That is a deliberate policy, not a courtesy: the
alternative is a fork that silently accumulates fixes the upstream project never
receives, which is how forks become dead ends.

To make that practical:

- **Two repositories, two jobs.** [`AlanSRU/arexibo`](https://github.com/AlanSRU/arexibo)
  is a GitHub fork of Arexibo kept solely for opening pull requests upstream.
  This repository is the divergent project. Keeping them separate means an
  upstream PR never has Gaxibo's changes dragged into its diff.
- **History is rooted at upstream.** `d47fb25` is an ancestor of `main`, so
  commits cherry-pick cleanly in either direction.
- **`master` tracks upstream untouched**; `main` is Gaxibo. `git fetch upstream`
  and compare against `master` to see what has moved.
- **Never mix concerns in a commit.** A general bug fix and a step of the
  GStreamer work must not share a commit, or the fix cannot be sent upstream
  without the rest.

Currently upstream: [birkenfeld/arexibo#36](https://github.com/birkenfeld/arexibo/pull/36),
the scheduling fix described above.

## Licence

**AGPL-3.0-or-later**, inherited from Arexibo. Upstream copyright and the
`LICENSE` file are unchanged. This repository is public because the players are
flashed with modified binaries, which obliges us to offer corresponding source.
