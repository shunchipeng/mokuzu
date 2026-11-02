//! The virtual desktop browsers draw on when no screen is plugged in, and the `/browser` live
//! view of it. Mokuzu starts Xvfb once and hands its `DISPLAY` to local MCP servers, so a browser
//! one opens headed (invisible-playwright-mcp with `STEALTHFOX_HEADLESS=0`) shows up there.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::OnceCell;

pub(crate) mod viewer;

/// The `/browser` command: a link to a live view of the desktop, to watch it.
pub const BROWSER_COMMAND: &str = "/browser";

/// `port` is the viewer's port, taken on first use; 0 picks a free one.
pub async fn view_link(port: u16) -> Result<String> {
    viewer::link(port).await
}

/// The Xvfb screen browsers draw on when no real one is plugged in; started once, then kept
/// until mokuzu exits.
static DESKTOP: OnceCell<Option<(Child, String)>> = OnceCell::const_new();

/// The virtual desktop's `DISPLAY`, starting it on a `size` screen on first use; `None` with a
/// real screen plugged in, or without Xvfb.
pub async fn desktop(size: (u32, u32)) -> Option<String> {
    DESKTOP
        .get_or_init(|| async {
            if screen_attached() {
                None
            } else {
                virtual_screen(size).await
            }
        })
        .await
        .as_ref()
        .map(|(_, display)| display.clone())
}

/// The virtual desktop's `DISPLAY` when it is up and ffmpeg can grab it.
async fn screen() -> Option<String> {
    static FFMPEG: OnceCell<bool> = OnceCell::const_new();
    let (_, display) = DESKTOP.get()?.as_ref()?;
    FFMPEG
        .get_or_init(|| installed("ffmpeg"))
        .await
        .then(|| display.clone())
}

/// Whether `program` is on the `PATH`.
async fn installed(program: &str) -> bool {
    tokio::process::Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok()
}

/// Whether browsers have a real screen to draw on: always off Linux; on Linux, a display server
/// with a monitor plugged in.
fn screen_attached() -> bool {
    !cfg!(target_os = "linux")
        || ((std::env::var_os("DISPLAY").is_some()
            || std::env::var_os("WAYLAND_DISPLAY").is_some())
            && monitor_connected())
}

/// Starts Xvfb on a free display and returns it with its `DISPLAY` value, or `None` when Xvfb
/// is not installed.
async fn virtual_screen((width, height): (u32, u32)) -> Option<(Child, String)> {
    let spawned = tokio::process::Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            tracing::debug!("no Xvfb ({err}); no virtual desktop");
            return None;
        }
    };
    // `-displayfd 1` prints the display number it picked once it is ready.
    let mut line = String::new();
    let mut stdout = BufReader::new(child.stdout.take()?);
    let read = tokio::time::timeout(Duration::from_secs(10), stdout.read_line(&mut line)).await;
    let number = line.trim();
    if !matches!(read, Ok(Ok(_))) || number.is_empty() {
        tracing::warn!("Xvfb did not start; no virtual desktop");
        return None;
    }
    tracing::debug!("virtual desktop on Xvfb display :{number}");
    Some((child, format!(":{number}")))
}

/// Whether a screen is plugged in. A desktop session runs on a headless Raspberry Pi too, and a
/// window there is never drawn, so pages never finish rendering.
fn monitor_connected() -> bool {
    std::fs::read_dir("/sys/class/drm").is_ok_and(|ports| {
        ports.flatten().any(|port| {
            std::fs::read_to_string(port.path().join("status"))
                .is_ok_and(|status| status.trim() == "connected")
        })
    })
}

/// A random number from std's randomly keyed hasher.
fn random() -> u64 {
    use std::hash::BuildHasher as _;
    std::collections::hash_map::RandomState::new().hash_one(0u8)
}
