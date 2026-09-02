// Xibo player Rust implementation, (c) 2022-2024 Georg Brandl.
// Licensed under the GNU AGPL, version 3 or later.

//! Internal webserver to point the webview to.

use std::{sync::Arc, fs, io::Read, io::Seek, thread, collections::HashMap};
use std::path::{Path, PathBuf};
use anyhow::{anyhow, bail, ensure, Context, Result};
use itertools::Itertools;
use tiny_http::{Request, Response, ResponseBox, Header, StatusCode};

use crossbeam_channel::Sender;

use crate::bridge::{self, BridgeMsg};
use crate::util::percent_decode;


pub struct Server {
    dir: PathBuf,
    server: tiny_http::Server,
    /// Set by the WPE renderer, which has no QWebChannel and so receives the
    /// page's calls over HTTP instead.  `None` under the Qt renderer, where
    /// the bridge routes are not served at all.
    bridge: Option<Sender<BridgeMsg>>,
}

impl Server {
    pub fn new(dir: PathBuf, port: u16) -> Result<Self> {
        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| anyhow!(e))?;
        let dir = dir.canonicalize().context("getting canonical server dir name")?;
        Ok(Self { dir, server, bridge: None })
    }

    /// Enable the page-to-host bridge routes.  See [`crate::bridge`].
    pub fn with_bridge(mut self, tx: Sender<BridgeMsg>) -> Self {
        self.bridge = Some(tx);
        self
    }

    pub fn port(&self) -> u16 {
        self.server.server_addr().to_ip().expect("IP address").port()
    }

    pub fn start_pool(self) {
        let server = Arc::new(self.server);
        for _ in 0..4 {
            let server = server.clone();
            let dir = self.dir.clone();
            let bridge = self.bridge.clone();
            thread::spawn(move || {
                loop {
                    let mut req = server.recv().unwrap();
                    match Self::serve(&dir, bridge.as_ref(), &mut req) {
                        Ok(resp) => {  let _ = req.respond(resp); }
                        Err(e) => {
                            log::warn!("processing HTTP req {}: {:#}", req.url(), e);
                            let _ = req.respond(Response::empty(500));
                        }
                    }
                }
            });
        }
    }

    /// Serve a single HTTP request.
    fn serve(dir: &Path, bridge: Option<&Sender<BridgeMsg>>,
             req: &mut Request) -> Result<ResponseBox> {
        log::debug!("HTTP request: {}", req.url());

        // Bridge routes, served only when a renderer asked for them.  Checked
        // before the static handler because these paths must never fall
        // through to the cache directory.
        if let Some(tx) = bridge {
            let url = req.url().to_owned();
            if url == bridge::BRIDGE_SCRIPT_PATH {
                return Ok(Response::from_data(bridge::BRIDGE_SCRIPT.as_bytes())
                          .with_header(Header::from_bytes(
                              b"Content-Type", b"application/javascript").unwrap())
                          .boxed());
            }
            if url.starts_with(bridge::BRIDGE_PREFIX) {
                let mut body = String::new();
                // A malformed body is the page's problem, not ours: log it and
                // answer, rather than letting the error path return 500 to a
                // page that cannot do anything about it.
                if let Err(e) = req.as_reader().read_to_string(&mut body) {
                    log::warn!("bridge {url}: unreadable body: {e}");
                    return Ok(Response::empty(400).boxed());
                }
                match bridge::parse(&url, body.trim_end()) {
                    Some(msg) => {
                        log::debug!("bridge {url}: {msg:?}");
                        // A full channel means the renderer is wedged; dropping
                        // the call is better than blocking a server thread.
                        if tx.try_send(msg).is_err() {
                            log::warn!("bridge {url}: renderer not accepting calls");
                        }
                        return Ok(Response::empty(204).boxed());
                    }
                    None => {
                        log::warn!("bridge {url}: unrecognised call");
                        return Ok(Response::empty(404).boxed());
                    }
                }
            }
        }

        Ok(match req.url() {
            // built-in files?
            "/favicon.ico" => Response::from_data(b"").boxed(),
            "/splash.jpg" => Response::from_data(SPLASH_JPG).boxed(),
            "/0.xlf.html" => Response::from_data(SPLASH_HTML).boxed(),

            // SDK duration callbacks — just ACK them; arexibo uses XLF durations
            "/duration/set" | "/duration/extend" | "/duration/expire" =>
                Response::from_data(b"{}").with_header(
                    Header::from_bytes(b"Content-Type", b"application/json").unwrap()
                ).boxed(),

            // any other static files
            url => {
                let parts = url.split('?').collect_vec();
                let path = dir.join(&parts[0][1..]);

                let canonical_path = match path.canonicalize() {
                    Ok(p) if p.starts_with(dir) => p,
                    Ok(_) => {
                        log::warn!("processing HTTP req {}: 403 path outside cache dir", req.url());
                        return Ok(Response::empty(403).boxed());
                    }
                    Err(e) => {
                        log::warn!("processing HTTP req {}: 404 canonicalize: {e}", req.url());
                        return Ok(Response::empty(404).boxed());
                    }
                };
                let ext = canonical_path.extension().and_then(|e| e.to_str());

                let query_params = parts.get(1).map(|par| par.split('&').map(|p| {
                    let mut kv = p.split('=');
                    let k = percent_decode(kv.next().unwrap_or(""));
                    let v = percent_decode(kv.next().unwrap_or(""));
                    (k, v)
                }).collect::<HashMap<_, _>>()).unwrap_or_default();

                if !canonical_path.is_file() {
                    log::warn!("processing HTTP req {}: 404 not found", req.url());
                    return Ok(Response::empty(404).boxed());
                }
                let mut fp = fs::File::open(&canonical_path)?;

                // Under the WPE renderer, generated layouts must load our
                // bridge shim rather than Qt's transport.  Rewritten on the
                // way out rather than in layout.rs, so the same file on disk
                // serves both renderers and the translator's version marker
                // (which forces re-translation when its output changes) does
                // not have to move.
                if bridge.is_some() && parts[0].ends_with(".xlf.html") {
                    let mut text = String::new();
                    fp.read_to_string(&mut text)?;
                    if let Some(w) = query_params.get("w") {
                        text = text.replace("[[ViewPortWidth]]", w);
                    }
                    return Ok(Response::from_data(bridge::rewrite_html(&text).into_bytes())
                              .with_header(Header::from_bytes(
                                  b"Content-Type", b"text/html").unwrap())
                              .boxed());
                }

                // implement replacing [[ViewPortWidth]] by requested width
                if ext == Some("html") && query_params.contains_key("w") {
                    let mut data = Vec::new();
                    fp.read_to_end(&mut data)?;
                    if let Some(i) = (0..data.len())
                        .find(|&i| data[i..].starts_with(b"[[ViewPortWidth]]")) {
                        let mut new_data = data[..i].to_vec();
                        new_data.extend_from_slice(query_params["w"].as_bytes());
                        new_data.extend_from_slice(&data[i + 17..]);
                        data = new_data;
                    }

                    return Ok(Response::from_data(data)
                        .with_header(Header::from_bytes(b"Content-Type",
                                                        b"text/html").unwrap())
                        .boxed());
                }

                // implement HTTP Range query for gstreamer
                for h in req.headers() {
                    if h.field.equiv("Range") {
                        let total_size = fp.metadata()?.len();
                        let (from, to, size) = parse_range(total_size, h.value.as_ref())?;
                        fp.seek(std::io::SeekFrom::Start(from))?;
                        let stream = fp.take(size);

                        let range = format!("bytes {from}-{to}/{total_size}");
                        return Ok(Response::new(
                            StatusCode(206),
                            vec![
                                Header::from_bytes(b"Content-Range", range).unwrap(),
                                Header::from_bytes(b"Content-Type", b"video/mp4").unwrap(),
                            ],
                            stream,
                            Some(size as usize),
                            None
                        ).with_chunked_threshold(usize::MAX).boxed());
                    }
                }

                // guess the MIME type based on filename
                let ctype = match ext {
                    Some("html") => "text/html",
                    Some("js" | "mjs") => "text/javascript",
                    Some("ttf" | "otf") => "application/font-sfnt",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("png") => "image/png",
                    Some("pdf") => "application/pdf",
                    Some("mp4") => "video/mp4",
                    Some("avi") => "video/avi",
                    Some("ogv") => "video/ogg",
                    Some("webm") => "video/webm",
                    _ => "",
                };

                Response::from_file(fp)
                    // for gstreamer, need a response with Content-Length => no chunked
                    .with_chunked_threshold(usize::MAX)
                    .with_header(Header::from_bytes(b"Content-Type", ctype).unwrap())
                    .boxed()
            }
        })
    }
}

const SPLASH_HTML: &[u8] = br#"<!DOCTYPE html>
<html>
<head>
<script src="qrc:///qtwebchannel/qwebchannel.js"></script>
<script>
new QWebChannel(qt.webChannelTransport, function(channel) {
  window.arexiboGui = channel.objects.arexibo;
  window.arexiboGui.jsLayoutInit(0, 1920, 1080);
});
</script>
</head>
<body style="margin: 0">
<img style="display: block; width: 100%; height: 100%" src="splash.jpg">
</body>
</html>
"#;

const SPLASH_JPG: &[u8] = include_bytes!("../assets/splash.jpg");


/// Parse a HTTP Range header.
fn parse_range(total_size: u64, header: &str) -> Result<(u64, u64, u64)> {
    let mut parts = header.split(&['=', '-'][..]);
    let (from, to) = match parts.next_tuple() {
        Some(("bytes", from, to)) => {
            (from.parse().unwrap_or(0), to.parse().unwrap_or(total_size - 1))
        }
        _ => bail!("invalid Range header")
    };
    ensure!(from <= to && to < total_size, "invalid Range from/to");
    let size = to - from + 1;
    Ok((from, to, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;

    /// Minimal HTTP client: the point is to exercise the real server over a
    /// real socket, so a compile is not mistaken for a working route.
    fn request(port: u16, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n\
                           Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                          body.len());
        sock.write_all(req.as_bytes()).expect("write");
        let mut resp = String::new();
        let _ = std::io::Read::read_to_string(&mut sock, &mut resp);
        let status = resp.split_whitespace().nth(1)
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    #[test]
    fn bridge_routes_serve_and_dispatch() {
        let dir = std::env::temp_dir().join(format!("gaxibo-srv-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("14.xlf.html"),
                  "<script src='qrc:///qtwebchannel/qwebchannel.js'></script><img src='42.png'>")
            .unwrap();
        fs::write(dir.join("42.png"), b"notreallyapng").unwrap();

        let (tx, rx) = crossbeam_channel::unbounded();
        let srv = Server::new(dir.clone(), 0).unwrap().with_bridge(tx);
        let port = srv.port();
        srv.start_pool();

        // the shim is served
        let (st, body) = request(port, "GET", bridge::BRIDGE_SCRIPT_PATH, "");
        assert_eq!(st, 200);
        assert!(body.contains("jsLayoutInit"), "shim body was: {body:.120}");

        // a layout is rewritten to use it, and nothing else is disturbed
        let (st, body) = request(port, "GET", "/14.xlf.html", "");
        assert_eq!(st, 200);
        assert!(body.contains(bridge::BRIDGE_SCRIPT_PATH));
        assert!(!body.contains("qrc:///"));
        assert!(body.contains("42.png"));

        // a call from the page reaches the renderer
        let (st, _) = request(port, "POST", "/arexibo/layoutInit", "14\n1920\n1024");
        assert_eq!(st, 204);
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
                   BridgeMsg::LayoutInit { id: 14, width: 1920, height: 1024 });

        // an unknown bridge call is refused rather than falling through to the
        // cache directory
        let (st, _) = request(port, "POST", "/arexibo/nonsense", "");
        assert_eq!(st, 404);

        let _ = fs::remove_dir_all(&dir);
    }
}
