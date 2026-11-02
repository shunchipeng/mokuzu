//! Live view of the virtual desktop for the user to watch: a small web page on the local network
//! that shows the whole desktop (see `DESKTOP`). Nothing done there reaches the browsers on it. The link carries a random token; without it every request is refused.

use std::collections::HashMap;
use std::net::{IpAddr, UdpSocket};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::OnceCell;

use super::{random, screen};

/// The viewer's link, starting it on first use; it then serves until mokuzu exits.
// ponytail: never stops once started; stop it with the session if an open port bothers anyone.
pub(super) async fn link(port: u16) -> Result<String> {
    static LINK: OnceCell<String> = OnceCell::const_new();
    LINK.get_or_try_init(|| start(port)).await.cloned()
}

/// Serves the viewer on `port`, or a free one when 0.
async fn start(port: u16) -> Result<String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .context("failed to open the viewer's port; is browser.viewer_port taken?")?;
    let port = listener.local_addr()?.port();
    // ponytail: 128 bits from std's OS-seeded hasher keys; switch to getrandom if this ever
    // guards more than a LAN page.
    let token = format!("{:016x}{:016x}", random(), random());
    let host = lan_ip().map_or_else(|| "localhost".to_owned(), |ip| ip.to_string());
    let expected = token.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let token = expected.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(stream, &token).await {
                    tracing::debug!("viewer request failed: {err:#}");
                }
            });
        }
    });
    tracing::debug!(port, "browser viewer is up");
    Ok(format!("http://{host}:{port}/?token={token}"))
}

/// The address other machines on the network reach this one at: the one the default route
/// leaves from. Connecting a UDP socket sends nothing.
pub(crate) fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// Answers one HTTP/1.1 request, then closes the connection.
async fn serve(connection: TcpStream, token: &str) -> Result<()> {
    let (read, mut write) = connection.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        bail!("bad request line {line:?}");
    };
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await? == 0 || header.trim_end().is_empty() {
            break;
        }
    }

    let url = Url::parse(&format!("http://viewer{target}"))?;
    let query: HashMap<_, _> = url.query_pairs().collect();
    let allowed = query
        .get("token")
        .is_some_and(|given| same(given.as_bytes(), token.as_bytes()));
    let display = screen().await;
    let (status, kind, content) = if allowed {
        match (method, url.path(), display) {
            ("GET", "/stream", Some(display)) => return stream(&display, &mut write).await,
            ("GET", "/stream", None) => (
                "503 Service Unavailable",
                "text/plain",
                b"no virtual desktop".to_vec(),
            ),
            ("GET", "/", _) => (
                "200 OK",
                "text/html; charset=utf-8",
                PAGE.as_bytes().to_vec(),
            ),
            _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
        }
    } else {
        (
            "403 Forbidden",
            "text/plain",
            b"wrong or missing token".to_vec(),
        )
    };
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\
         cache-control: no-store\r\nreferrer-policy: no-referrer\r\nconnection: close\r\n\r\n",
        content.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(&content).await?;
    Ok(())
}

/// Streams the whole `display` as MJPEG until the viewer goes, a few frames a second.
async fn stream(display: &str, write: &mut OwnedWriteHalf) -> Result<()> {
    let mut ffmpeg = tokio::process::Command::new("ffmpeg")
        .args([
            "-loglevel",
            "error",
            "-f",
            "x11grab",
            "-framerate",
            "5",
            "-i",
            display,
        ])
        .args([
            "-q:v",
            "8",
            "-f",
            "mpjpeg",
            "-boundary_tag",
            "frame",
            "-flush_packets",
            "1",
        ])
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to run ffmpeg")?;
    let mut frames = ffmpeg.stdout.take().context("ffmpeg has no output")?;
    write
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: multipart/x-mixed-replace; boundary=frame\r\n\
              cache-control: no-store\r\nconnection: close\r\n\r\n",
        )
        .await?;
    // Ends when the viewer goes and a write fails; dropping `ffmpeg` then kills it.
    tokio::io::copy(&mut frames, write).await?;
    Ok(())
}

/// Constant-time comparison, so response timing gives the token away a byte at a time.
fn same(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="referrer" content="no-referrer">
<title>mokuzu browser</title>
<style>
  /* Tokens from ui-ux-pro-max's design system for a developer tool: Swiss minimal, slate, dense,
     dark first with a light theme, green for a live connection. */
  :root {
    color-scheme: dark light;
    --background: #0f172a; --card: #1b2336; --muted: #272f42; --border: #475569;
    --foreground: #f8fafc; --muted-foreground: #94a3b8; --ring: #f8fafc;
    --accent: #22c55e; --on-accent: #0f172a; --warning: #f59e0b; --destructive: #ef4444;
    --shadow: 0 1px 2px rgb(0 0 0 / .4), 0 12px 32px rgb(0 0 0 / .35);
    --font: "IBM Plex Sans", system-ui, -apple-system, "Segoe UI", sans-serif;
    --mono: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace;
    --space-1: 4px; --space-2: 8px; --space-3: 12px; --space-4: 16px;
    --radius: 8px; --control: 40px; --fast: 150ms;
  }
  @media (prefers-color-scheme: light) {
    :root { --background: #f1f5f9; --card: #ffffff; --muted: #e2e8f0; --border: #94a3b8;
      --foreground: #0f172a; --muted-foreground: #475569; --ring: #0f172a; --accent: #15803d;
      --on-accent: #ffffff; --warning: #b45309; --destructive: #b91c1c;
      --shadow: 0 1px 2px rgb(15 23 42 / .08), 0 12px 32px rgb(15 23 42 / .12); }
  }
  * { box-sizing: border-box; }
  html, body { height: 100%; }
  body { margin: 0; display: flex; flex-direction: column; background: var(--background);
    color: var(--foreground); font: 14px/1.5 var(--font); }
  header { display: flex; align-items: center; gap: var(--space-2); background: var(--card);
    padding: var(--space-2) max(var(--space-4), env(safe-area-inset-right))
      var(--space-2) max(var(--space-4), env(safe-area-inset-left)); }
  header { border-bottom: 1px solid var(--muted); padding-top: max(var(--space-2), env(safe-area-inset-top)); }
  .brand { display: flex; align-items: center; gap: var(--space-2); font-weight: 600;
    letter-spacing: -.01em; white-space: nowrap; padding-right: var(--space-2); }
  svg { width: 18px; height: 18px; fill: none; stroke: currentColor; stroke-width: 2;
    stroke-linecap: round; stroke-linejoin: round; flex: none; }
  :disabled { opacity: .4; cursor: not-allowed; }
  .grow { flex: 1; }
  main { flex: 1; min-height: 0; position: relative; }
  /* Pinned to main's box so the picture shrinks to fit it whatever its shape. */
  .stage { position: absolute; inset: var(--space-4); display: flex; align-items: center;
    justify-content: center; padding-bottom: env(safe-area-inset-bottom); }
  #screen { display: block; max-width: 100%; max-height: 100%; border-radius: var(--radius);
    border: 1px solid var(--muted); box-shadow: var(--shadow); background: var(--card); }
  #screen[hidden], #empty[hidden] { display: none; }
  #empty { text-align: center; color: var(--muted-foreground); max-width: 340px; }
  #empty svg { width: 32px; height: 32px; stroke-width: 1.5; margin-bottom: var(--space-3); }
  #empty strong { display: block; color: var(--foreground); font-size: 16px; margin-bottom: var(--space-1); }
  .pill { display: inline-flex; align-items: center; gap: var(--space-2); height: 28px;
    padding: 0 var(--space-3); border-radius: 999px; background: var(--muted); font-size: 12px;
    font-weight: 500; white-space: nowrap; }
  .pill::before { content: ""; width: 8px; height: 8px; border-radius: 50%;
    background: var(--state, var(--muted-foreground)); }
  .pill[data-state="live"] { --state: var(--accent); }
  .pill[data-state="wait"] { --state: var(--warning); }
  .pill[data-state="error"] { --state: var(--destructive); }
  @media (prefers-reduced-motion: reduce) { * { transition: none !important; } }
  /* Fingers need bigger targets, and iOS zooms into fields with text under 16px. */
  @media (max-width: 600px) {
    header { flex-wrap: wrap; }
    .brand { display: none; }
    .pill { order: 1; }
    .stage { inset: var(--space-2); }
  }
</style>
<header>
  <span class="brand"><svg viewBox="0 0 24 24" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 9h18M7 6.5h.01M10 6.5h.01"/></svg>mokuzu</span>
  <span class="grow"></span>
  <span id="status" class="pill" data-state="wait" role="status" aria-atomic="true">Connecting</span>
</header>
<main><div class="stage">
  <img id="screen" alt="Live view of mokuzu's desktop" hidden>
  <div id="empty">
    <svg viewBox="0 0 24 24" aria-hidden="true"><rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/></svg>
    <strong id="empty-title">Connecting to mokuzu…</strong>
    <span id="empty-text"></span>
  </div>
</div></main>
<script>
const token = new URLSearchParams(location.search).get('token');
const $ = id => document.getElementById(id);
const status = (text, state) => { $('status').textContent = text; $('status').dataset.state = state; };

// One long response the browser shows frame by frame. Shown at once rather than on `load`,
// which browsers fire unevenly for a stream of frames.
function show() {
  $('screen').src = `/stream?token=${token}&t=${Date.now()}`;
  $('screen').hidden = false; $('empty').hidden = true; status('Live', 'live');
}
$('screen').onerror = () => {
  $('screen').hidden = true; $('screen').removeAttribute('src'); $('empty').hidden = false;
  $('empty-title').textContent = 'Nothing to show';
  $('empty-text').textContent = 'mokuzu shows its virtual desktop, run on Linux without a screen '
    + 'with Xvfb and ffmpeg installed, or mokuzu stopped. Trying again.';
  status('Waiting', 'wait');
  setTimeout(show, 3000);
};
show();
</script>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abd", b"abc"));
        assert!(!same(b"ab", b"abc"));
    }

    #[tokio::test]
    async fn requests_need_the_token() {
        // Its own viewer: `link`'s dies with the first test runtime that started it.
        let link = start(0).await.expect("viewer starts");
        let client = reqwest::Client::new();
        let page = client.get(&link).send().await.expect("page");
        assert_eq!(page.status(), 200);
        assert!(page.text().await.expect("body").contains("mokuzu browser"));
        let stream = link.replace("/?", "/stream?");
        let stream = client.get(&stream).send().await.expect("stream");
        assert_eq!(stream.status(), 503, "no desktop started");
        let base = link.split('?').next().expect("base");
        let refused = client.get(base).send().await.expect("refused");
        assert_eq!(refused.status(), 403);
    }
}
