use std::{net::SocketAddr, sync::Arc};

use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::AbortHandle,
};
use uuid::Uuid;

use crate::{
    Computer, ComputerControl, ComputerError, ComputerFrames, ComputerState, InterventionReason,
};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;

/// A loopback-only live visual observer with human control buttons.
///
/// The preview is a thin consumer of [`ComputerFrames`]. Slow rendering or a
/// closed browser cannot block the action actor. A random path capability
/// protects its pause/resume endpoints from unrelated local web pages.
pub struct ComputerPreview {
    url: String,
    task: AbortHandle,
    _computer: Computer,
}

impl ComputerPreview {
    /// Starts a preview server on an ephemeral `127.0.0.1` port.
    pub async fn spawn(computer: &Computer) -> Result<Self, ComputerError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| ComputerError::Native {
                message: format!("failed to bind computer preview: {error}"),
            })?;
        let address = listener
            .local_addr()
            .map_err(|error| ComputerError::Native {
                message: format!("failed to read computer preview address: {error}"),
            })?;
        let token = Uuid::now_v7().simple().to_string();
        let url = format!("http://{address}/{token}/");
        let frames = computer.frames();
        let control = computer.control();
        let task = tokio::spawn(serve(listener, token, frames, control));
        Ok(Self {
            url,
            task: task.abort_handle(),
            _computer: computer.clone(),
        })
    }

    /// Starts the preview and asks macOS to open it in the user's browser.
    pub async fn spawn_and_open(computer: &Computer) -> Result<Self, ComputerError> {
        let preview = Self::spawn(computer).await?;
        #[cfg(target_os = "macos")]
        {
            let status = tokio::process::Command::new("/usr/bin/open")
                .arg(preview.url())
                .status()
                .await
                .map_err(|error| ComputerError::Native {
                    message: format!("failed to open computer preview: {error}"),
                })?;
            if !status.success() {
                return Err(ComputerError::Native {
                    message: format!("preview browser launcher exited with {status}"),
                });
            }
        }
        Ok(preview)
    }

    /// Returns the unguessable loopback preview URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Stops the preview without stopping computer actions.
    pub fn close(self) {}
}

impl Drop for ComputerPreview {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(
    listener: TcpListener,
    token: String,
    frames: ComputerFrames,
    control: ComputerControl,
) {
    let token: Arc<str> = token.into();
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            return;
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let token = Arc::clone(&token);
        let frames = frames.clone();
        let control = control.clone();
        tokio::spawn(async move {
            if let Err(error) = handle(stream, peer, &token, &frames, &control).await {
                tracing::debug!(%peer, %error, "computer preview request failed");
            }
        });
    }
}

async fn handle(
    mut stream: TcpStream,
    _peer: SocketAddr,
    token: &str,
    frames: &ComputerFrames,
    control: &ComputerControl,
) -> Result<(), std::io::Error> {
    let mut request = vec![0_u8; MAX_REQUEST_BYTES];
    let count = stream.read(&mut request).await?;
    request.truncate(count);
    let request = String::from_utf8_lossy(&request);
    let mut line = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = line.next().unwrap_or_default();
    let path = line
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let root = format!("/{token}/");
    if !path.starts_with(&root) {
        return respond(&mut stream, 404, "text/plain", b"not found", &[]).await;
    }
    let route = &path[root.len()..];
    match (method, route) {
        ("GET", "") => {
            let html = preview_html(token);
            respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                html.as_bytes(),
                &[("Content-Security-Policy", "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:")],
            )
            .await
        }
        ("GET", "state") => {
            let Some(state) = frames.latest() else {
                return respond(&mut stream, 204, "application/json", b"", no_store()).await;
            };
            let body = serde_json::to_vec(&PreviewState::from_state(&state, token))
                .unwrap_or_else(|_| b"{}".to_vec());
            respond(&mut stream, 200, "application/json", &body, no_store()).await
        }
        ("GET", "frame") => {
            let Some(state) = frames.latest() else {
                return respond(&mut stream, 204, "image/png", b"", no_store()).await;
            };
            let Some(image) = state.screenshot else {
                return respond(&mut stream, 204, "image/png", b"", no_store()).await;
            };
            let metadata = tokio::fs::metadata(&image.path).await?;
            if metadata.len() > MAX_FRAME_BYTES {
                return respond(
                    &mut stream,
                    413,
                    "text/plain",
                    b"frame too large",
                    no_store(),
                )
                .await;
            }
            let body = tokio::fs::read(image.path).await?;
            respond(&mut stream, 200, "image/png", &body, no_store()).await
        }
        ("POST", "pause") => {
            control.pause();
            respond(&mut stream, 204, "text/plain", b"", no_store()).await
        }
        ("POST", "resume") => {
            control.resume();
            respond(&mut stream, 204, "text/plain", b"", no_store()).await
        }
        ("POST", "takeover") => {
            control.intervene(InterventionReason::HumanInput);
            respond(&mut stream, 204, "text/plain", b"", no_store()).await
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found", no_store()).await,
    }
}

const fn no_store() -> &'static [(&'static str, &'static str)] {
    &[("Cache-Control", "no-store")]
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    headers: &[(&str, &str)],
) -> Result<(), std::io::Error> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        413 => "Content Too Large",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

#[derive(Serialize)]
struct PreviewState<'a> {
    generation: u64,
    application: &'a str,
    window: &'a str,
    elements: usize,
    settled: bool,
    digest: Option<&'a str>,
    frame_url: String,
}

impl<'a> PreviewState<'a> {
    fn from_state(state: &'a ComputerState, token: &str) -> Self {
        Self {
            generation: state.generation,
            application: &state.application.name,
            window: state.window.title.as_deref().unwrap_or("Untitled window"),
            elements: state.elements.len(),
            settled: state.settled,
            digest: state.screenshot.as_ref().map(|image| image.digest.as_str()),
            frame_url: format!("/{token}/frame"),
        }
    }
}

fn preview_html(token: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Nanocodex Computer</title><style>
:root{{color-scheme:dark;background:#111;color:#eee;font:14px ui-monospace,SFMono-Regular,monospace}}
body{{margin:0;display:grid;grid-template-rows:auto 1fr;height:100vh;overflow:hidden}}
header{{display:flex;gap:12px;align-items:center;padding:10px 14px;background:#1b1b1d;border-bottom:1px solid #333}}
#title{{flex:1;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}} button{{border:1px solid #555;background:#29292c;color:#eee;border-radius:7px;padding:7px 11px;cursor:pointer}} button:hover{{background:#38383c}} #takeover{{border-color:#d47531}}
main{{display:grid;place-items:center;min-height:0;background:repeating-conic-gradient(#171719 0 25%,#141416 0 50%) 50%/20px 20px}}
img{{max-width:100%;max-height:100%;object-fit:contain;box-shadow:0 10px 40px #0008}} #status{{color:#aaa}}
</style></head><body><header><span id="title">Waiting for the first frame…</span><span id="status"></span><button id="pause">Pause</button><button id="resume">Resume</button><button id="takeover">Take over</button></header><main><img id="frame" alt="Live target window"></main>
<script>
const root='/{token}/', img=document.querySelector('#frame'), title=document.querySelector('#title'), status=document.querySelector('#status'); let digest='';
async function poll(){{try{{const r=await fetch(root+'state',{{cache:'no-store'}});if(r.status===200){{const s=await r.json();title.textContent=s.application+' — '+s.window;status.textContent='gen '+s.generation+' · '+s.elements+' AX · '+(s.settled?'settled':'moving');if(s.digest&&s.digest!==digest){{digest=s.digest;img.src=s.frame_url+'?d='+encodeURIComponent(digest)}}}}}}catch(_e){{status.textContent='disconnected'}}setTimeout(poll,250)}}
for(const id of ['pause','resume','takeover'])document.querySelector('#'+id).onclick=()=>fetch(root+id,{{method:'POST'}});poll();
</script></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_capability_is_scoped_into_every_endpoint() {
        let html = preview_html("secret-token");
        assert!(html.contains("/secret-token/"));
        assert!(!html.contains("http://0.0.0.0"));
    }

    #[tokio::test]
    async fn loopback_preview_serves_ui_and_takeover_controls() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let preview = ComputerPreview::spawn(&computer).await.unwrap();

        let response = request(preview.url(), "GET", "").await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Security-Policy:"));
        assert!(response.contains("Nanocodex Computer"));

        let response = request(preview.url(), "POST", "takeover").await;
        assert!(response.starts_with("HTTP/1.1 204 No Content"));
        assert!(computer.control().is_paused());
    }

    async fn request(url: &str, method: &str, route: &str) -> String {
        let target = url.strip_prefix("http://").unwrap();
        let (address, capability) = target.split_once('/').unwrap();
        let path = format!("/{capability}{route}");
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }
}
