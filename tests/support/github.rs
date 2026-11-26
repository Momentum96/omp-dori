//! A fake GitHub release API on loopback only: the release list and `releases/tags/<tag>` for one
//! scripted release whose assets are served from a local folder (the first download of each asset
//! answers with a redirect, as GitHub's asset links do). The list also holds, newer and with higher
//! tags, a prerelease and a release of other software, as the real repository does; neither may be
//! chosen. Every request path is counted.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::json;

#[derive(Default)]
struct State {
    /// Tag and the folder of its assets; `None` answers 404 (no release yet).
    release: Option<(String, PathBuf)>,
    seen: Vec<String>,
}

pub struct FakeGithub {
    pub port: u16,
    state: Arc<Mutex<State>>,
}

impl FakeGithub {
    pub fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let state = Arc::clone(&shared);
                    tokio::spawn(async move {
                        let service = service_fn(move |req| {
                            std::future::ready(Ok::<_, std::convert::Infallible>(answer(
                                &req, &state, port,
                            )))
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            });
        });
        Self { port, state }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Publishes `tag` with the assets in `folder`.
    pub fn publish(&self, tag: &str, folder: PathBuf) {
        self.state.lock().unwrap().release = Some((tag.to_string(), folder));
    }

    /// Requests seen so far, by path.
    pub fn seen(&self) -> Vec<String> {
        self.state.lock().unwrap().seen.clone()
    }
}

fn answer(req: &Request<Incoming>, state: &Mutex<State>, port: u16) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or_default().to_string();
    let release = {
        let mut s = state.lock().unwrap();
        s.seen.push(path.clone());
        s.release.clone()
    };
    let reply = |status: u16, body: Vec<u8>| {
        Response::builder()
            .status(status)
            .body(Full::new(Bytes::from(body)))
            .unwrap()
    };
    let Some((tag, folder)) = release else {
        return reply(404, b"{\"message\":\"Not Found\"}".to_vec());
    };
    let list = path.ends_with("/releases");
    if list || path.ends_with(&format!("/releases/tags/{tag}")) {
        let mut names: Vec<String> = std::fs::read_dir(&folder)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let assets: Vec<_> = names
            .iter()
            .map(|n| {
                json!({
                    "name": n,
                    "browser_download_url": format!("http://127.0.0.1:{port}/download/{n}"),
                })
            })
            .collect();
        let body =
            json!({ "tag_name": tag, "prerelease": false, "draft": false, "assets": assets });
        if !list {
            return reply(200, body.to_string().into_bytes());
        }
        let other = json!([{ "name": "other-tool.tar.gz", "browser_download_url": "http://127.0.0.1:9/x" }]);
        let body = json!([
            { "tag_name": "v98.0.0", "prerelease": true, "draft": false, "assets": assets },
            { "tag_name": "v99.0.0", "prerelease": false, "draft": false, "assets": other },
            body,
        ]);
        return reply(200, body.to_string().into_bytes());
    }
    if let Some(name) = path.strip_prefix("/download/") {
        // Like GitHub: the asset link redirects to storage with a signed query.
        let location = format!("http://127.0.0.1:{port}/storage/{name}?sig=abc");
        return Response::builder()
            .status(302)
            .header("location", location)
            .body(Full::new(Bytes::new()))
            .unwrap();
    }
    if let Some(name) = path.strip_prefix("/storage/") {
        if query != "sig=abc" {
            return reply(403, Vec::new());
        }
        return match std::fs::read(folder.join(name)) {
            Ok(bytes) => reply(200, bytes),
            Err(_) => reply(404, Vec::new()),
        };
    }
    reply(404, Vec::new())
}
