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
  placeholders; the frames come from a `v4l2slh264dec` branch. The plan here
  said `compositor` -- measured on the hardware, that drops frames badly
  (2.4 cores, many drops), because a `compositor`, `input-selector` or
  `videoconvert` between `v4l2slh264dec` and `waylandsink` copies 1920x1024 at
  60 fps through system memory. Composition happens in the Wayland compositor
  instead; see Status.
- **The Qt bridge is replaced.** `qrc:///qtwebchannel` resolves in nothing but
  Qt. WPE provides `run-javascript` for the host->JS direction already; only
  JS->host needs building, via a script-message handler.
- **Qt goes last**, once every widget type is served by the GStreamer path.

## Status

**Built, and running on hardware.** `--renderer wpe` selects the GStreamer path
beside the untouched Qt one, and it is what the LEDPlayer image ships and pins.
Verified on a Firefly Station P1 Pro against a Xibo 4.5.1 CMS.

- **Hardware decode**, through an explicit `qtdemux ! h264parse !
  v4l2slh264dec ! waylandsink` chain. `decodebin` cannot negotiate with
  `waylandsink` at all -- the decoder's output is dmabuf and autoplug works
  from static caps, which do not advertise it.
- **Video in a region, not only full screen.** Gaxibo owns one `xdg_toplevel`
  and hands every sink that surface plus a render rectangle, so the page and
  each clip become subsurfaces of one window. Composition is the Wayland
  compositor's, on the GPU: measured, wlroots was already importing the
  decoder's dmabuf and sampling it on the Mali even for a full-screen clip, so
  there never was a direct-scanout path to protect.
- **Several clips at once**, keyed by widget id.
- **HTML renders**, including the CMS's own bundle, and in Xibo 4.5 nearly
  every complex widget is `renderAs=html` and therefore already handled.

Measured on that board, of 6 cores:

| | total | note |
|---|---|---|
| full-screen clip, page paused | **0.27** | the page is not worth drawing under it |
| 940x500 clip in a region, page live | **0.75** | |
| two concurrent clips | **0.93** | both ran 60 s in 60 s |
| one clip plus a marquee | **2.9** | the marquee alone is ~2.2 |

### Not done

- **Screenshots.** The Qt path renders the webview; here it needs a `tee` into
  `pngenc`. The CMS asks for them.
- **Sound, of any kind.** The page pipeline takes only `wpevideosrc`'s video
  pad. Note that Xibo's `audio` and `videoin` widget types have no arm in
  `layout.rs` at all and report "unsupported media type" in **both** renderers,
  upstream included -- that is a separate gap from this one.
- **Tickers are expensive.** A marquee costs ~2.2 cores of WPE page rendering
  while a clock repainting every second costs nothing, so the page renderer is
  the limit rather than the VPU or the composite.
- **Qt is still in the image**, which is most of the ~270 MB the GStreamer
  path added. Removing it needs every widget type served by this renderer.

### The carried fixes

Two, both upstream-bound and kept here until they land:

- **`Schedule::update()`** advanced its index past a layout that had played
  through without telling the caller to navigate, which left a display with a
  single scheduled layout permanently ignoring schedule changes — no error, and
  the CMS reporting it as up to date. Submitted upstream.
- **A video region ended on a sampled duration** rather than on the clip
  ending. `video.duration` is NaN until metadata loads and the region switch
  treats NaN as falsy, so a clip could silently play for one second; Xibo
  publishes video widgets with `duration=0` ("use the media's own length"), so
  this is the ordinary case rather than an edge one. Drafted, held until it had
  longer on hardware -- which it now has.

Both diagnosed on hardware against a Xibo 4.5.1 CMS.

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

### Knowing where the three forks stand

`tools/fork-status.py --fetch` compares this fork, upstream and
[romoloman/arexibo](https://github.com/romoloman/arexibo), which share the base
commit `d47fb25`. The capability matrix is **derived from the three trees** on
every run rather than written down, so it cannot go stale; [FORKS.md](FORKS.md)
holds the parts that cannot be derived — a verdict per commit, and the gap
against what the CMS expects of a player. Worth running before fixing anything,
because the same bug has been found in more than one of these trees.

Currently upstream: [birkenfeld/arexibo#36](https://github.com/birkenfeld/arexibo/pull/36),
the scheduling fix described above. The video-duration fix is drafted against
`master` on the PR fork and not yet sent.

## Licence

**AGPL-3.0-or-later**, inherited from Arexibo. Upstream copyright and the
`LICENSE` file are unchanged. This repository is public because the players are
flashed with modified binaries, which obliges us to offer corresponding source.
