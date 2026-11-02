//! `run_code`: runs Python or shell code in a bubblewrap (`bwrap`) sandbox on Linux. The sandbox
//! sees the system's programs read-only, writes only to Mokuzu's workspace, and has no network
//! and no view of the user's home or other processes, so it runs without approval, scheduled
//! jobs and webhooks included. Without `bwrap`, or where it cannot make a sandbox, the tool is
//! not offered.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::config::Config;
use crate::files;

/// Seconds a run gets when the call names none, and the most it may ask for.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 600;
/// Characters of stdout and of stderr each returned to the model; the rest is cut from the middle.
const MAX_OUTPUT_CHARS: usize = 10_000;
/// Address space a run may use, in KiB (`ulimit -v`); numpy and friends reserve a lot.
const MAX_MEMORY_KIB: u64 = 4 * 1024 * 1024;
/// Largest file a run may write, in KiB (`ulimit -f`).
const MAX_FILE_KIB: u64 = 512 * 1024;
/// Top-level system folders mounted read-only when present; on merged-/usr systems most are
/// symlinks into /usr and are recreated as such.
const SYSTEM_DIRS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/etc", "/opt",
];

static AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Checks once, at startup, that `bwrap` can make a sandbox here; later calls are free.
pub async fn probe() -> bool {
    if let Some(&available) = AVAILABLE.get() {
        return available;
    }
    let works = cfg!(target_os = "linux")
        && tokio::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--unshare-all",
                "--die-with-parent",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
    if !works && cfg!(target_os = "linux") {
        tracing::info!("run_code is off: bwrap is missing or cannot make a sandbox here");
    }
    *AVAILABLE.get_or_init(|| works)
}

/// Whether `run_code` is offered: `probe` found a working `bwrap`.
pub fn available() -> bool {
    AVAILABLE.get().copied().unwrap_or(false)
}

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "run_code".to_owned(),
        description: format!(
            "Run Python 3 or shell code in a sandbox: for calculations, data analysis, file \
             conversion, and charts. The working directory is your workspace, the only place it \
             can write; files it saves there can be sent with send_file. It has no network, so \
             fetch data with your other tools and write it to the workspace first. Only packages \
             installed on the system are importable; report a missing one instead of retrying. \
             matplotlib draws to files (Agg). Runs time out after `timeout` seconds (default \
             {DEFAULT_TIMEOUT_SECS}, at most {MAX_TIMEOUT_SECS}). Returns the exit code, stdout, \
             and stderr."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "language": {"type": "string", "enum": ["python", "shell"]},
                "code": {"type": "string", "description": "The program, given on its stdin."},
                "timeout": {"type": "integer", "description": "Seconds before the run is killed."},
            },
            "required": ["language", "code"],
        }),
    }
}

/// Runs one `run_code` call; failures come back as text for the model.
pub async fn run(config: &Config, args: &Value) -> String {
    match run_code(config, args).await {
        Ok(out) => out,
        Err(err) => format!("error: {err:#}"),
    }
}

async fn run_code(config: &Config, args: &Value) -> Result<String> {
    let interpreter = match args["language"].as_str() {
        Some("python") => "python3 -",
        Some("shell") => "sh -s",
        _ => anyhow::bail!("`language` must be python or shell"),
    };
    let code = args["code"].as_str().context("`code` is required")?;
    let timeout = args["timeout"]
        .as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    let workspace = files::workspace(config);
    tokio::fs::create_dir_all(&workspace)
        .await
        .with_context(|| format!("cannot create {}", workspace.display()))?;
    let workspace = tokio::fs::canonicalize(&workspace).await?;
    let mut child = tokio::process::Command::new("bwrap")
        .args(bwrap_args(&workspace, interpreter))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to start bwrap")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    let code = code.to_owned();
    // Fed from a task, so a program that reads no stdin can't stall on a full pipe.
    tokio::spawn(async move {
        let _ = stdin.write_all(code.as_bytes()).await;
    });
    // Dropping the wait on timeout (or on /stop) kills bwrap, and --die-with-parent the rest.
    let output =
        match tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
            Ok(output) => output.context("failed to run bwrap")?,
            Err(_) => return Ok(format!("error: timed out after {timeout}s and was killed")),
        };
    let code = output
        .status
        .code()
        .map_or_else(|| "killed by a signal".to_owned(), |c| c.to_string());
    Ok(format!(
        "exit code: {code}\nstdout:\n{}\nstderr:\n{}",
        cut(&String::from_utf8_lossy(&output.stdout)),
        cut(&String::from_utf8_lossy(&output.stderr)),
    ))
}

/// `bwrap` arguments for running `interpreter` with `workspace` as its only writable folder.
fn bwrap_args(workspace: &Path, interpreter: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    for dir in SYSTEM_DIRS {
        match std::fs::read_link(dir) {
            Ok(target) => args.extend([
                "--symlink".to_owned(),
                target.to_string_lossy().into_owned(),
                (*dir).to_owned(),
            ]),
            Err(_) => args.extend([
                "--ro-bind-try".to_owned(),
                (*dir).to_owned(),
                (*dir).to_owned(),
            ]),
        }
    }
    let workspace = workspace.to_string_lossy().into_owned();
    args.extend(
        [
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--hostname",
            "sandbox",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "MPLBACKEND",
            "Agg",
            "--setenv",
            "PYTHONDONTWRITEBYTECODE",
            "1",
            "--setenv",
            "MPLCONFIGDIR",
            "/tmp",
        ]
        .map(str::to_owned),
    );
    args.extend([
        "--bind".to_owned(),
        workspace.clone(),
        workspace.clone(),
        "--setenv".to_owned(),
        "HOME".to_owned(),
        workspace.clone(),
        "--chdir".to_owned(),
        workspace,
        "sh".to_owned(),
        "-c".to_owned(),
        format!("ulimit -v {MAX_MEMORY_KIB} -f {MAX_FILE_KIB}; exec {interpreter}"),
    ]);
    args
}

/// `text` within `MAX_OUTPUT_CHARS`, keeping its start and end.
fn cut(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return text.to_owned();
    }
    let half = MAX_OUTPUT_CHARS / 2;
    let head: String = text.chars().take(half).collect();
    let tail: String = text.chars().skip(total - half).collect();
    format!(
        "{head}\n[… {} characters cut …]\n{tail}",
        total - MAX_OUTPUT_CHARS
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_write_only_the_workspace_and_cut_the_network() {
        let args = bwrap_args(Path::new("/data/workspace"), "python3 -");
        let joined = args.join(" ");
        assert!(
            joined.contains("--bind /data/workspace /data/workspace"),
            "{joined}"
        );
        assert!(joined.contains("--unshare-all") && joined.contains("--clearenv"));
        assert_eq!(args.iter().filter(|a| *a == "--bind").count(), 1);
        assert!(args.last().is_some_and(|a| a.ends_with("exec python3 -")));
    }

    #[test]
    fn long_output_keeps_both_ends() {
        let text = format!("start{}end", "x".repeat(MAX_OUTPUT_CHARS * 2));
        let out = cut(&text);
        assert!(out.starts_with("start") && out.ends_with("end"));
        assert!(out.chars().count() < MAX_OUTPUT_CHARS + 100);
    }

    /// Needs Linux with bwrap: `cargo test -- --ignored sandbox`.
    #[tokio::test]
    #[ignore = "needs bwrap"]
    async fn sandbox_runs_code_in_the_workspace_only() {
        assert!(probe().await, "bwrap cannot make a sandbox here");
        let dir = std::env::temp_dir().join(format!("mokuzu-sandbox-{}", std::process::id()));
        let config = Config {
            database_path: dir.join("mokuzu.db"),
            ..Config::parse("[opencode-go]\napi_key = \"k\"\n").expect("valid")
        };
        let run_py = |code: &str| {
            let args = json!({"language": "python", "code": code});
            let config = config.clone();
            async move { run(&config, &args).await }
        };
        let out = run_py("open('out.txt', 'w').write('hi'); print(6 * 7)").await;
        assert!(out.contains("exit code: 0") && out.contains("42"), "{out}");
        assert!(files::workspace(&config).join("out.txt").exists());
        let home = std::env::home_dir().expect("home");
        let escape = format!(
            "open({:?}, 'w').write('x')",
            home.join("mokuzu-escape").display().to_string()
        );
        assert!(!run_py(&escape).await.contains("exit code: 0"));
        let net = "import socket; socket.create_connection(('1.1.1.1', 53), 3)";
        assert!(!run_py(net).await.contains("exit code: 0"));
        let shell = run(
            &config,
            &json!({"language": "shell", "code": "sleep 5", "timeout": 1}),
        )
        .await;
        assert!(shell.contains("timed out"), "{shell}");
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
