mod agent;
mod browser;
mod compact;
mod config;
mod cron;
mod db;
mod discord;
mod document;
mod fetch;
mod files;
mod legacy;
mod mcp;
mod memory;
mod oauth;
mod onboarding;
mod review;
mod sandbox;
mod search;
mod service;
mod settings;
mod skill;
mod swarm;
mod todo;
mod voice;
mod webhook;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

const USAGE: &str =
    "usage: mokuzu [serve|configure|install|uninstall|update|restart|login PROVIDER] [--config PATH] [--version]

  serve      run the Discord bot (default)
  configure  interactive setup: Discord, model, voice, tools
  login      device OAuth login: github-copilot or chatgpt (OpenAI)
  install    run `mokuzu serve` in the background and keep it alive (launchd or systemd)
  uninstall  remove the background service
  update     install the latest release and restart the background service
  restart    restart the background service, e.g. after editing the config";

#[tokio::main]
async fn main() -> Result<()> {
    install_tls_provider();
    let mut command = None;
    let mut config_path = None;
    let mut login_provider = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                config_path = Some(PathBuf::from(args.next().context("--config needs a path")?))
            }
            Some("-h" | "--help") => {
                println!("{USAGE}");
                return Ok(());
            }
            Some("-V" | "--version") => {
                println!("mokuzu {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some(
                name @ ("serve" | "configure" | "install" | "uninstall" | "update" | "restart"
                | "login"),
            ) if command.is_none() => command = Some(name.to_owned()),
            Some(name @ ("github-copilot" | "chatgpt" | "openai"))
                if command.as_deref() == Some("login") && login_provider.is_none() =>
            {
                login_provider = Some(if name == "github-copilot" {
                    config::Provider::GithubCopilot
                } else {
                    config::Provider::Chatgpt
                });
            }
            _ => bail!("unexpected argument {arg:?}\n\n{USAGE}"),
        }
    }
    if command.as_deref() == Some("uninstall") {
        return service::uninstall();
    }
    if command.as_deref() == Some("update") {
        return service::update();
    }
    if command.as_deref() == Some("restart") {
        return service::restart();
    }
    let config_path = match config_path {
        Some(path) => std::path::absolute(path).context("failed to resolve --config path")?,
        None => std::env::home_dir()
            .context("cannot find home directory")?
            .join(".config/mokuzu/config.toml"),
    };
    let config_path = legacy::migrate(
        config_path,
        matches!(command.as_deref(), None | Some("serve")),
    )?;

    if command.as_deref() == Some("login") {
        let provider =
            login_provider.context("login needs a provider: github-copilot or chatgpt")?;
        return oauth::login(&config_path, provider).await;
    }

    if command.as_deref() == Some("configure") {
        onboarding::run(&config_path).await?;
        return Ok(());
    }
    // First run in a terminal: set up instead of failing on the missing file.
    let config = if !config_path.exists() && std::io::stdin().is_terminal() {
        onboarding::run(&config_path).await?
    } else {
        config::Config::load(&config_path)?
    };
    if command.as_deref() == Some("install") {
        if config.discord.is_none() {
            bail!("add a [discord] section to {} first", config_path.display());
        }
        return service::install(&config_path);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_new(&config.log_level).context("invalid log.level in config")?,
        )
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    discord::serve(config).await
}

/// Picks rustls's crypto backend for the whole process. reqwest brings aws-lc-rs and songbird's
/// own reqwest brings ring; with both compiled in, rustls cannot choose and panics on the first
/// TLS connection that relies on the default.
fn install_tls_provider() {
    // Fails only when a provider is already installed, which is just as good.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
