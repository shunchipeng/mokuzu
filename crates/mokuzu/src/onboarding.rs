//! `mokuzu configure`: a step-by-step, full-screen setup with dropdowns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListState, Padding, Paragraph, Wrap};

use crate::config::{self, Api, Config, Provider};
use crate::oauth;
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table};

/// Common OpenCode Go models; `config::Api::for_model` picks each one's endpoint format.
pub const MODELS: &[&str] = &[
    "minimax-m3",
    "qwen3.8-max",
    "qwen3.8-flash",
    "glm-5.3",
    "glm-5.3-flash",
    "kimi-k3",
    "kimi-k2.7-code",
    "deepseek-v4-pro",
    "deepseek-v4.1-flash",
    "mimo-v2.6-pro",
    "mimo-v2.6-flash",
    "longcat-2.0",
    "claude-haiku-5-5",
    "gpt-6-luna",
    "gpt-5.6-luna",
    "grok-4.7",
    "muse-spark-1.3-contributor",
    "gemini-4-argon-contributor",
];
/// Current Claude models on Anthropic's API.
const ANTHROPIC_MODELS: &[&str] = &[
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];
const OPENCODE_GO: &str = "OpenCode Go";
const ANTHROPIC: &str = "Anthropic";
const GITHUB_COPILOT: &str = "GitHub Copilot (OAuth)";
const CHATGPT: &str = "OpenAI ChatGPT (OAuth)";
const COPILOT_MODELS: &[&str] = &["gpt-5.3-codex", "claude-sonnet-4.6"];
const CHATGPT_MODELS: &[&str] = &["gpt-5.3-codex", "gpt-5.4"];
const CUSTOM: &str = "Custom…";
const ENABLED: &str = "Enabled";
const DISABLED: &str = "Disabled";
const SEARXNG: &str = "SearXNG";
const BRAVE: &str = "Brave Search API";
const STEPS: [&str; 6] = ["Discord", "Model", "Voice", "Web search", "MCP", "Advanced"];
const STEP_DISCORD: usize = 0;
const STEP_MODEL: usize = 1;
const STEP_VOICE: usize = 2;
const STEP_SEARCH: usize = 3;
const STEP_MCP: usize = 4;
const STEP_ADVANCED: usize = 5;
const NO: &str = "No";
const YES: &str = "Yes";
const EVERY_MESSAGE: &str = "Every message";
const MENTIONED: &str = "Only when @mentioned";
const STDIO: &str = "Local command (stdio)";
const HTTP: &str = "Remote URL (HTTP)";
const KEEP: &str = "Keep";
const REMOVE: &str = "Remove";
const ASK: &str = "Ask every call";
const ASK_ME: &str = "Ask me every time";
const AUTO: &str = "Auto (LLM review)";
const SAME_MODEL: &str = "Same as main model";
const NEVER_ASK: &str = "Never ask";
const API_AUTO: &str = "Auto (from model name)";

/// Shows the setup and saves the config to `path`.
pub async fn run(path: &Path) -> Result<Config> {
    let current = path.exists().then(|| Config::load(path).ok()).flatten();
    let mut form = Form::new(current.as_ref())?;
    let target = path.to_owned();
    let saved =
        tokio::task::spawn_blocking(move || ratatui::run(|terminal| form.run(terminal, &target)))
            .await
            .context("configure screen panicked")??;
    match saved {
        Some(config) => {
            println!("✓ saved {}", path.display());
            Ok(Config {
                path: path.to_owned(),
                ..config
            })
        }
        None => bail!("not saved"),
    }
}

enum Kind {
    Text,
    Secret,
    Select(Vec<String>),
}

struct Field {
    step: usize,
    label: String,
    kind: Kind,
    value: String,
}

// Field indices.
const PROVIDER: usize = 0;
const KEY: usize = 1;
const MODEL: usize = 2;
const CUSTOM_MODEL: usize = 3;
const TOKEN: usize = 4;
const USERS: usize = 5;
const SEARCH: usize = 6;
const SEARCH_URL: usize = 7;
const SEARCH_USER: usize = 8;
const SEARCH_PASSWORD: usize = 9;
const LOG: usize = 10;
const DATABASE: usize = 11;
const MCP_ADD: usize = 12;
const MCP_NAME: usize = 13;
const MCP_COMMAND: usize = 14;
const MCP_ARGS: usize = 15;
const MCP_ENV: usize = 16;
const MCP_URL: usize = 17;
const MCP_TOKEN: usize = 18;
const MCP_APPROVE: usize = 19;
const APPROVAL: usize = 20;
const REVIEWER: usize = 21;
const TIMEZONE: usize = 22;
const BRAVE_KEY: usize = 23;
const GUILD_ADD: usize = 24;
const GUILD_ID: usize = 25;
const GUILD_USERS: usize = 26;
const GUILD_REPLY: usize = 27;
const SEARCH_RESULTS: usize = 28;
const API: usize = 29;
const BASE_URL: usize = 30;
const MAX_TOKENS: usize = 31;
const COMPACT_AT: usize = 32;
const BROWSER_WIDTH: usize = 33;
const BROWSER_HEIGHT: usize = 34;
const VIEWER_PORT: usize = 35;
const VOICE: usize = 36;
const VOICE_KEY: usize = 37;
const VOICE_LANGUAGE: usize = 38;
const VOICE_CHANNELS: usize = 39;
const VOICE_NAME: usize = 40;
const VOICE_MODEL: usize = 41;
const VOICE_URL: usize = 42;
const WEBHOOK_PORT: usize = 43;
/// First of the rows for already configured MCP servers, followed by the configured Discord
/// servers' rows.
const MCP_FIRST: usize = 44;

/// Focus value for the Save button under the fields.
const BUTTON: usize = usize::MAX;
const LOGIN: usize = usize::MAX - 1;

enum LoginUpdate {
    Prompt { url: String, code: String },
    Done(std::result::Result<(), String>),
}

struct Login {
    provider: Provider,
    updates: mpsc::Receiver<LoginUpdate>,
    task: tokio::task::JoinHandle<()>,
    prompt: Option<(String, String)>,
}

impl Drop for Login {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Form {
    fields: Vec<Field>,
    step: usize,
    focus: usize,
    /// Open dropdown for the focused select field.
    menu: Option<ListState>,
    reveal: bool,
    status: Line<'static>,
    /// Configured MCP servers, one Keep / Remove row each from `MCP_FIRST` on.
    mcp_names: Vec<String>,
    /// Configured Discord servers, one Keep / Remove row each after the MCP rows.
    guilds: Vec<(u64, config::GuildRule)>,
    /// API key and base URL of each provider not picked now, so switching providers never
    /// carries one provider's key over to another, and saving keeps both.
    keys: BTreeMap<Provider, (String, String)>,
    /// Set once a save was tried, so empty required fields and unfinished steps show as errors.
    attempted: bool,
    /// `NO_COLOR` is set: draw without colors, relying on the ✓ ✗ ● ▌ marks.
    plain: bool,
    login: Option<Login>,
}

fn models(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::OpencodeGo => MODELS,
        Provider::Anthropic => ANTHROPIC_MODELS,
        Provider::GithubCopilot => COPILOT_MODELS,
        Provider::Chatgpt => CHATGPT_MODELS,
    }
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::OpencodeGo => OPENCODE_GO,
        Provider::Anthropic => ANTHROPIC,
        Provider::GithubCopilot => GITHUB_COPILOT,
        Provider::Chatgpt => CHATGPT,
    }
}

fn options(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// `values` plus `current` when it isn't one of them, so a hand-edited value survives.
fn options_with(values: &[&str], current: &str) -> Vec<String> {
    let mut all = options(values);
    if !all.iter().any(|v| v == current) {
        all.push(current.to_owned());
    }
    all
}

impl Form {
    fn new(current: Option<&Config>) -> Result<Self> {
        let discord = current.and_then(|c| c.discord.as_ref());
        let search = current.and_then(|c| c.search.as_ref());
        let (searxng, brave) = match search {
            Some(config::Search::Searxng(searxng)) => (Some(searxng), None),
            Some(config::Search::Brave(brave)) => (None, Some(brave)),
            None => (None, None),
        };
        let search_auth = searxng.and_then(|s| s.auth.as_ref());
        let provider = current.map_or(Provider::OpencodeGo, |c| c.provider);
        let model = current.map_or(models(provider)[0], |c| c.model.as_str());
        let preset = models(provider).contains(&model);
        let log = current.map_or("warn", |c| c.log_level.as_str());
        let database = match current {
            Some(c) => c.database_path.clone(),
            None => default_database_path()?,
        };
        let field = |step, label: &str, kind, value: String| Field {
            step,
            label: label.to_owned(),
            kind,
            value,
        };

        let mut model_options = options(models(provider));
        model_options.push(CUSTOM.to_owned());
        let mut fields = vec![
            field(
                STEP_MODEL,
                "Provider",
                Kind::Select(options(&[OPENCODE_GO, ANTHROPIC, GITHUB_COPILOT, CHATGPT])),
                provider_name(provider).to_owned(),
            ),
            field(
                STEP_MODEL,
                "API key",
                Kind::Secret,
                current
                    .map(|c| c.api_key.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                STEP_MODEL,
                "Model",
                Kind::Select(model_options),
                if preset { model } else { CUSTOM }.to_owned(),
            ),
            field(
                STEP_MODEL,
                "Model ID",
                Kind::Text,
                if preset {
                    String::new()
                } else {
                    model.to_owned()
                },
            ),
            field(
                STEP_DISCORD,
                "Bot token",
                Kind::Secret,
                discord
                    .map(|d| d.token.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                STEP_DISCORD,
                "Allowed user IDs",
                Kind::Text,
                discord
                    .map(|d| join_ids(&d.allowed_users))
                    .unwrap_or_default(),
            ),
            field(
                STEP_SEARCH,
                "Web search",
                Kind::Select(options(&[DISABLED, SEARXNG, BRAVE])),
                match search {
                    Some(config::Search::Searxng(_)) => SEARXNG,
                    Some(config::Search::Brave(_)) => BRAVE,
                    None => DISABLED,
                }
                .to_owned(),
            ),
            field(
                STEP_SEARCH,
                "SearXNG URL",
                Kind::Text,
                searxng.map(|s| s.url.clone()).unwrap_or_default(),
            ),
            field(
                STEP_SEARCH,
                "Username",
                Kind::Text,
                search_auth
                    .map(|(user, _)| user.clone())
                    .unwrap_or_default(),
            ),
            field(
                STEP_SEARCH,
                "Password",
                Kind::Secret,
                search_auth
                    .map(|(_, password)| password.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            field(
                STEP_ADVANCED,
                "Log level",
                Kind::Select(options_with(&["error", "warn", "info", "debug"], log)),
                log.to_owned(),
            ),
            field(
                STEP_ADVANCED,
                "Database",
                Kind::Text,
                database.to_string_lossy().into_owned(),
            ),
            field(
                STEP_MCP,
                "Add server",
                Kind::Select(options(&[NO, STDIO, HTTP])),
                NO.to_owned(),
            ),
            field(STEP_MCP, "Name", Kind::Text, String::new()),
            field(STEP_MCP, "Command", Kind::Text, String::new()),
            field(STEP_MCP, "Arguments", Kind::Text, String::new()),
            field(STEP_MCP, "Env (K=V, …)", Kind::Secret, String::new()),
            field(STEP_MCP, "URL", Kind::Text, String::new()),
            field(STEP_MCP, "Token", Kind::Secret, String::new()),
            field(
                STEP_MCP,
                "Approval",
                Kind::Select(options(&[ASK, NEVER_ASK])),
                ASK.to_owned(),
            ),
        ];
        let auto = current.is_some_and(|c| matches!(c.approval, config::Approval::Auto { .. }));
        fields.push(field(
            STEP_ADVANCED,
            "Approval",
            Kind::Select(options(&[ASK_ME, AUTO])),
            if auto { AUTO } else { ASK_ME }.to_owned(),
        ));
        let reviewer = match current.map(|c| (&c.approval, &c.model)) {
            Some((config::Approval::Auto { model }, main)) if model != main => model.as_str(),
            _ => SAME_MODEL,
        };
        let mut reviewers = vec![SAME_MODEL];
        reviewers.extend(models(provider));
        fields.push(field(
            STEP_ADVANCED,
            "Reviewer model",
            Kind::Select(options_with(&reviewers, reviewer)),
            reviewer.to_owned(),
        ));
        let zone = current.map_or_else(config::system_timezone, |c| c.timezone);
        fields.push(field(
            STEP_ADVANCED,
            "Time zone",
            Kind::Text,
            zone.name().to_owned(),
        ));
        fields.push(field(
            STEP_SEARCH,
            "Brave API key",
            Kind::Secret,
            brave
                .map(|b| b.api_key.as_str().to_owned())
                .unwrap_or_default(),
        ));
        fields.push(field(
            STEP_DISCORD,
            "Add server",
            Kind::Select(options(&[NO, YES])),
            NO.to_owned(),
        ));
        fields.push(field(STEP_DISCORD, "Server ID", Kind::Text, String::new()));
        fields.push(field(
            STEP_DISCORD,
            "Server user IDs",
            Kind::Text,
            String::new(),
        ));
        fields.push(field(
            STEP_DISCORD,
            "Reply to",
            Kind::Select(options(&[EVERY_MESSAGE, MENTIONED])),
            EVERY_MESSAGE.to_owned(),
        ));
        fields.push(field(
            STEP_SEARCH,
            "Results",
            Kind::Text,
            search.map_or(5, config::Search::results).to_string(),
        ));
        let api = match current {
            Some(c) if c.api != c.provider.api_for(&c.model) => c.api.name(),
            _ => API_AUTO,
        };
        let mut apis = vec![API_AUTO];
        apis.extend(Api::ALL.map(Api::name));
        fields.push(field(
            STEP_ADVANCED,
            "API format",
            Kind::Select(options(&apis)),
            api.to_owned(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Base URL",
            Kind::Text,
            current
                .map_or(config::DEFAULT_BASE_URL, |c| c.base_url.as_str())
                .to_owned(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Max output tokens",
            Kind::Text,
            current
                .map_or(config::DEFAULT_MAX_TOKENS, |c| c.max_tokens)
                .to_string(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Compact at tokens",
            Kind::Text,
            current
                .map_or(config::DEFAULT_COMPACT_AT_TOKENS, |c| c.compact_at_tokens)
                .to_string(),
        ));
        let screen = current.map_or(config::DEFAULT_SCREEN, |c| c.browser_screen);
        fields.push(field(
            STEP_ADVANCED,
            "Browser width",
            Kind::Text,
            screen.0.to_string(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Browser height",
            Kind::Text,
            screen.1.to_string(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Browser viewer port",
            Kind::Text,
            current
                .map_or(config::DEFAULT_VIEWER_PORT, |c| c.viewer_port)
                .to_string(),
        ));
        let voice = current.and_then(|c| c.voice.as_ref());
        let voice_name = voice.map_or(config::REALTIME_VOICES[0], |v| v.voice.as_str());
        fields.push(field(
            STEP_VOICE,
            "Voice calls",
            Kind::Select(options(&[DISABLED, ENABLED])),
            if voice.is_some() { ENABLED } else { DISABLED }.to_owned(),
        ));
        fields.push(field(
            STEP_VOICE,
            "OpenAI API key",
            Kind::Secret,
            voice
                .map(|v| v.api_key.as_str().to_owned())
                .unwrap_or_default(),
        ));
        fields.push(field(
            STEP_VOICE,
            "Language",
            Kind::Text,
            voice
                .map_or(config::DEFAULT_VOICE_LANGUAGE, |v| v.language.as_str())
                .to_owned(),
        ));
        fields.push(field(
            STEP_VOICE,
            "Auto-join channels",
            Kind::Text,
            voice.map(|v| join_ids(&v.channels)).unwrap_or_default(),
        ));
        fields.push(field(
            STEP_VOICE,
            "Voice",
            Kind::Select(options_with(&config::REALTIME_VOICES, voice_name)),
            voice_name.to_owned(),
        ));
        fields.push(field(
            STEP_VOICE,
            "Realtime model",
            Kind::Text,
            voice
                .map_or(config::DEFAULT_REALTIME_MODEL, |v| v.model.as_str())
                .to_owned(),
        ));
        fields.push(field(
            STEP_VOICE,
            "Realtime URL",
            Kind::Text,
            voice
                .map_or(config::DEFAULT_REALTIME_URL, |v| v.url.as_str())
                .to_owned(),
        ));
        fields.push(field(
            STEP_ADVANCED,
            "Webhook port",
            Kind::Text,
            current
                .and_then(|c| c.webhook_port)
                .map(|port| port.to_string())
                .unwrap_or_default(),
        ));
        let mcp_names: Vec<String> = current
            .map(|c| c.mcp.keys().cloned().collect())
            .unwrap_or_default();
        fields.extend(mcp_names.iter().map(|name| {
            field(
                STEP_MCP,
                name,
                Kind::Select(options(&[KEEP, REMOVE])),
                KEEP.to_owned(),
            )
        }));
        let guilds: Vec<(u64, config::GuildRule)> = discord
            .map(|d| d.guilds.clone().into_iter().collect())
            .unwrap_or_default();
        fields.extend(guilds.iter().map(|(id, rule)| {
            let reply = if rule.mention_only {
                MENTIONED
            } else {
                EVERY_MESSAGE
            };
            let keep = format!(
                "{KEEP} · {} · {}",
                match &rule.allowed_users {
                    config::Users::Everyone => "everyone".to_owned(),
                    config::Users::Only(ids) => join_ids(ids),
                },
                reply.to_lowercase()
            );
            field(
                STEP_DISCORD,
                &id.to_string(),
                Kind::Select(vec![keep.clone(), REMOVE.to_owned()]),
                keep,
            )
        }));
        Ok(Self {
            fields,
            step: STEP_DISCORD,
            focus: TOKEN,
            menu: None,
            reveal: false,
            status: Line::default(),
            mcp_names,
            guilds,
            keys: current
                .map(|c| {
                    c.keys
                        .iter()
                        .filter(|(p, _)| *p != provider)
                        .map(|(p, key)| {
                            (*p, (key.api_key.as_str().to_owned(), key.base_url.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            attempted: false,
            plain: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            login: None,
        })
    }

    /// Index of the first configured Discord server's row.
    fn guild_first(&self) -> usize {
        MCP_FIRST + self.mcp_names.len()
    }

    fn shown(&self, i: usize) -> bool {
        let field = &self.fields[i];
        field.step == self.step
            && match i {
                CUSTOM_MODEL => self.fields[MODEL].value == CUSTOM,
                VOICE_KEY | VOICE_MODEL | VOICE_NAME | VOICE_URL | VOICE_CHANNELS
                | VOICE_LANGUAGE => self.fields[VOICE].value == ENABLED,
                GUILD_ID | GUILD_USERS | GUILD_REPLY => self.fields[GUILD_ADD].value == YES,
                SEARCH_URL | SEARCH_USER | SEARCH_PASSWORD => self.fields[SEARCH].value == SEARXNG,
                SEARCH_RESULTS => self.fields[SEARCH].value != DISABLED,
                BRAVE_KEY => self.fields[SEARCH].value == BRAVE,
                MCP_NAME | MCP_APPROVE => self.fields[MCP_ADD].value != NO,
                MCP_COMMAND | MCP_ARGS | MCP_ENV => self.fields[MCP_ADD].value == STDIO,
                MCP_URL | MCP_TOKEN => self.fields[MCP_ADD].value == HTTP,
                REVIEWER => self.fields[APPROVAL].value == AUTO,
                API => self.provider() == Provider::OpencodeGo,
                KEY | BASE_URL => !self.provider().uses_oauth(),
                _ => true,
            }
    }

    /// Focusable rows of the current step: its visible fields, then the button.
    fn visible(&self) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|&i| self.shown(i))
            .chain((self.step == STEP_MODEL && self.provider().uses_oauth()).then_some(LOGIN))
            .chain([BUTTON])
            .collect()
    }

    fn move_focus(&mut self, step: isize) {
        let rows = self.visible();
        let at = rows.iter().position(|&i| i == self.focus).unwrap_or(0);
        let next = (at as isize + step).rem_euclid(rows.len() as isize);
        self.focus = rows[next as usize];
    }

    fn go_to_step(&mut self, step: usize) {
        self.step = step;
        self.focus = self.visible()[0];
        self.status = Line::default();
    }

    fn open_menu(&mut self) {
        if let Some(Kind::Select(values)) = self.fields.get(self.focus).map(|f| &f.kind) {
            let selected = values
                .iter()
                .position(|v| *v == self.fields[self.focus].value);
            self.menu = Some(ListState::default().with_selected(selected.or(Some(0))));
        }
    }

    /// Runs the event loop; `Some` once saved, `None` if the user quit.
    fn run(&mut self, terminal: &mut DefaultTerminal, path: &Path) -> Result<Option<Config>> {
        loop {
            self.poll_login();
            terminal.draw(|frame| self.draw(frame))?;
            if self.login.is_some() && !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(None);
            }
            if self.login.is_some() {
                if key.code == KeyCode::Esc {
                    self.cancel_login();
                }
                continue;
            }
            if let Some(menu) = &mut self.menu {
                let Kind::Select(values) = &self.fields[self.focus].kind else {
                    self.menu = None;
                    continue;
                };
                match key.code {
                    KeyCode::Up => menu.select_previous(),
                    KeyCode::Down => menu.select_next(),
                    KeyCode::Enter => {
                        let chosen = menu.selected().unwrap_or(0).min(values.len() - 1);
                        let before = self.provider();
                        self.fields[self.focus].value = values[chosen].clone();
                        self.menu = None;
                        if self.provider() != before {
                            self.switch_provider(before);
                        }
                        self.edited();
                    }
                    KeyCode::Esc => self.menu = None,
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Esc if self.step == 0 => return Ok(None),
                KeyCode::Esc => self.go_to_step(self.step - 1),
                // Steps are checked together on save, so any step can be visited in any order.
                KeyCode::Left if self.step > 0 => self.go_to_step(self.step - 1),
                KeyCode::Right if self.step + 1 < STEPS.len() => self.go_to_step(self.step + 1),
                KeyCode::Up | KeyCode::BackTab => self.move_focus(-1),
                KeyCode::Down | KeyCode::Tab => self.move_focus(1),
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.reveal = !self.reveal;
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(config) = self.save(path)? {
                        return Ok(Some(config));
                    }
                }
                KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if self.step == STEP_MODEL && self.provider().uses_oauth() {
                        self.start_login(path);
                    }
                }
                KeyCode::Enter if self.focus == LOGIN => self.start_login(path),
                KeyCode::Enter if self.focus == BUTTON => {
                    if let Some(config) = self.save(path)? {
                        return Ok(Some(config));
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ')
                    if self
                        .fields
                        .get(self.focus)
                        .is_some_and(|field| matches!(field.kind, Kind::Select(_))) =>
                {
                    self.open_menu();
                }
                KeyCode::Enter => self.move_focus(1),
                KeyCode::Backspace if self.focus < self.fields.len() => {
                    self.fields[self.focus].value.pop();
                    self.edited();
                }
                KeyCode::Char(c)
                    if self.focus < self.fields.len()
                        && !matches!(self.fields[self.focus].kind, Kind::Select(_)) =>
                {
                    self.fields[self.focus].value.push(c);
                    self.edited();
                }
                _ => {}
            }
        }
    }

    fn start_login(&mut self, path: &Path) {
        let provider = self.provider();
        let path = path.to_owned();
        let (sender, updates) = mpsc::channel();
        let task = tokio::runtime::Handle::current().spawn(async move {
            let prompts = sender.clone();
            let result = oauth::login_with_prompt(&path, provider, move |url, code| {
                let _ = prompts.send(LoginUpdate::Prompt { url, code });
            })
            .await
            .map_err(|err| format!("{err:#}"));
            let _ = sender.send(LoginUpdate::Done(result));
        });
        self.login = Some(Login {
            provider,
            updates,
            task,
            prompt: None,
        });
    }

    fn cancel_login(&mut self) {
        self.login = None;
        self.status = Line::from("Sign-in cancelled; settings kept.").yellow();
    }

    fn poll_login(&mut self) {
        let mut done = None;
        if let Some(login) = &mut self.login {
            loop {
                match login.updates.try_recv() {
                    Ok(LoginUpdate::Prompt { url, code }) => login.prompt = Some((url, code)),
                    Ok(LoginUpdate::Done(result)) => {
                        done = Some((login.provider, result));
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        done = Some((
                            login.provider,
                            Err("sign-in task ended unexpectedly".to_owned()),
                        ));
                        break;
                    }
                }
            }
        }
        if let Some((provider, result)) = done {
            self.login = None;
            self.status = match result {
                Ok(()) => Line::from(format!(
                    "✓ Signed in to {}. Save to keep your settings.",
                    provider_name(provider)
                ))
                .green(),
                Err(err) => Line::from(format!("✗ {err}")).red(),
            };
        }
    }

    /// Any edit clears a previous save error.
    fn provider(&self) -> Provider {
        match self.fields[PROVIDER].value.as_str() {
            ANTHROPIC => Provider::Anthropic,
            GITHUB_COPILOT => Provider::GithubCopilot,
            CHATGPT => Provider::Chatgpt,
            _ => Provider::OpencodeGo,
        }
    }

    /// Points the model and reviewer menus at the newly picked provider, and swaps in its own
    /// API key and base URL, keeping `from`'s for when it is picked again.
    fn switch_provider(&mut self, from: Provider) {
        let to = self.provider();
        let left = (self.value(KEY), self.value(BASE_URL));
        self.keys.insert(from, left);
        let (key, url) = self
            .keys
            .remove(&to)
            .unwrap_or_else(|| (String::new(), to.default_base_url().to_owned()));
        self.fields[KEY].value = key;
        self.fields[BASE_URL].value = url;
        let mut model_options = options(models(to));
        model_options.push(CUSTOM.to_owned());
        self.fields[MODEL].value = model_options[0].clone();
        self.fields[MODEL].kind = Kind::Select(model_options);
        let mut reviewers = vec![SAME_MODEL];
        reviewers.extend(models(to));
        self.fields[REVIEWER].value = SAME_MODEL.to_owned();
        self.fields[REVIEWER].kind = Kind::Select(options(&reviewers));
    }

    fn edited(&mut self) {
        self.status = Line::default();
    }

    fn save(&mut self, path: &Path) -> Result<Option<Config>> {
        self.attempted = true;
        // Steps can be skipped with ←→, so check them all and show the first one with a problem.
        if let Some((step, err)) =
            (0..STEPS.len()).find_map(|step| self.check_step(step).err().map(|err| (step, err)))
        {
            self.go_to_step(step);
            self.status = Line::from(format!("✗ {err}")).red();
            return Ok(None);
        }
        let answers = match self.answers() {
            Ok(answers) => answers,
            Err(err) => {
                self.status = Line::from(format!("✗ {err}")).red();
                return Ok(None);
            }
        };
        let (remove, add) = match self.mcp() {
            Ok(changes) => changes,
            Err(err) => {
                self.status = Line::from(format!("✗ {err}")).red();
                return Ok(None);
            }
        };
        let old = std::fs::read_to_string(path).ok();
        let text = merge_mcp(&render_toml(&answers), old.as_deref(), &remove, add);
        let config = Config::parse(&text).context("form produced an invalid config")?;

        config::write(path, &text)?;
        Ok(Some(config))
    }

    fn value(&self, i: usize) -> String {
        self.fields[i].value.trim().to_owned()
    }

    fn required(&self, i: usize) -> std::result::Result<String, String> {
        let value = self.value(i);
        if value.is_empty() {
            return Err(format!("{} is required", self.fields[i].label));
        }
        Ok(value)
    }

    /// Validates the fields of `step` only.
    fn check_step(&self, step: usize) -> std::result::Result<(), String> {
        match step {
            STEP_DISCORD => {
                self.discord()?;
            }
            STEP_MODEL => {
                if !self.provider().uses_oauth() {
                    self.required(KEY)?;
                }
                self.model()?;
            }
            STEP_VOICE => {
                self.voice()?;
            }
            STEP_SEARCH => {
                self.search()?;
            }
            STEP_MCP => {
                self.mcp()?;
            }
            _ => {
                self.required(DATABASE)?;
                self.timezone()?;
                self.model_options()?;
                self.browser_options()?;
                self.number::<u16>(WEBHOOK_PORT, 1)?;
            }
        }
        Ok(())
    }

    fn model(&self) -> std::result::Result<String, String> {
        if self.fields[MODEL].value == CUSTOM {
            self.required(CUSTOM_MODEL)
        } else {
            self.required(MODEL)
        }
    }

    /// The `[model]` and provider settings past name and key. A field left empty is written as
    /// its default, so the file holds every value mokuzu runs with.
    fn model_options(&self) -> std::result::Result<ModelOptions, String> {
        let provider = self.provider();
        let api = match self.value(API) {
            // Anthropic speaks only its own format.
            api if api == API_AUTO || provider != Provider::OpencodeGo => {
                provider.api_for(&self.model()?).name().to_owned()
            }
            api => api,
        };
        let base_url = self.value(BASE_URL);
        Ok(ModelOptions {
            api,
            base_url: if base_url.is_empty() {
                provider.default_base_url().to_owned()
            } else {
                base_url
            },
            max_tokens: self
                .number(MAX_TOKENS, 1)?
                .unwrap_or(config::DEFAULT_MAX_TOKENS),
            compact_at_tokens: self
                .number(COMPACT_AT, config::MIN_COMPACT_AT_TOKENS)?
                .unwrap_or(config::DEFAULT_COMPACT_AT_TOKENS),
        })
    }

    /// The `[browser]` settings; a field left empty is written as its default.
    fn browser_options(&self) -> std::result::Result<BrowserOptions, String> {
        Ok(BrowserOptions {
            width: self
                .number(BROWSER_WIDTH, config::MIN_SCREEN.0)?
                .unwrap_or(config::DEFAULT_SCREEN.0),
            height: self
                .number(BROWSER_HEIGHT, config::MIN_SCREEN.1)?
                .unwrap_or(config::DEFAULT_SCREEN.1),
            viewer_port: self
                .number(VIEWER_PORT, 1)?
                .unwrap_or(config::DEFAULT_VIEWER_PORT),
        })
    }

    /// Field `i` as a number of at least `min`; `None` when empty.
    fn number<T>(&self, i: usize, min: T) -> std::result::Result<Option<T>, String>
    where
        T: std::str::FromStr + PartialOrd + std::fmt::Display,
    {
        let value = self.value(i);
        if value.is_empty() {
            return Ok(None);
        }
        let label = &self.fields[i].label;
        match value.parse::<T>() {
            Ok(n) if n >= min => Ok(Some(n)),
            _ => Err(format!("{label} must be a whole number of at least {min}")),
        }
    }

    /// Search results per query, 1 to 20; 5 when empty.
    fn search_results(&self) -> std::result::Result<usize, String> {
        match self.number(SEARCH_RESULTS, 1)? {
            Some(n) if n > 20 => Err("Results must be between 1 and 20".to_owned()),
            n => Ok(n.unwrap_or(5)),
        }
    }

    /// This machine's time zone when empty.
    fn timezone(&self) -> std::result::Result<String, String> {
        let zone = self.value(TIMEZONE);
        if zone.is_empty() {
            return Ok(config::system_timezone().name().to_owned());
        }
        zone.parse::<chrono_tz::Tz>()
            .map_err(|_| format!("Time zone `{zone}` is not an IANA name like Asia/Taipei"))?;
        Ok(zone)
    }

    /// The `[voice]` section, when calls are on.
    fn voice(&self) -> std::result::Result<Option<VoiceAnswers>, String> {
        if self.fields[VOICE].value != ENABLED {
            return Ok(None);
        }
        let url = self.required(VOICE_URL)?;
        if !url.starts_with("wss://") {
            return Err("Realtime URL must start with wss://".to_owned());
        }
        Ok(Some(VoiceAnswers {
            api_key: self.required(VOICE_KEY)?,
            url,
            model: self.required(VOICE_MODEL)?,
            voice: self.required(VOICE_NAME)?,
            language: self.required(VOICE_LANGUAGE)?,
            channels: parse_ids(&self.value(VOICE_CHANNELS), "channel")?,
        }))
    }

    fn discord(&self) -> std::result::Result<DiscordAnswers, String> {
        Ok(DiscordAnswers {
            token: self.required(TOKEN)?,
            allowed_users: parse_user_ids(&self.value(USERS))?,
            guilds: self.guild_answers()?,
        })
    }

    /// Configured servers not set to Remove, plus the one being added.
    fn guild_answers(&self) -> std::result::Result<Vec<GuildAnswers>, String> {
        let first = self.guild_first();
        let mut guilds: Vec<GuildAnswers> = self
            .guilds
            .iter()
            .enumerate()
            .filter(|(i, _)| self.fields[first + i].value != REMOVE)
            .map(|(_, (id, rule))| GuildAnswers {
                id: *id,
                mention_only: rule.mention_only,
                allowed_users: rule.allowed_users.clone(),
            })
            .collect();
        if self.fields[GUILD_ADD].value != YES {
            return Ok(guilds);
        }
        let id = parse_server_id(&self.required(GUILD_ID)?)?;
        if guilds.iter().any(|g| g.id == id) {
            return Err(format!(
                "server {id} exists; set it to Remove to replace it"
            ));
        }
        guilds.push(GuildAnswers {
            id,
            mention_only: self.fields[GUILD_REPLY].value == MENTIONED,
            allowed_users: match self.value(GUILD_USERS).as_str() {
                "*" => config::Users::Everyone,
                ids => config::Users::Only(parse_user_ids(ids)?),
            },
        });
        Ok(guilds)
    }

    fn search(&self) -> std::result::Result<Option<SearchAnswers>, String> {
        match self.fields[SEARCH].value.as_str() {
            SEARXNG => {}
            BRAVE => {
                return Ok(Some(SearchAnswers::Brave {
                    api_key: self.required(BRAVE_KEY)?,
                    results: self.search_results()?,
                }));
            }
            _ => return Ok(None),
        }
        let url = self.required(SEARCH_URL)?;
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("SearXNG URL must start with http:// or https://".to_owned());
        }
        let auth = match (self.value(SEARCH_USER), self.value(SEARCH_PASSWORD)) {
            (user, password) if user.is_empty() && password.is_empty() => None,
            (user, password) if !user.is_empty() && !password.is_empty() => Some((user, password)),
            _ => return Err("fill in both Username and Password, or leave both empty".to_owned()),
        };
        Ok(Some(SearchAnswers::Searxng {
            url,
            auth,
            results: self.search_results()?,
        }))
    }

    /// MCP servers to remove, and the one to add as `(name, table)`.
    fn mcp(&self) -> std::result::Result<McpChanges, String> {
        let remove: Vec<String> = self
            .mcp_names
            .iter()
            .enumerate()
            .filter(|(i, _)| self.fields[MCP_FIRST + i].value == REMOVE)
            .map(|(_, name)| name.clone())
            .collect();
        let kind = self.fields[MCP_ADD].value.as_str();
        if kind == NO {
            return Ok((remove, None));
        }
        let name = self.required(MCP_NAME)?;
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err("server Name may use only letters, digits, _ and -".to_owned());
        }
        if self.mcp_names.contains(&name) && !remove.contains(&name) {
            return Err(format!("`{name}` exists; set it to Remove to replace it"));
        }
        let mut table = Table::new();
        if kind == STDIO {
            table["command"] = toml_edit::value(self.required(MCP_COMMAND)?);
            let args: Array = self.value(MCP_ARGS).split_whitespace().collect();
            if !args.is_empty() {
                table["args"] = toml_edit::value(args);
            }
            let mut env = InlineTable::new();
            for pair in self.value(MCP_ENV).split(',').map(str::trim) {
                if pair.is_empty() {
                    continue;
                }
                let Some((key, value)) = pair.split_once('=') else {
                    return Err("Env entries need KEY=VALUE, separated by commas".to_owned());
                };
                env.insert(key.trim(), value.trim().into());
            }
            if !env.is_empty() {
                table["env"] = toml_edit::value(env);
            }
        } else {
            let url = self.required(MCP_URL)?;
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err("URL must start with http:// or https://".to_owned());
            }
            table["url"] = toml_edit::value(url);
            let token = self.value(MCP_TOKEN);
            if !token.is_empty() {
                table["token"] = toml_edit::value(token);
            }
        }
        table["approve"] = toml_edit::value(self.fields[MCP_APPROVE].value != NEVER_ASK);
        Ok((remove, Some((name, table))))
    }

    fn answers(&self) -> std::result::Result<Answers, String> {
        Ok(Answers {
            provider: self.provider(),
            api_key: if self.provider().uses_oauth() {
                String::new()
            } else {
                self.required(KEY)?
            },
            model: self.model()?,
            discord: self.discord()?,
            voice: self.voice()?,
            search: self.search()?,
            log_level: self.required(LOG)?,
            auto_approval: if self.fields[APPROVAL].value == AUTO {
                Some(match self.value(REVIEWER) {
                    reviewer if reviewer == SAME_MODEL => self.model()?,
                    reviewer => reviewer,
                })
            } else {
                None
            },
            database: PathBuf::from(self.required(DATABASE)?),
            timezone: self.timezone()?,
            options: self.model_options()?,
            other_keys: self
                .keys
                .iter()
                .filter(|(provider, (key, _))| provider.uses_oauth() || !key.trim().is_empty())
                .map(|(&provider, (key, url))| {
                    let url = if url.is_empty() {
                        provider.default_base_url().to_owned()
                    } else {
                        url.clone()
                    };
                    (provider, key.trim().to_owned(), url)
                })
                .collect(),
            browser: self.browser_options()?,
            webhook_port: self.number(WEBHOOK_PORT, 1)?,
        })
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let rows = self.visible();
        // The Save row takes two lines: a gap, then the button.
        let fields_height = (rows.len() as u16 + 3).max(STEPS.len() as u16 + 2);
        let [header, main, help_area, status, keys] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(fields_height),
            Constraint::Length(5),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let [sidebar, body] =
            Layout::horizontal([Constraint::Length(18), Constraint::Fill(1)]).areas(main);

        frame.render_widget(
            Line::from(vec![
                " mokuzu ".black().on_cyan().bold(),
                "configure".bold(),
            ]),
            header,
        );
        frame.render_widget(
            Line::from(concat!("v", env!("CARGO_PKG_VERSION"), " "))
                .dark_gray()
                .right_aligned(),
            header,
        );

        let steps: Vec<Line> = STEPS
            .iter()
            .enumerate()
            .map(|(step, name)| {
                let (mark, style) = if step == self.step {
                    (
                        "●",
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )
                } else if self.check_step(step).is_ok() {
                    ("✓", Style::new().fg(Color::Green))
                } else if self.attempted {
                    ("✗", Style::new().fg(Color::Red))
                } else {
                    ("○", Style::new().fg(Color::DarkGray))
                };
                let bar = if step == self.step { "▌" } else { " " };
                Line::from(vec![
                    bar.cyan(),
                    Span::styled(format!("{mark} {name}"), style),
                ])
            })
            .collect();
        frame.render_widget(
            Paragraph::new(steps).block(Block::bordered().border_type(BorderType::Rounded)),
            sidebar,
        );

        let mut lines: Vec<Line> = Vec::new();
        let mut menu_anchor = None;
        for (row, &i) in rows.iter().enumerate() {
            let focused = i == self.focus;
            if i == LOGIN {
                let button = " Sign in… (^L) ";
                lines.push(Line::from(if focused {
                    button.black().on_cyan().bold()
                } else {
                    button.cyan()
                }));
                continue;
            }
            if i == BUTTON {
                lines.push(Line::default());
                let button = if focused {
                    " Save ".black().on_cyan().bold()
                } else {
                    " Save ".cyan().on_dark_gray()
                };
                lines.push(Line::from(vec![Span::raw("  "), button]));
                continue;
            }
            let field = &self.fields[i];
            let shown = match &field.kind {
                Kind::Secret if !self.reveal && !field.value.is_empty() => {
                    "•".repeat(field.value.chars().count().min(24))
                }
                Kind::Select(_) => format!("{} ▾", field.value),
                _ => field.value.clone(),
            };
            let cursor = if focused && !matches!(field.kind, Kind::Select(_)) {
                "▏"
            } else {
                ""
            };
            let label_style = if focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            if focused {
                menu_anchor = Some(row as u16);
            }
            let error = self.field_error(i);
            let value_style = if error.is_some() {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            };
            let mark = match error {
                Some(_) => "  ✗".red(),
                None if CHECKED.contains(&i) && !field.value.trim().is_empty() => "  ✓".green(),
                None => Span::raw(""),
            };
            let line = Line::from(vec![
                if focused {
                    "▌ ".cyan()
                } else {
                    Span::raw("  ")
                },
                Span::styled(format!("{:<20} ", field.label), label_style),
                Span::styled(shown, value_style),
                Span::raw(cursor),
                mark,
            ]);
            lines.push(if focused {
                line.style(Style::new().bg(FOCUS_BG))
            } else {
                line
            });
        }
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::Cyan))
                    .title(format!(" {} ", STEPS[self.step]).bold()),
            ),
            body,
        );

        let (title, about, warning) = match self.fields.get(self.focus) {
            Some(field) => {
                let (about, warning) = help(self.focus);
                (field.label.as_str(), about, warning)
            }
            None if self.focus == LOGIN => (
                "OAuth sign-in",
                "Start browser authorization. The verification URL and one-time code appear here; Esc cancels while keeping your settings.",
                "",
            ),
            None => (
                "Save",
                "Tests the connection to the model, then writes the config. ^S saves from any field.",
                "",
            ),
        };
        let mut text = vec![Line::from(about)];
        if !warning.is_empty() {
            text.push(Line::from(format!("⚠ {warning}")).yellow());
        }
        if let Some(err) = self.field_error(self.focus) {
            text.push(Line::from(format!("✗ {err}")).red());
        }
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .border_style(Style::new().fg(Color::DarkGray))
                    .padding(Padding::horizontal(1))
                    .title(format!(" {title} ")),
            ),
            help_area,
        );
        frame.render_widget(Paragraph::new(self.status.clone()), status);
        let keys_text = if self.menu.is_some() {
            " ↑↓ choose  Enter select  Esc close"
        } else if self.step == STEP_MODEL && self.provider().uses_oauth() {
            " ←→ step  ↑↓ field  ^L sign in  ^S save  Esc back  ^C quit"
        } else {
            " ←→ step  ↑↓ field  Enter open/next  ^R show secrets  ^S save  Esc back  ^C quit"
        };
        frame.render_widget(Paragraph::new(keys_text).dark_gray(), keys);

        if let (Some(menu), Some(anchor), Some(Kind::Select(values))) = (
            &mut self.menu,
            menu_anchor,
            self.fields.get(self.focus).map(|f| &f.kind),
        ) {
            let area = dropdown_area(body, anchor, values.len() as u16);
            let list = List::new(values.iter().map(String::as_str))
                .block(Block::bordered().border_type(BorderType::Rounded))
                .highlight_style(Style::new().black().on_cyan())
                .highlight_symbol("› ");
            frame.render_widget(Clear, area);
            frame.render_stateful_widget(list, area, menu);
        }
        if let Some(login) = &self.login {
            let area = centered(frame.area(), 72, 11);
            let mut lines = vec![
                Line::from(provider_name(login.provider)).bold(),
                Line::default(),
            ];
            match &login.prompt {
                Some((url, code)) => {
                    lines.push(Line::from("Open this URL in your browser:"));
                    lines.push(Line::from(url.clone()).cyan());
                    lines.push(Line::from(format!("One-time code: {code}")).bold());
                    lines.push(Line::from("Waiting for browser authorization…"));
                }
                None => lines.push(Line::from("Starting sign-in…")),
            }
            lines.push(Line::default());
            lines.push(Line::from("Esc cancels · settings are kept"));
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                    Block::bordered()
                        .border_type(BorderType::Rounded)
                        .title(" OAuth sign-in ")
                        .padding(Padding::horizontal(1)),
                ),
                area,
            );
        }
        if self.plain {
            for cell in frame.buffer_mut().content.iter_mut() {
                cell.set_fg(Color::Reset).set_bg(Color::Reset);
            }
        }
    }

    /// What is wrong with field `i`'s value, if anything. Empty required fields only count once
    /// a save was tried, so a fresh form isn't all red.
    fn field_error(&self, i: usize) -> Option<String> {
        if !self.fields.get(i).is_some_and(|_| self.shown(i)) {
            return None;
        }
        let value = self.value(i);
        if value.is_empty() {
            let required = matches!(
                i,
                KEY | CUSTOM_MODEL
                    | TOKEN
                    | USERS
                    | GUILD_ID
                    | GUILD_USERS
                    | SEARCH_URL
                    | BRAVE_KEY
                    | MCP_NAME
                    | MCP_COMMAND
                    | MCP_URL
                    | DATABASE
                    | VOICE_KEY
                    | VOICE_MODEL
                    | VOICE_URL
                    | VOICE_LANGUAGE
            );
            return (self.attempted && required)
                .then(|| format!("{} is required", self.fields[i].label));
        }
        match i {
            USERS => parse_user_ids(&value).err(),
            GUILD_ID => parse_server_id(&value).err(),
            GUILD_USERS if value != "*" => parse_user_ids(&value).err(),
            VOICE_CHANNELS => parse_ids(&value, "channel").err(),
            TIMEZONE => self.timezone().err(),
            SEARCH_RESULTS => self.search_results().err(),
            MAX_TOKENS => self.number::<u32>(i, 1).err(),
            COMPACT_AT => self.number(i, config::MIN_COMPACT_AT_TOKENS).err(),
            BROWSER_WIDTH => self.number(i, config::MIN_SCREEN.0).err(),
            BROWSER_HEIGHT => self.number(i, config::MIN_SCREEN.1).err(),
            VIEWER_PORT | WEBHOOK_PORT => self.number::<u16>(i, 1).err(),
            VOICE_URL => {
                (!value.starts_with("wss://")).then(|| "must start with wss://".to_owned())
            }
            SEARCH_URL | MCP_URL => (!value.starts_with("http://")
                && !value.starts_with("https://"))
            .then(|| "must start with http:// or https://".to_owned()),
            _ => None,
        }
    }
}

/// Fields whose value is checked as it is typed, so a valid one gets a ✓.
const CHECKED: &[usize] = &[
    USERS,
    GUILD_ID,
    GUILD_USERS,
    TIMEZONE,
    SEARCH_URL,
    MCP_URL,
    SEARCH_RESULTS,
    MAX_TOKENS,
    COMPACT_AT,
    BROWSER_WIDTH,
    BROWSER_HEIGHT,
    VIEWER_PORT,
    WEBHOOK_PORT,
];

/// Background of the focused row.
const FOCUS_BG: Color = Color::Indexed(236);

/// Help for field `i`, and a warning to show with it ("" for none).
fn help(i: usize) -> (&'static str, &'static str) {
    let about = match i {
        KEY => "Your OpenCode Go or Anthropic API key. ^R shows it.",
        MODEL => "The model Mokuzu talks with. Pick Custom… to type another model ID.",
        CUSTOM_MODEL => "A model ID your provider serves.",
        TOKEN => {
            "Bot token from the Discord Developer Portal › Bot. Turn on Message Content Intent \
             there too, or the bot cannot connect."
        }
        USERS => {
            "Comma-separated Discord user IDs, used in DMs and in servers not added below. With \
             Developer Mode on, right-click a name › Copy User ID."
        }
        GUILD_ADD => {
            "Give one server its own users and reply rule. Servers already added are listed \
             below; set one to Remove to drop or replace it."
        }
        GUILD_ID => "With Developer Mode on, right-click the server › Copy Server ID.",
        VOICE => {
            "Lets /voice in a server start a voice call: mokuzu joins your voice channel and \
             talks through OpenAI's Realtime API, handing anything past small talk to the agent."
        }
        VOICE_KEY => "An OpenAI API key, from platform.openai.com. ^R shows it.",
        VOICE_MODEL => "The Realtime API model that talks.",
        VOICE_NAME => "The voice it speaks with; marin and cedar sound the most natural.",
        VOICE_URL => "The Realtime API's WebSocket endpoint. Change it only for a proxy.",
        VOICE_LANGUAGE => "The language calls are held in, e.g. Traditional Chinese.",
        VOICE_CHANNELS => {
            "Comma-separated voice channel IDs mokuzu joins as soon as you do, without /voice; \
             blank for none. Right-click the channel › Copy Channel ID."
        }
        GUILD_USERS => "Comma-separated user IDs, or * for everyone in the server.",
        GUILD_REPLY => "Every message, or only messages that @mention the bot, in threads too.",
        SEARCH => "Gives the model a web_search tool through SearXNG or the Brave Search API.",
        SEARCH_URL => "Your SearXNG instance, starting with http:// or https://.",
        SEARCH_USER | SEARCH_PASSWORD => {
            "Basic auth for SearXNG, if it needs it. Give both or neither."
        }
        BRAVE_KEY => "Your Brave Search API key.",
        LOG => "How much `mokuzu serve` logs.",
        DATABASE => "SQLite file that holds conversations and memory.",
        MCP_ADD => {
            "Connect an MCP server, a local command or a remote URL. Its tools reach the model \
             as <name>__<tool>."
        }
        MCP_NAME => "Letters, digits, _ and -. Tools are named <name>__<tool>.",
        MCP_COMMAND => "The program to start, like npx.",
        MCP_ARGS => "Arguments, separated by spaces.",
        MCP_ENV => "Environment variables as KEY=VALUE, separated by commas.",
        MCP_URL => "The server's URL, starting with http:// or https://.",
        MCP_TOKEN => "Bearer token for the server, if it needs one.",
        MCP_APPROVE => "Ask before each of this server's tool calls, or let them run.",
        APPROVAL => {
            "Who approves actions like settings changes and MCP calls: you every time, or \
             a reviewer model that passes low-risk ones and asks you about the rest."
        }
        REVIEWER => "The model that reviews actions when approval is Auto.",
        TIMEZONE => "IANA name like Asia/Taipei, for message times and scheduled jobs.",
        SEARCH_RESULTS => "How many results each web search returns, 1 to 20.",
        API => {
            "Which format the model's endpoint speaks. Auto guesses from the model name; pick one \
             if it guesses wrong."
        }
        BASE_URL => "The provider's endpoint. Change it only for a proxy or mirror.",
        MAX_TOKENS => "Longest reply the model may write, in tokens.",
        COMPACT_AT => {
            "When the conversation grows past about this many tokens, old tool output is cleared \
             and older turns are summarized. Lower it for models with a smaller context window."
        }
        BROWSER_WIDTH | BROWSER_HEIGHT => {
            "Screen size, in pixels, of the virtual desktop browsers draw on when no monitor is \
             plugged in. Larger makes screenshots and the /browser view sharper. Takes effect \
             when mokuzu next starts."
        }
        VIEWER_PORT => "Port of the /browser live view on this machine's network.",
        WEBHOOK_PORT => {
            "Port on this machine's network that webhooks mokuzu makes listen on; each POST to \
             one runs its instructions. Leave empty to turn webhooks off. Services on the \
             internet need a tunnel in front of it. Takes effect when mokuzu next starts."
        }
        PROVIDER => {
            "OpenCode Go and Anthropic use API keys. For GitHub Copilot or OpenAI ChatGPT, \
             choose Sign in… or press ^L to authorize in your browser. OAuth credentials \
             stay separate from API keys and from other providers."
        }
        _ => "Keep it, or Remove it.",
    };
    let warning = match i {
        GUILD_USERS => {
            "With *, anyone in the server can make the bot read files on this machine. Only the \
             main list can approve actions."
        }
        MCP_APPROVE => "Never ask lets the model call this server's tools on its own.",
        _ => "",
    };
    (about, warning)
}

/// Centers the authorization dialog, including in small terminals.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

/// Places the dropdown just under field row `row`, aligned with the value column.
fn dropdown_area(body: Rect, row: u16, items: u16) -> Rect {
    let x = body.x + 1 + 2 + 21;
    let y = body.y + 1 + row + 1;
    let [area] = Layout::horizontal([Constraint::Length(28)])
        .flex(Flex::Start)
        .areas(Rect::new(
            x,
            y,
            body.width.saturating_sub(x - body.x),
            items + 2,
        ));
    area
}

struct DiscordAnswers {
    token: String,
    allowed_users: Vec<u64>,
    guilds: Vec<GuildAnswers>,
}

/// The `[voice]` section.
struct VoiceAnswers {
    api_key: String,
    url: String,
    model: String,
    voice: String,
    language: String,
    channels: Vec<u64>,
}

/// One `[discord.guilds.<id>]` table.
struct GuildAnswers {
    id: u64,
    mention_only: bool,
    allowed_users: config::Users,
}

fn join_ids(ids: &[u64]) -> String {
    ids.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Comma-separated Discord IDs; `what` names them in errors ("user", "channel").
fn parse_ids(text: &str, what: &str) -> std::result::Result<Vec<u64>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u64>()
                .map_err(|_| format!("`{part}` is not a Discord {what} ID (digits only)"))
        })
        .collect()
}

fn parse_server_id(text: &str) -> std::result::Result<u64, String> {
    text.parse::<u64>()
        .map_err(|_| format!("`{text}` is not a Discord server ID (digits only)"))
}

fn parse_user_ids(text: &str) -> std::result::Result<Vec<u64>, String> {
    let ids = parse_ids(text, "user")?;
    if ids.is_empty() {
        return Err("enter at least one user ID".to_owned());
    }
    Ok(ids)
}

fn default_database_path() -> Result<PathBuf> {
    Ok(std::env::home_dir()
        .context("cannot find home directory")?
        .join(".local/share/mokuzu/mokuzu.db"))
}

enum SearchAnswers {
    Searxng {
        url: String,
        auth: Option<(String, String)>,
        results: usize,
    },
    Brave {
        api_key: String,
        results: usize,
    },
}

struct Answers {
    provider: Provider,
    /// Other providers' keys and base URLs, saved in their own sections.
    other_keys: Vec<(Provider, String, String)>,
    model: String,
    api_key: String,
    discord: DiscordAnswers,
    voice: Option<VoiceAnswers>,
    search: Option<SearchAnswers>,
    log_level: String,
    /// `Some` for auto approval, holding the reviewer model.
    auto_approval: Option<String>,
    database: PathBuf,
    timezone: String,
    options: ModelOptions,
    browser: BrowserOptions,
    /// `[webhook] port`; `None` leaves webhooks off.
    webhook_port: Option<u16>,
}

/// `[browser]` keys.
struct BrowserOptions {
    width: u32,
    height: u32,
    viewer_port: u16,
}

struct ModelOptions {
    api: String,
    base_url: String,
    max_tokens: u32,
    compact_at_tokens: usize,
}

/// MCP servers to remove, and one to add as `(name, table)`.
type McpChanges = (Vec<String>, Option<(String, Table)>);

/// `text` with the `[mcp.servers]` from the `old` config file carried over, minus `remove`, plus `add`.
/// Old entries are copied as written, so their comments and secrets survive.
fn merge_mcp(
    text: &str,
    old: Option<&str>,
    remove: &[String],
    add: Option<(String, Table)>,
) -> String {
    let Ok(mut new) = text.parse::<DocumentMut>() else {
        return text.to_owned();
    };
    let mut servers = old
        .and_then(|old| old.parse::<DocumentMut>().ok())
        .and_then(|old| old.get("mcp")?.get("servers")?.as_table().cloned())
        .unwrap_or_default();
    for name in remove {
        servers.remove(name);
    }
    if let Some((name, table)) = add {
        servers.insert(&name, Item::Table(table));
    }
    if servers.is_empty() {
        return new.to_string();
    }
    servers.set_implicit(true);
    let mut mcp = Table::new();
    mcp.set_implicit(true);
    mcp.insert("servers", Item::Table(servers));
    new["mcp"] = Item::Table(mcp);
    new.to_string()
}

/// Quotes and escapes a TOML string.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn render_toml(answers: &Answers) -> String {
    let options = &answers.options;
    let mut text = format!(
        "# Written by `mokuzu configure`; run it again to change these settings.\n\
         version = {version}\n\
         timezone = {zone}\n\
         \n\
         [model]\n\
         name = {model}\n\
         provider = {section}\n\
         api = {api}\n\
         max_tokens = {max_tokens}\n\
         compact_at_tokens = {compact_at}\n\
         \n\
         [{section_name}]\n\
         api_key = {key}\n\
         base_url = {url}\n",
        version = config::VERSION,
        zone = quote(&answers.timezone),
        model = quote(&answers.model),
        section = quote(answers.provider.section()),
        section_name = answers.provider.section(),
        api = quote(&options.api),
        max_tokens = options.max_tokens,
        compact_at = options.compact_at_tokens,
        key = quote(&answers.api_key),
        url = quote(&options.base_url),
    );
    for (provider, key, url) in &answers.other_keys {
        text.push_str(&format!(
            "\n[{}]\napi_key = {}\nbase_url = {}\n",
            provider.section(),
            quote(key),
            quote(url)
        ));
    }
    let browser = &answers.browser;
    text.push_str(&format!(
        "\n\
         [log]\n\
         level = {log}\n\
         \n\
         [database]\n\
         path = {db}\n\
         \n\
         [browser]\n\
         width = {width}\n\
         height = {height}\n\
         viewer_port = {port}\n\
         \n\
         [approval]\n",
        log = quote(&answers.log_level),
        db = quote(&answers.database.to_string_lossy()),
        width = browser.width,
        height = browser.height,
        port = browser.viewer_port,
    ));
    match &answers.auto_approval {
        Some(reviewer) => text.push_str(&format!("mode = \"auto\"\nmodel = {}\n", quote(reviewer))),
        None => text.push_str("mode = \"ask\"\n"),
    }
    match &answers.search {
        Some(SearchAnswers::Searxng { url, auth, results }) => {
            text.push_str(&format!(
                "\n[tools.searxng]\nurl = {}\nresults = {results}\n",
                quote(url)
            ));
            if let Some((user, password)) = auth {
                text.push_str(&format!(
                    "username = {}\npassword = {}\n",
                    quote(user),
                    quote(password)
                ));
            }
        }
        Some(SearchAnswers::Brave { api_key, results }) => text.push_str(&format!(
            "\n[tools.brave]\napi_key = {}\nresults = {results}\n",
            quote(api_key)
        )),
        None => {}
    }
    let discord = &answers.discord;
    text.push_str(&format!(
        "\n[discord]\ntoken = {}\nallowed_users = [{}]\n",
        quote(&discord.token),
        join_ids(&discord.allowed_users)
    ));
    for guild in &discord.guilds {
        text.push_str(&format!(
            "\n[discord.guilds.{}]\nallowed_users = {}\n",
            guild.id,
            match &guild.allowed_users {
                config::Users::Everyone => quote("*"),
                config::Users::Only(ids) => format!("[{}]", join_ids(ids)),
            }
        ));
        text.push_str(&format!("mention_only = {}\n", guild.mention_only));
    }
    if let Some(voice) = &answers.voice {
        text.push_str(&format!(
            "\n[voice]\napi_key = {}\nurl = {}\nmodel = {}\nvoice = {}\nlanguage = {}\n\
             channels = [{}]\n",
            quote(&voice.api_key),
            quote(&voice.url),
            quote(&voice.model),
            quote(&voice.voice),
            quote(&voice.language),
            join_ids(&voice.channels)
        ));
    }
    if let Some(port) = answers.webhook_port {
        text.push_str(&format!("\n[webhook]\nport = {port}\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_step_validates_new_server() {
        let config = Config::parse(
            "[opencode-go]\napi_key = \"k\"\n[mcp.servers.linear]\nurl = \"https://a.example\"\n",
        )
        .expect("valid");
        let mut form = Form::new(Some(&config)).expect("form");
        form.fields[MCP_ADD].value = HTTP.to_owned();
        form.fields[MCP_NAME].value = "linear".to_owned();
        form.fields[MCP_URL].value = "https://mcp.linear.app/mcp".to_owned();
        assert!(
            form.mcp().is_err(),
            "existing name is refused unless removed"
        );
        form.fields[MCP_FIRST].value = REMOVE.to_owned();
        let (remove, add) = form.mcp().expect("replaces it");
        assert_eq!(remove, ["linear"]);
        assert_eq!(add.expect("added").0, "linear");

        form.fields[MCP_ADD].value = STDIO.to_owned();
        form.fields[MCP_NAME].value = "gh".to_owned();
        form.fields[MCP_COMMAND].value = "npx".to_owned();
        form.fields[MCP_ARGS].value = "-y  server-github".to_owned();
        form.fields[MCP_ENV].value = "TOKEN=abc, MODE=ro".to_owned();
        let (_, add) = form.mcp().expect("valid");
        let merged = merge_mcp("[opencode-go]\napi_key = \"k\"\n", None, &[], add);
        let gh = &Config::parse(&merged).expect("valid").mcp["gh"];
        assert_eq!(gh.args, ["-y", "server-github"]);
        assert_eq!(gh.env["MODE"].as_str(), "ro");
        form.fields[MCP_ENV].value = "oops".to_owned();
        assert!(form.mcp().is_err());
    }

    #[test]
    fn merge_mcp_keeps_removes_and_adds_servers() {
        let new = "[opencode-go]\napi_key = \"k\"\n";
        let old = "[opencode-go]\napi_key = \"old\"\n\n\
                   [mcp.servers.fs]\ncommand = \"npx\"\n\n\
                   [mcp.servers.gone]\nurl = \"https://gone.example\"\n";
        let mut linear = Table::new();
        linear["url"] = toml_edit::value("https://mcp.linear.app/mcp");
        linear["token"] = toml_edit::value("lin_api_x");
        let merged = merge_mcp(
            new,
            Some(old),
            &["gone".to_owned()],
            Some(("linear".to_owned(), linear)),
        );
        let config = Config::parse(&merged).expect("valid");
        assert_eq!(config.api_key.as_str(), "k");
        assert_eq!(config.mcp["fs"].command.as_deref(), Some("npx"));
        assert!(!config.mcp.contains_key("gone"));
        assert_eq!(
            config.mcp["linear"].token.as_ref().map(|t| t.as_str()),
            Some("lin_api_x")
        );
        assert_eq!(merge_mcp(new, None, &[], None), new);
    }

    #[test]
    fn rendered_config_parses_back() {
        let text = render_toml(&Answers {
            provider: Provider::OpencodeGo,
            other_keys: Vec::new(),
            model: "qwen3.8-max".to_owned(),
            api_key: "sk-\"quoted\"".to_owned(),
            discord: DiscordAnswers {
                token: "tok".to_owned(),
                allowed_users: vec![1, 22],
                guilds: vec![
                    GuildAnswers {
                        id: 100,
                        mention_only: true,
                        allowed_users: config::Users::Only(vec![2, 3]),
                    },
                    GuildAnswers {
                        id: 200,
                        mention_only: false,
                        allowed_users: config::Users::Everyone,
                    },
                ],
            },
            voice: Some(VoiceAnswers {
                api_key: "sk-voice".to_owned(),
                url: config::DEFAULT_REALTIME_URL.to_owned(),
                model: config::DEFAULT_REALTIME_MODEL.to_owned(),
                voice: "cedar".to_owned(),
                language: "繁體中文".to_owned(),
                channels: vec![300, 301],
            }),
            search: Some(SearchAnswers::Searxng {
                url: "https://search.example.com".to_owned(),
                auth: Some(("me".to_owned(), "p\"w".to_owned())),
                results: 8,
            }),
            log_level: "info".to_owned(),
            auto_approval: Some("glm-5.3-flash".to_owned()),
            database: PathBuf::from("/tmp/m.db"),
            timezone: "Asia/Taipei".to_owned(),
            options: ModelOptions {
                api: "openai".to_owned(),
                base_url: "https://proxy.example/v1".to_owned(),
                max_tokens: 8000,
                compact_at_tokens: 50_000,
            },
            browser: BrowserOptions {
                width: 1920,
                height: 800,
                viewer_port: 8765,
            },
            webhook_port: Some(22051),
        });
        assert!(
            config::migrate(&text).expect("valid").is_none(),
            "written at the current version, every key spelled out"
        );
        let config = Config::parse(&text).expect("valid");
        assert_eq!(config.timezone, chrono_tz::Tz::Asia__Taipei);
        assert_eq!(config.api, Api::Openai);
        assert_eq!(config.base_url, "https://proxy.example/v1");
        assert_eq!(
            (config.max_tokens, config.compact_at_tokens),
            (8000, 50_000)
        );
        assert_eq!(config.browser_screen, (1920, 800));
        assert_eq!(config.viewer_port, 8765);
        assert_eq!(config.webhook_port, Some(22051));
        assert_eq!(config.model, "qwen3.8-max");
        assert_eq!(
            config.approval,
            config::Approval::Auto {
                model: "glm-5.3-flash".to_owned()
            }
        );
        assert_eq!(config.api_key.as_str(), "sk-\"quoted\"");
        let discord = config.discord.as_ref().expect("discord");
        assert_eq!(discord.allowed_users, [1, 22]);
        assert!(discord.mention_only(Some(100)) && discord.can_talk(Some(100), 2));
        assert_eq!(discord.guilds[&200].allowed_users, config::Users::Everyone);
        let voice = config.voice.as_ref().expect("voice");
        assert_eq!(
            (voice.api_key.as_str(), voice.voice.as_str()),
            ("sk-voice", "cedar")
        );
        assert_eq!(voice.channels, [300, 301]);
        assert_eq!(voice.language, "繁體中文");
        let mut reopened = Form::new(Some(&config)).expect("form");
        assert_eq!(reopened.value(VOICE), ENABLED);
        assert_eq!(reopened.value(VOICE_KEY), "sk-voice");
        assert_eq!(reopened.value(VOICE_CHANNELS), "300, 301");
        reopened.step = STEP_VOICE;
        assert!(reopened.visible().contains(&VOICE_MODEL));
        reopened.fields[VOICE_URL].value = "https://not-a-socket".to_owned();
        assert!(
            reopened.check_step(STEP_VOICE).is_err(),
            "the endpoint is a WebSocket"
        );
        reopened.fields[VOICE].value = DISABLED.to_owned();
        assert!(!reopened.visible().contains(&VOICE_MODEL));
        assert!(reopened.voice().expect("valid").is_none());
        reopened.fields[VOICE].value = ENABLED.to_owned();
        reopened.fields[VOICE_URL].value = config::DEFAULT_REALTIME_URL.to_owned();
        assert_eq!(reopened.value(API), "openai");
        assert_eq!(reopened.value(COMPACT_AT), "50000");
        assert_eq!(reopened.value(VIEWER_PORT), "8765");
        assert_eq!(reopened.value(WEBHOOK_PORT), "22051");
        assert_eq!(reopened.model_options().expect("valid").max_tokens, 8000);
        let first = reopened.guild_first();
        assert_eq!(reopened.fields[first].label, "100");
        assert_eq!(
            reopened.fields[first].value,
            "Keep · 2, 3 · only when @mentioned"
        );
        assert_eq!(
            reopened.fields[first + 1].value,
            "Keep · everyone · every message"
        );
        reopened.fields[first + 1].value = REMOVE.to_owned();
        let kept = reopened.guild_answers().expect("valid");
        assert_eq!(
            kept.iter()
                .map(|g| (g.id, g.mention_only, g.allowed_users.clone()))
                .collect::<Vec<_>>(),
            [(100, true, config::Users::Only(vec![2, 3]))]
        );
        let Some(config::Search::Searxng(searxng)) = config.search else {
            panic!("search section");
        };
        assert_eq!(searxng.url, "https://search.example.com");
        assert_eq!(searxng.results, 8);
        let (user, password) = searxng.auth.expect("basic auth");
        assert_eq!((user.as_str(), password.as_str()), ("me", "p\"w"));
    }

    #[test]
    fn brave_choice_renders_its_section() {
        let mut form = Form::new(None).expect("form");
        form.fields[SEARCH].value = BRAVE.to_owned();
        form.step = STEP_SEARCH;
        assert!(form.visible().contains(&BRAVE_KEY));
        assert!(!form.visible().contains(&SEARCH_URL));
        assert!(form.check_step(STEP_SEARCH).is_err(), "Brave needs a key");
        form.fields[BRAVE_KEY].value = "bk".to_owned();
        let Some(answers) = form.search().expect("valid") else {
            panic!("search on");
        };
        let text = render_toml(&Answers {
            provider: Provider::OpencodeGo,
            other_keys: Vec::new(),
            model: "m".to_owned(),
            api_key: "k".to_owned(),
            discord: DiscordAnswers {
                token: "t".to_owned(),
                allowed_users: vec![1],
                guilds: Vec::new(),
            },
            voice: None,
            search: Some(answers),
            log_level: "warn".to_owned(),
            auto_approval: None,
            database: PathBuf::from("/tmp/m.db"),
            timezone: "UTC".to_owned(),
            options: form.model_options().expect("valid"),
            browser: form.browser_options().expect("valid"),
            webhook_port: None,
        });
        let config = Config::parse(&text).expect("valid");
        let Some(config::Search::Brave(brave)) = &config.search else {
            panic!("brave section");
        };
        assert_eq!((brave.api_key.as_str(), brave.results), ("bk", 5));
        let reopened = Form::new(Some(&config)).expect("form");
        assert_eq!(reopened.fields[SEARCH].value, BRAVE);
        assert_eq!(reopened.fields[BRAVE_KEY].value, "bk");
    }

    #[test]
    fn each_provider_keeps_its_own_key_and_base_url() {
        let mut form = Form::new(None).expect("form");
        form.fields[KEY].value = "o-key".to_owned();
        form.fields[PROVIDER].value = ANTHROPIC.to_owned();
        form.switch_provider(Provider::OpencodeGo);
        assert_eq!(form.value(KEY), "", "no key carried over");
        assert_eq!(form.value(MODEL), "claude-opus-5-5");
        assert_eq!(form.value(BASE_URL), config::ANTHROPIC_BASE_URL);
        form.step = STEP_ADVANCED;
        assert!(!form.visible().contains(&API), "Anthropic has one format");
        form.fields[KEY].value = "sk-ant".to_owned();
        form.fields[TOKEN].value = "t".to_owned();
        form.fields[USERS].value = "1".to_owned();
        let answers = form.answers().expect("valid");
        assert_eq!(
            answers.options.base_url,
            config::ANTHROPIC_BASE_URL,
            "defaults are written out"
        );
        let text = render_toml(&answers);
        assert!(
            text.contains("[anthropic]\napi_key = \"sk-ant\"\n")
                && text.contains("[opencode-go]\napi_key = \"o-key\"\n")
                && text.contains("provider = \"anthropic\""),
            "{text}"
        );
        let config = Config::parse(&text).expect("valid");
        assert_eq!(config.provider, Provider::Anthropic);
        assert_eq!(config.api_key.as_str(), "sk-ant");
        let mut reopened = Form::new(Some(&config)).expect("form");
        assert_eq!(reopened.value(PROVIDER), ANTHROPIC);
        assert_eq!(reopened.value(KEY), "sk-ant");
        assert_eq!(reopened.value(MODEL), "claude-opus-5-5");
        let Kind::Select(reviewers) = &reopened.fields[REVIEWER].kind else {
            panic!("reviewer menu");
        };
        assert!(reviewers.iter().any(|r| r == "claude-haiku-5-5"));
        reopened.fields[PROVIDER].value = OPENCODE_GO.to_owned();
        reopened.switch_provider(Provider::Anthropic);
        assert_eq!(reopened.value(KEY), "o-key");
        assert_eq!(reopened.value(BASE_URL), config::DEFAULT_BASE_URL);
    }

    #[test]
    fn oauth_provider_switches_round_trip_without_keys_or_leaking_an_api_key() {
        let mut form = Form::new(None).unwrap();
        form.fields[TOKEN].value = "discord-token".to_owned();
        form.fields[USERS].value = "1".to_owned();
        form.fields[KEY].value = "opencode-key".to_owned();
        form.fields[PROVIDER].value = GITHUB_COPILOT.to_owned();
        form.switch_provider(Provider::OpencodeGo);
        form.step = STEP_MODEL;
        form.check_step(STEP_MODEL).unwrap();
        assert!(!form.visible().contains(&KEY));
        assert!(form.value(KEY).is_empty());
        assert_eq!(form.model_options().unwrap().api, "responses");
        form.fields[PROVIDER].value = CHATGPT.to_owned();
        form.switch_provider(Provider::GithubCopilot);
        form.check_step(STEP_MODEL).unwrap();
        let text = render_toml(&form.answers().unwrap());
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.provider, Provider::Chatgpt);
        assert!(config.api_key.as_str().is_empty());
        assert!(
            config
                .keys
                .iter()
                .any(|(p, _)| *p == Provider::GithubCopilot)
        );
        let mut reopened = Form::new(Some(&config)).unwrap();
        assert_eq!(reopened.value(PROVIDER), CHATGPT);
        reopened.fields[PROVIDER].value = OPENCODE_GO.to_owned();
        reopened.switch_provider(Provider::Chatgpt);
        assert_eq!(reopened.value(KEY), "opencode-key");
        reopened.fields[PROVIDER].value = GITHUB_COPILOT.to_owned();
        reopened.switch_provider(Provider::OpencodeGo);
        assert_eq!(reopened.value(MODEL), COPILOT_MODELS[0]);
        assert!(reopened.value(KEY).is_empty());
        let config = Config::parse(&render_toml(&reopened.answers().unwrap())).unwrap();
        assert_eq!(config.provider, Provider::GithubCopilot);
    }

    #[test]
    fn steps_validate_and_hide_conditional_fields() {
        let mut form = Form::new(None).expect("form");
        assert_eq!(form.step, STEP_DISCORD, "the bot comes first");
        assert!(form.visible().contains(&TOKEN));
        assert_eq!(
            form.check_step(form.step).err().as_deref(),
            Some("Bot token is required")
        );

        form.step = STEP_MODEL;
        assert_eq!(
            form.check_step(form.step).err().as_deref(),
            Some("API key is required")
        );
        form.fields[KEY].value = "k".to_owned();
        assert!(!form.visible().contains(&CUSTOM_MODEL));
        form.fields[MODEL].value = CUSTOM.to_owned();
        assert!(form.visible().contains(&CUSTOM_MODEL));
        assert!(
            form.check_step(form.step).is_err(),
            "custom model needs an ID"
        );
        form.fields[CUSTOM_MODEL].value = "qwen-next".to_owned();
        form.check_step(form.step).expect("model step valid");

        form.step = STEP_DISCORD;
        form.fields[TOKEN].value = "t".to_owned();
        form.fields[USERS].value = "12, x".to_owned();
        assert!(form.check_step(form.step).is_err());
        form.fields[USERS].value = "12, 34".to_owned();
        assert!(!form.visible().contains(&GUILD_ID));
        form.fields[GUILD_ADD].value = YES.to_owned();
        assert!(form.visible().contains(&GUILD_REPLY));
        assert!(form.check_step(form.step).is_err(), "server needs an ID");
        assert_eq!(
            form.field_error(GUILD_ID),
            None,
            "empty only counts after a save"
        );
        form.attempted = true;
        assert!(form.field_error(GUILD_ID).is_some());
        form.fields[GUILD_ID].value = "x".to_owned();
        form.fields[GUILD_USERS].value = "12".to_owned();
        assert!(form.field_error(GUILD_ID).is_some());
        assert!(form.check_step(form.step).is_err(), "server ID is digits");
        form.fields[GUILD_ID].value = "100".to_owned();
        form.fields[GUILD_USERS].value = String::new();
        assert!(form.check_step(form.step).is_err(), "server needs users");
        form.fields[GUILD_USERS].value = "*".to_owned();
        form.fields[GUILD_REPLY].value = MENTIONED.to_owned();
        form.check_step(form.step).expect("discord step valid");

        form.step = STEP_VOICE;
        assert!(
            !form.visible().contains(&VOICE_KEY),
            "calls are off by default"
        );
        form.check_step(form.step).expect("voice step valid");

        form.step = STEP_SEARCH;
        assert!(!form.visible().contains(&SEARCH_URL));
        form.fields[SEARCH].value = SEARXNG.to_owned();
        assert!(form.visible().contains(&SEARCH_PASSWORD));
        assert!(!form.visible().contains(&BRAVE_KEY));
        form.fields[SEARCH_URL].value = "search.example.com".to_owned();
        assert!(form.check_step(form.step).is_err(), "URL needs a scheme");
        form.fields[SEARCH_URL].value = "https://search.example.com".to_owned();
        form.fields[SEARCH_USER].value = "me".to_owned();
        assert!(
            form.check_step(form.step).is_err(),
            "username without password"
        );
        form.fields[SEARCH_PASSWORD].value = "pw".to_owned();
        form.fields[SEARCH_RESULTS].value = "21".to_owned();
        assert!(form.check_step(form.step).is_err(), "at most 20 results");
        form.fields[SEARCH_RESULTS].value = "8".to_owned();
        form.check_step(form.step).expect("search step valid");

        form.step = STEP_ADVANCED;
        form.fields[TIMEZONE].value = "Taipei".to_owned();
        assert!(form.check_step(form.step).is_err(), "not an IANA name");
        form.fields[TIMEZONE].value = "Asia/Taipei".to_owned();
        form.fields[COMPACT_AT].value = "500".to_owned();
        assert!(form.check_step(form.step).is_err(), "below the minimum");
        assert!(form.field_error(COMPACT_AT).is_some());
        form.fields[COMPACT_AT].value = config::DEFAULT_COMPACT_AT_TOKENS.to_string();
        form.fields[BROWSER_WIDTH].value = "100".to_owned();
        assert!(form.check_step(form.step).is_err(), "screen too small");
        form.fields[BROWSER_WIDTH].value = config::DEFAULT_SCREEN.0.to_string();
        form.fields[VIEWER_PORT].value = "70000".to_owned();
        assert!(form.check_step(form.step).is_err(), "not a port");
        form.fields[VIEWER_PORT].value = config::DEFAULT_VIEWER_PORT.to_string();
        form.fields[WEBHOOK_PORT].value = "0".to_owned();
        assert!(form.check_step(form.step).is_err(), "not a port");
        form.fields[WEBHOOK_PORT].value.clear();
        form.check_step(form.step).expect("advanced step valid");
        let browser = form.browser_options().expect("valid");
        assert_eq!(
            (browser.width, browser.viewer_port),
            (config::DEFAULT_SCREEN.0, config::DEFAULT_VIEWER_PORT),
            "defaults are written out"
        );
        form.fields[COMPACT_AT].value.clear();
        assert_eq!(
            form.model_options().expect("valid").compact_at_tokens,
            config::DEFAULT_COMPACT_AT_TOKENS,
            "an empty field is written as its default"
        );

        let answers = form.answers().expect("all steps valid");
        assert_eq!(answers.model, "qwen-next");
        let discord = answers.discord;
        assert_eq!(discord.allowed_users, [12, 34]);
        assert_eq!(
            discord
                .guilds
                .iter()
                .map(|g| (g.id, g.mention_only, g.allowed_users.clone()))
                .collect::<Vec<_>>(),
            [(100, true, config::Users::Everyone)]
        );
    }

    #[test]
    fn draws_step_and_opens_dropdown() {
        let mut form = Form::new(None).expect("form");
        form.fields[KEY].value = "sk-secret".to_owned();
        form.step = STEP_MODEL;
        form.focus = MODEL;
        form.open_menu();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| form.draw(frame)).expect("draw");
        let screen = format!("{}", terminal.backend());
        assert!(screen.contains("● Model"));
        assert!(
            screen.contains("qwen3.8-max"),
            "dropdown lists models:\n{screen}"
        );
        assert!(!screen.contains("sk-secret"));
    }

    #[tokio::test]
    async fn oauth_dialog_displays_device_code_and_returns_to_the_form_after_success() {
        let mut form = Form::new(None).unwrap();
        form.fields[PROVIDER].value = CHATGPT.to_owned();
        form.switch_provider(Provider::OpencodeGo);
        form.step = STEP_MODEL;
        form.focus = LOGIN;
        assert!(form.visible().contains(&LOGIN));
        let (sender, updates) = mpsc::channel();
        form.login = Some(Login {
            provider: Provider::Chatgpt,
            updates,
            task: tokio::spawn(std::future::pending()),
            prompt: None,
        });
        sender
            .send(LoginUpdate::Prompt {
                url: "https://auth.openai.com/codex/device".to_owned(),
                code: "TEST-1234".to_owned(),
            })
            .unwrap();
        form.poll_login();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| form.draw(frame)).unwrap();
        let screen = format!("{}", terminal.backend());
        assert!(screen.contains("OAuth sign-in"));
        assert!(screen.contains("https://auth.openai.com/codex/device"));
        assert!(screen.contains("TEST-1234"));
        sender.send(LoginUpdate::Done(Ok(()))).unwrap();
        form.poll_login();
        assert!(form.login.is_none());
        assert!(form.status.to_string().contains("Signed in"));
        assert_eq!(form.provider(), Provider::Chatgpt);
        assert_eq!(form.value(MODEL), CHATGPT_MODELS[0]);
        terminal.draw(|frame| form.draw(frame)).unwrap();
        assert!(!format!("{}", terminal.backend()).contains("TEST-1234"));
    }

    #[tokio::test]
    async fn cancelling_oauth_aborts_polling_without_discarding_settings() {
        let mut form = Form::new(None).unwrap();
        form.fields[TOKEN].value = "unsaved-discord-token".to_owned();
        let (_sender, updates) = mpsc::channel();
        let task = tokio::spawn(std::future::pending());
        let abort = task.abort_handle();
        form.login = Some(Login {
            provider: Provider::GithubCopilot,
            updates,
            task,
            prompt: None,
        });
        form.cancel_login();
        tokio::task::yield_now().await;
        assert!(abort.is_finished());
        assert!(form.login.is_none());
        assert_eq!(form.value(TOKEN), "unsaved-discord-token");
        assert!(form.status.to_string().contains("cancelled"));
    }

    #[test]
    fn user_ids_are_validated() {
        assert_eq!(parse_user_ids(" 1, 2 ,"), Ok(vec![1, 2]));
        assert!(parse_user_ids("abc").is_err());
        assert!(parse_user_ids(" , ").is_err());
    }
}
