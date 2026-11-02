//! TOML config file: the model's provider (API key or OAuth), tools, MCP servers, logging, storage, and Discord.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

mod migrate;

pub use migrate::VERSION;
#[cfg(test)]
pub use migrate::migrate;

pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 16_000;
pub const DEFAULT_COMPACT_AT_TOKENS: usize = 200_000;
pub const MIN_COMPACT_AT_TOKENS: usize = 10_000;
/// Size of the virtual desktop browsers draw on, when there is no real screen.
pub const DEFAULT_SCREEN: (u32, u32) = (1280, 800);
pub const MIN_SCREEN: (u32, u32) = (640, 480);
pub const DEFAULT_VIEWER_PORT: u16 = 22050;
pub const DEFAULT_REALTIME_URL: &str = "wss://api.openai.com/v1/realtime";
pub const DEFAULT_REALTIME_MODEL: &str = "gpt-realtime-2.1-mini";
/// What a voice call speaks until set otherwise.
pub const DEFAULT_VOICE_LANGUAGE: &str = "English";
/// Voices the Realtime API offers; `marin` and `cedar` sound the most natural.
pub const REALTIME_VOICES: [&str; 10] = [
    "marin", "cedar", "alloy", "ash", "ballad", "coral", "echo", "sage", "shimmer", "verse",
];

/// Which OpenCode Go endpoint format a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Api {
    /// `/messages`: MiniMax, Qwen, and Claude.
    Anthropic,
    /// `/chat/completions`: GLM, Kimi, DeepSeek, and the rest.
    Openai,
    /// `/responses`, OpenAI's Responses API: GPT, Grok, and Muse Spark.
    Responses,
    /// Google's `generateContent`: Gemini.
    Gemini,
}

impl Api {
    pub const ALL: [Self; 4] = [Self::Anthropic, Self::Openai, Self::Responses, Self::Gemini];

    /// The name `model.api` takes.
    pub fn name(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
            Self::Responses => "responses",
            Self::Gemini => "gemini",
        }
    }

    /// OpenCode Go serves MiniMax, Qwen, and Claude in Anthropic format and most others as chat completions.
    // ponytail: name-prefix guess; set `model.api` for anything it gets wrong.
    pub fn for_model(model: &str) -> Self {
        if ["minimax", "qwen", "claude"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            Self::Anthropic
        } else if ["gpt", "grok", "muse"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            Self::Responses
        } else if model.starts_with("gemini") {
            Self::Gemini
        } else {
            Self::Openai
        }
    }
}

/// Who serves the model, with a separate config section for each provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    OpencodeGo,
    /// Anthropic's own API, which speaks only the Anthropic format.
    Anthropic,
    /// GitHub Copilot subscription, authenticated with device OAuth.
    GithubCopilot,
    /// OpenAI's ChatGPT subscription, through Rig's Codex OAuth provider.
    Chatgpt,
}

impl Provider {
    pub const ALL: [Self; 4] = [
        Self::OpencodeGo,
        Self::Anthropic,
        Self::GithubCopilot,
        Self::Chatgpt,
    ];

    pub fn uses_oauth(self) -> bool {
        matches!(self, Self::GithubCopilot | Self::Chatgpt)
    }

    /// Default wire format. Copilot can override this for models with different endpoint support.
    pub fn api_for(self, model: &str) -> Api {
        match self {
            Self::OpencodeGo => Api::for_model(model),
            Self::Anthropic => Api::Anthropic,
            Self::GithubCopilot
                if {
                    let name = model.to_ascii_lowercase();
                    name.contains("codex")
                        || name.starts_with("gpt-6")
                        || name.starts_with("gpt-5.6")
                } =>
            {
                Api::Responses
            }
            Self::GithubCopilot => Api::Openai,
            Self::Chatgpt => Api::Responses,
        }
    }

    /// The config section holding its key.
    pub fn section(self) -> &'static str {
        match self {
            Self::OpencodeGo => "opencode-go",
            Self::Anthropic => "anthropic",
            Self::GithubCopilot => "github-copilot",
            Self::Chatgpt => "chatgpt",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpencodeGo => DEFAULT_BASE_URL,
            Self::Anthropic => ANTHROPIC_BASE_URL,
            Self::GithubCopilot => "https://api.githubcopilot.com",
            Self::Chatgpt => "https://chatgpt.com/backend-api/codex",
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpencodeGo => "minimax-m3",
            Self::Anthropic => "claude-opus-5-5",
            Self::GithubCopilot => "gpt-4.1",
            Self::Chatgpt => "gpt-6.1-sol",
        }
    }
}

/// One provider's section: its key, and the base URL it is reached at.
#[derive(Debug, Clone)]
pub struct ProviderKey {
    pub api_key: Secret,
    pub base_url: String,
}

/// API key or token whose `Debug` output is redacted.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    /// Format version; `parse` migrates older files first, so it is always `VERSION` here.
    #[serde(default, rename = "version")]
    _version: Option<i64>,
    /// IANA name, e.g. `Asia/Taipei`.
    timezone: String,
    model: ModelSection,
    #[serde(rename = "opencode-go")]
    opencode_go: Option<ProviderSection>,
    anthropic: Option<ProviderSection>,
    #[serde(rename = "github-copilot")]
    github_copilot: Option<ProviderSection>,
    chatgpt: Option<ProviderSection>,
    #[serde(default)]
    tools: ToolsSection,
    #[serde(default)]
    mcp: McpSection,
    approval: ApprovalSection,
    log: LogSection,
    discord: Option<DiscordSection>,
    voice: Option<VoiceSection>,
    webhook: Option<WebhookSection>,
    database: DatabaseSection,
    browser: BrowserSection,
}

/// `[webhook]`: the HTTP server outside services post events to, waking webhooks.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookSection {
    port: u16,
}

/// `[browser]`: the virtual desktop browsers draw on, and its live view.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserSection {
    /// Size in pixels of the Xvfb desktop.
    width: u32,
    height: u32,
    /// Port of the `/browser` live view.
    viewer_port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DatabaseSection {
    path: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelSection {
    name: String,
    /// Which provider section is in use.
    provider: Provider,
    /// Wire format.
    api: Api,
    max_tokens: u32,
    /// Estimated history size that triggers compaction.
    compact_at_tokens: usize,
}

/// A provider's key and endpoint. OAuth providers leave the key empty and use their fixed endpoint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderSection {
    api_key: Option<Secret>,
    base_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsSection {
    searxng: Option<SearxngSection>,
    brave: Option<BraveSection>,
}

/// `[tools.searxng]`: a SearXNG instance for the `web_search` tool, optionally behind basic auth.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearxngSection {
    url: String,
    username: Option<String>,
    password: Option<Secret>,
    results: usize,
}

impl SearxngSection {
    fn validate(self) -> Result<Searxng> {
        let url = self.url.trim_end_matches('/').to_owned();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            bail!("tools.searxng.url must start with http:// or https://");
        }
        let auth = match (self.username, self.password) {
            (Some(user), Some(password)) => Some((user, password)),
            (None, None) => None,
            _ => bail!("tools.searxng needs both username and password for basic auth, or neither"),
        };
        if !(1..=20).contains(&self.results) {
            bail!("tools.searxng.results must be between 1 and 20");
        }
        Ok(Searxng {
            url,
            auth,
            results: self.results,
        })
    }
}

/// `[tools.brave]`: the Brave Search API for the `web_search` tool.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BraveSection {
    api_key: Secret,
    results: usize,
}

impl BraveSection {
    fn validate(self) -> Result<Brave> {
        if self.api_key.as_str().trim().is_empty() {
            bail!("tools.brave.api_key is empty");
        }
        if !(1..=20).contains(&self.results) {
            bail!("tools.brave.results must be between 1 and 20");
        }
        Ok(Brave {
            api_key: self.api_key,
            results: self.results,
        })
    }
}

/// Validated `[tools.brave]`.
#[derive(Debug, Clone)]
pub struct Brave {
    pub api_key: Secret,
    /// How many results `web_search` returns.
    pub results: usize,
}

/// Where `web_search` searches: one provider, picked in the config.
#[derive(Debug, Clone)]
pub enum Search {
    Searxng(Searxng),
    Brave(Brave),
}

impl Search {
    /// How many results `web_search` returns.
    pub fn results(&self) -> usize {
        match self {
            Self::Searxng(searxng) => searxng.results,
            Self::Brave(brave) => brave.results,
        }
    }
}

/// Validated `[tools.searxng]`.
#[derive(Debug, Clone)]
pub struct Searxng {
    /// Base URL without a trailing slash; `/search` is appended.
    pub url: String,
    /// Basic auth username and password.
    pub auth: Option<(String, Secret)>,
    /// How many results `web_search` returns.
    pub results: usize,
}

/// `[approval]`: who approves MCP calls and settings changes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalSection {
    mode: ApprovalMode,
    /// Reviewer model; required for `auto`.
    model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ApprovalMode {
    Ask,
    Auto,
}

/// How actions that need approval are approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    /// Ask the user every time.
    Ask,
    /// A reviewer model passes low-risk actions; anything else is put to the user.
    Auto { model: String },
}

/// `[mcp.servers.<name>]`: MCP servers whose tools the model can call.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSection {
    #[serde(default)]
    servers: BTreeMap<String, McpServer>,
}

/// One MCP server: a local program over stdio (`command`) or a remote one over HTTP (`url`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for `command`, often API tokens.
    #[serde(default)]
    pub env: BTreeMap<String, Secret>,
    pub url: Option<String>,
    /// Sent as `Authorization: Bearer <token>` to `url`.
    pub token: Option<Secret>,
    /// Ask the user before each call; turn off only for servers that can't change anything.
    pub approve: bool,
}

impl McpServer {
    fn validate(&self, name: &str) -> Result<()> {
        // Tool names sent to the model are `<server>__<tool>`, limited to these characters.
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("mcp.servers.{name}: names may use only letters, digits, _ and -");
        }
        match (&self.command, &self.url) {
            (Some(_), None) => {}
            (None, Some(url)) if url.starts_with("http://") || url.starts_with("https://") => {}
            (None, Some(_)) => bail!("mcp.servers.{name}.url must start with http:// or https://"),
            _ => bail!("mcp.servers.{name} needs exactly one of `command` or `url`"),
        }
        if self.command.is_some() && self.token.is_some() {
            bail!(
                "mcp.servers.{name}.token is for `url` servers; pass tokens to `command` in `env`"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogSection {
    level: String,
}

/// `[voice]`: voice calls in Discord (`/voice`) through OpenAI's Realtime API.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSection {
    /// OpenAI API key.
    pub api_key: Secret,
    /// Realtime WebSocket endpoint; `?model=` is appended.
    pub url: String,
    pub model: String,
    /// One of `REALTIME_VOICES`, or any other the API offers.
    pub voice: String,
    /// The language calls are held in, as words the model reads ("Traditional Chinese", "日本語").
    pub language: String,
    /// Voice channels mokuzu joins on its own when someone allowed to talk to it joins them.
    pub channels: Vec<u64>,
}

impl VoiceSection {
    fn validate(&self) -> Result<()> {
        if self.api_key.as_str().trim().is_empty() {
            bail!("voice.api_key is empty");
        }
        if !self.url.starts_with("wss://") {
            bail!("voice.url must start with wss://");
        }
        if [&self.model, &self.voice, &self.language]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            bail!("voice.model, voice.voice and voice.language must not be empty");
        }
        Ok(())
    }
}

/// `[discord]`: lets `mokuzu serve` chat through a Discord bot, by DM and in any server channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordSection {
    pub token: Secret,
    /// Discord user IDs allowed to talk to the bot in DMs and in servers not listed in `guilds`;
    /// everyone else is ignored.
    pub allowed_users: Vec<u64>,
    /// `[discord.guilds.<server ID>]`: rules for one server, used there instead of `allowed_users`.
    #[serde(default)]
    pub guilds: BTreeMap<u64, GuildRule>,
}

impl DiscordSection {
    fn guild(&self, guild: Option<u64>) -> Option<&GuildRule> {
        guild.and_then(|guild| self.guilds.get(&guild))
    }

    /// Whether `user` may talk to the bot in `guild` (`None` for DMs).
    pub fn can_talk(&self, guild: Option<u64>, user: u64) -> bool {
        match self.guild(guild).map(|rule| &rule.allowed_users) {
            Some(Users::Everyone) => true,
            Some(Users::Only(ids)) => ids.contains(&user),
            None => self.allowed_users.contains(&user),
        }
    }

    /// Whether `user` may approve actions in `guild`. In a server open to everyone, only the
    /// main `allowed_users` may.
    pub fn can_approve(&self, guild: Option<u64>, user: u64) -> bool {
        match self.guild(guild).map(|rule| &rule.allowed_users) {
            Some(Users::Only(ids)) => ids.contains(&user),
            Some(Users::Everyone) | None => self.allowed_users.contains(&user),
        }
    }

    /// Whether the bot answers only @mentions in `guild`.
    pub fn mention_only(&self, guild: Option<u64>) -> bool {
        self.guild(guild).is_some_and(|rule| rule.mention_only)
    }
}

/// One server's rules for the Discord bot.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuildRule {
    /// Who may talk to the bot in this server: user IDs, or `"*"` for everyone.
    pub allowed_users: Users,
    /// Answer only messages that @mention the bot, in its threads too.
    pub mention_only: bool,
}

/// A server's `allowed_users`: a non-empty list of user IDs, or `"*"` for everyone.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawUsers")]
pub enum Users {
    Everyone,
    Only(Vec<u64>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawUsers {
    Ids(Vec<u64>),
    Pattern(String),
}

impl TryFrom<RawUsers> for Users {
    type Error = String;

    fn try_from(raw: RawUsers) -> std::result::Result<Self, String> {
        match raw {
            RawUsers::Pattern(text) if text == "*" => Ok(Self::Everyone),
            RawUsers::Pattern(text) => Err(format!(
                "allowed_users is a list of user IDs or \"*\", not {text:?}"
            )),
            RawUsers::Ids(ids) if ids.is_empty() => {
                Err("allowed_users is empty; list user IDs or use \"*\"".to_owned())
            }
            RawUsers::Ids(ids) => Ok(Self::Only(ids)),
        }
    }
}

/// Validated settings with every default filled in.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub max_tokens: u32,
    /// Estimated history size (tokens) at which older turns are summarized.
    pub compact_at_tokens: usize,
    pub provider: Provider,
    pub api_key: Secret,
    pub api: Api,
    /// The provider's base URL, e.g. `https://opencode.ai/zen/go/v1`.
    pub base_url: String,
    /// Every provider section in the file, the one in use too, so `mokuzu configure` keeps each
    /// provider's own key.
    pub keys: Vec<(Provider, ProviderKey)>,
    /// Enables the `web_search` tool when set.
    pub search: Option<Search>,
    /// MCP servers by name.
    pub mcp: BTreeMap<String, McpServer>,
    pub approval: Approval,
    /// `tracing` filter directive, e.g. `warn` or `mokuzu=debug`.
    pub log_level: String,
    pub discord: Option<DiscordSection>,
    /// Enables `/voice` calls in Discord.
    pub voice: Option<VoiceSection>,
    /// SQLite file holding conversation history.
    pub database_path: PathBuf,
    /// The user's time zone, for message timestamps and scheduled jobs.
    pub timezone: chrono_tz::Tz,
    /// `(width, height)` of the virtual desktop browsers draw on when there is no real screen.
    pub browser_screen: (u32, u32),
    /// Port of the `/browser` live view.
    pub viewer_port: u16,
    /// Port the webhook server listens on; `None` turns webhooks off.
    pub webhook_port: Option<u16>,
    /// File this was loaded from; empty when parsed from text.
    pub path: PathBuf,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            bail!("no config at {}; run `mokuzu configure`", path.display());
        }
        let mut text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        if let Some(migrated) =
            migrate::migrate(&text).with_context(|| format!("invalid config {}", path.display()))?
        {
            write(path, &migrated)?;
            eprintln!(
                "updated {} to config version {VERSION}; the old file is kept as .bak",
                path.display()
            );
            text = migrated;
        }
        let config =
            Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            ..config
        })
    }

    /// Parses `text`, migrating it to `VERSION` first in memory.
    pub fn parse(text: &str) -> Result<Self> {
        let migrated = migrate::migrate(text)?;
        let file: File = toml::from_str(migrated.as_deref().unwrap_or(text))?;
        if file
            .discord
            .as_ref()
            .is_some_and(|d| d.allowed_users.is_empty())
        {
            bail!("discord.allowed_users is empty; list your Discord user ID");
        }
        let mut keys = Vec::new();
        for (provider, section) in [
            (Provider::OpencodeGo, file.opencode_go),
            (Provider::Anthropic, file.anthropic),
            (Provider::GithubCopilot, file.github_copilot),
            (Provider::Chatgpt, file.chatgpt),
        ] {
            let Some(section) = section else { continue };
            let api_key = match section.api_key {
                Some(key) => key,
                None if provider.uses_oauth() => Secret::default(),
                None => bail!("[{}] requires api_key", provider.section()),
            };
            let base_url = match section.base_url {
                Some(url) => url,
                None if provider.uses_oauth() => provider.default_base_url().to_owned(),
                None => bail!("[{}] requires base_url", provider.section()),
            };
            let key = ProviderKey {
                api_key,
                base_url: base_url.trim_end_matches('/').to_owned(),
            };
            keys.push((provider, key));
        }
        if keys.is_empty() {
            bail!(
                "add a model provider section: [opencode-go], [anthropic], [github-copilot], or [chatgpt]"
            );
        }
        for (provider, key) in &keys {
            if provider.uses_oauth()
                && (!key.api_key.as_str().is_empty() || key.base_url != provider.default_base_url())
            {
                bail!(
                    "[{}] uses OAuth and its built-in endpoint; omit api_key and base_url overrides",
                    provider.section()
                );
            }
        }
        let provider = file.model.provider;
        let ProviderKey { api_key, base_url } = keys
            .iter()
            .find(|(p, _)| *p == provider)
            .map(|(_, key)| key.clone())
            .with_context(|| {
                format!(
                    "model.provider is {0}, but there is no [{0}] section",
                    provider.section()
                )
            })?;

        let compact_at_tokens = file.model.compact_at_tokens;
        if compact_at_tokens < MIN_COMPACT_AT_TOKENS {
            bail!("model.compact_at_tokens must be at least {MIN_COMPACT_AT_TOKENS}");
        }
        if let Some(voice) = &file.voice {
            voice.validate()?;
        }
        for (name, server) in &file.mcp.servers {
            server.validate(name)?;
        }
        let browser_screen = (file.browser.width, file.browser.height);
        if browser_screen.0 < MIN_SCREEN.0 || browser_screen.1 < MIN_SCREEN.1 {
            bail!(
                "browser.width and browser.height must be at least {}x{}",
                MIN_SCREEN.0,
                MIN_SCREEN.1
            );
        }
        if file.browser.viewer_port == 0 {
            bail!("browser.viewer_port must be between 1 and 65535");
        }
        let webhook_port = file.webhook.map(|w| w.port);
        if webhook_port == Some(0) {
            bail!("webhook.port must be between 1 and 65535");
        }
        let timezone = file.timezone.parse().map_err(|_| {
            anyhow::anyhow!(
                "timezone {:?} is not an IANA time zone like Asia/Taipei",
                file.timezone
            )
        })?;
        let model = file.model.name;
        let api = file.model.api;
        if provider == Provider::Anthropic && api != Api::Anthropic {
            bail!("[anthropic] only speaks model.api = \"anthropic\"");
        }
        if provider == Provider::Chatgpt && api != Api::Responses {
            bail!("[chatgpt] only speaks model.api = \"responses\"");
        }
        if provider == Provider::GithubCopilot && !matches!(api, Api::Openai | Api::Responses) {
            bail!("[github-copilot] requires model.api = \"openai\" or \"responses\"");
        }
        let approval = match (file.approval.mode, file.approval.model) {
            (ApprovalMode::Ask, None) => Approval::Ask,
            (ApprovalMode::Ask, Some(_)) => bail!("approval.model is only for mode = \"auto\""),
            (ApprovalMode::Auto, Some(model)) => Approval::Auto { model },
            (ApprovalMode::Auto, None) => bail!("approval.mode = \"auto\" needs approval.model"),
        };
        Ok(Self {
            approval,
            api,
            model,
            max_tokens: file.model.max_tokens,
            compact_at_tokens,
            provider,
            api_key,
            base_url,
            keys,
            search: match (file.tools.searxng, file.tools.brave) {
                (Some(_), Some(_)) => {
                    bail!("set only one of [tools.searxng] and [tools.brave] for web search")
                }
                (Some(searxng), None) => Some(Search::Searxng(searxng.validate()?)),
                (None, Some(brave)) => Some(Search::Brave(brave.validate()?)),
                (None, None) => None,
            },
            mcp: file.mcp.servers,
            log_level: file.log.level,
            discord: file.discord,
            voice: file.voice,
            database_path: file.database.path,
            timezone,
            browser_screen,
            viewer_port: file.browser.viewer_port,
            webhook_port,
            path: PathBuf::new(),
        })
    }
}

/// This machine's time zone (`TZ`, else the system setting), or UTC if it can't be read.
pub fn system_timezone() -> chrono_tz::Tz {
    match iana_time_zone::get_timezone().map(|name| name.parse()) {
        Ok(Ok(zone)) => zone,
        _ => {
            tracing::warn!("cannot read this machine's time zone; using UTC");
            chrono_tz::Tz::UTC
        }
    }
}

/// Writes the config readable only by the user, keeping the previous file as `.bak`.
pub fn write(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    if path.exists() {
        let backup = path.with_extension("toml.bak");
        std::fs::copy(path, &backup)
            .with_context(|| format!("failed to back up to {}", backup.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    // `mode` only applies on creation; tighten an existing file too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_sections_need_no_api_key_and_use_their_own_wire_formats() {
        for provider in [Provider::GithubCopilot, Provider::Chatgpt] {
            let config =
                Config::parse(&format!("[{}]\n", provider.section())).expect("OAuth config");
            assert_eq!(config.provider, provider);
            assert_eq!(config.api, provider.api_for(&config.model));
            assert_eq!(config.base_url, provider.default_base_url());
            assert!(config.api_key.as_str().is_empty());
        }
        let copilot =
            Config::parse("[github-copilot]\n[model]\nname = \"claude-sonnet-5.5\"\n").unwrap();
        assert_eq!(
            copilot.api,
            Api::Openai,
            "Copilot serves Claude as chat completions"
        );
        let codex = Config::parse("[github-copilot]\n[model]\nname = \"gpt-5.3-codex\"\n").unwrap();
        assert_eq!(codex.api, Api::Responses);
        let luna = Config::parse("[github-copilot]\n[model]\nname = \"gpt-6-luna\"\n").unwrap();
        assert_eq!(luna.api, Api::Responses);
        let custom = Config::parse(
            "[github-copilot]\n[model]\nname = \"custom-model\"\napi = \"responses\"\n",
        )
        .unwrap();
        assert_eq!(custom.api, Api::Responses);
    }

    #[test]
    fn oauth_rejects_api_keys_endpoint_overrides_and_incompatible_wire_formats() {
        for section in ["github-copilot", "chatgpt"] {
            assert!(
                Config::parse(&format!("[{section}]\napi_key = \"not-an-oauth-login\"\n")).is_err()
            );
            assert!(
                Config::parse(&format!(
                    "[{section}]\nbase_url = \"https://other.example\"\n"
                ))
                .is_err()
            );
            assert!(
                Config::parse(&format!("[{section}]\n[model]\napi = \"anthropic\"\n")).is_err()
            );
        }
        let config = Config::parse(
            "[model]\nprovider = \"chatgpt\"\n[chatgpt]\n[anthropic]\napi_key = \"keep-me\"\n",
        )
        .unwrap();
        assert_eq!(config.provider, Provider::Chatgpt);
        assert_eq!(
            config
                .keys
                .iter()
                .find(|(p, _)| *p == Provider::Anthropic)
                .unwrap()
                .1
                .api_key
                .as_str(),
            "keep-me"
        );
    }

    #[test]
    fn example_config_parses() {
        let example = include_str!("../../../config.example.toml");
        assert!(
            migrate::migrate(example).expect("valid").is_none(),
            "the example is at the current version"
        );
        let config = Config::parse(example).expect("valid");
        assert_eq!(config.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(config.api, Api::Anthropic);
    }

    #[test]
    fn anthropic_replaces_opencode_go() {
        let config = Config::parse("[anthropic]\napi_key = \"sk-ant\"\n").expect("valid");
        assert_eq!(config.provider, Provider::Anthropic);
        assert_eq!(config.base_url, ANTHROPIC_BASE_URL);
        assert_eq!(config.model, "claude-opus-5-5");
        assert_eq!(config.api, Api::Anthropic);
        assert_eq!(config.api_key.as_str(), "sk-ant");
        let both = "[anthropic]\napi_key = \"a\"\n[opencode-go]\napi_key = \"o\"\n";
        assert!(Config::parse(both).is_err(), "both need model.provider");
        let picked =
            Config::parse(&format!("[model]\nprovider = \"anthropic\"\n{both}")).expect("valid");
        assert_eq!(picked.provider, Provider::Anthropic);
        assert_eq!(picked.api_key.as_str(), "a");
        assert_eq!(picked.keys.len(), 2, "the unused key is kept");
        assert!(Config::parse("[log]\nlevel = \"warn\"\n").is_err());
        let wrong_api = "[model]\napi = \"openai\"\n[anthropic]\napi_key = \"a\"\n";
        assert!(Config::parse(wrong_api).is_err());
    }

    #[test]
    fn timezone_parses_and_bad_ones_are_refused() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!("timezone = \"Asia/Taipei\"\n{base}")).expect("valid");
        assert_eq!(config.timezone, chrono_tz::Tz::Asia__Taipei);
        assert!(Config::parse(&format!("timezone = \"Taipei\"\n{base}")).is_err());
    }

    #[test]
    fn mcp_servers_parse_and_bad_ones_are_refused() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!(
            "{base}[mcp.servers.fs]\ncommand = \"npx\"\nargs = [\"-y\", \"x\"]\n\
             [mcp.servers.remote]\nurl = \"https://e.com/mcp\"\ntoken = \"t\"\napprove = false\n"
        ))
        .expect("valid");
        assert!(config.mcp["fs"].approve);
        assert!(!config.mcp["remote"].approve);
        for bad in [
            "[mcp.servers.fs]\n",
            "[mcp.servers.fs]\ncommand = \"x\"\nurl = \"https://e.com\"\n",
            "[mcp.servers.fs]\nurl = \"ftp://e.com\"\n",
            "[mcp.servers.\"a b\"]\ncommand = \"x\"\n",
        ] {
            assert!(Config::parse(&format!("{base}{bad}")).is_err(), "{bad}");
        }
    }

    #[test]
    fn approval_defaults_to_ask_and_auto_reviewer_defaults_to_main_model() {
        let base = "[opencode-go]\napi_key = \"k\"\n[model]\nname = \"glm-5.3\"\n";
        assert_eq!(Config::parse(base).expect("valid").approval, Approval::Ask);
        let auto = Config::parse(&format!("{base}[approval]\nmode = \"auto\"\n")).expect("valid");
        assert_eq!(
            auto.approval,
            Approval::Auto {
                model: "glm-5.3".to_owned()
            }
        );
        let picked = format!("{base}[approval]\nmode = \"auto\"\nmodel = \"kimi-k3\"\n");
        assert_eq!(
            Config::parse(&picked).expect("valid").approval,
            Approval::Auto {
                model: "kimi-k3".to_owned()
            }
        );
    }

    #[test]
    fn browser_section_sets_screen_and_port() {
        let key = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(key).expect("valid");
        assert_eq!(
            (config.browser_screen, config.viewer_port),
            (DEFAULT_SCREEN, DEFAULT_VIEWER_PORT)
        );
        let set = format!("{key}[browser]\nwidth = 1920\nheight = 1080\nviewer_port = 8765\n");
        let config = Config::parse(&set).expect("valid");
        assert_eq!(
            (config.browser_screen, config.viewer_port),
            ((1920, 1080), 8765)
        );
        assert!(Config::parse(&format!("{key}[browser]\nwidth = 100\n")).is_err());
        assert!(Config::parse(&format!("{key}[browser]\nviewer_port = 0\n")).is_err());
    }

    #[test]
    fn webhook_section_sets_port() {
        let key = "[opencode-go]\napi_key = \"k\"\n";
        assert_eq!(Config::parse(key).expect("valid").webhook_port, None);
        let set = format!("{key}[webhook]\nport = 22051\n");
        assert_eq!(
            Config::parse(&set).expect("valid").webhook_port,
            Some(22051)
        );
        assert!(Config::parse(&format!("{key}[webhook]\nport = 0\n")).is_err());
        assert!(Config::parse(&format!("{key}[webhook]\n")).is_err());
    }

    #[test]
    fn old_tool_sections_still_load() {
        let text = "[opencode-go]\napi_key = \"k\"\n\n[tools.bash]\ntimeout_secs = 60\n\n\
                    [tools.claude_code]\ndirs = [\"~/repos\"]\n";
        assert!(Config::parse(text).is_ok());
    }

    #[test]
    fn minimal_config_fills_defaults_and_redacts_key() {
        let config = Config::parse(
            r#"
            [opencode-go]
            api_key = "sk-secret"
            base_url = "http://localhost:8080/v1/"
            "#,
        )
        .expect("valid");
        assert_eq!(config.model, "minimax-m3");
        assert_eq!(config.max_tokens, 16_000);
        assert_eq!(config.compact_at_tokens, 200_000);
        assert_eq!(config.base_url, "http://localhost:8080/v1");
        assert_eq!(Api::for_model("glm-5.3"), Api::Openai);
        assert_eq!(Api::for_model("claude-haiku-5-5"), Api::Anthropic);
        assert_eq!(Api::for_model("gpt-6-luna"), Api::Responses);
        assert_eq!(Api::for_model("gemini-4-argon-contributor"), Api::Gemini);
        assert_eq!(config.log_level, "warn");
        assert!(config.discord.is_none());
        assert!(
            config
                .database_path
                .ends_with(".local/share/mokuzu/mokuzu.db")
        );
        assert!(!format!("{config:?}").contains("sk-secret"));
    }

    #[test]
    fn searxng_section_validates_auth_pairs() {
        let base = "[opencode-go]\napi_key = \"k\"\n[tools.searxng]\n";
        let config = Config::parse(&format!(
            "{base}url = \"https://search.example.com/\"\nusername = \"me\"\npassword = \"pw\""
        ))
        .expect("valid");
        let Some(Search::Searxng(searxng)) = config.search else {
            panic!("searxng configured");
        };
        assert_eq!(searxng.url, "https://search.example.com");
        assert_eq!(searxng.results, 5);
        assert_eq!(searxng.auth.map(|(user, _)| user).as_deref(), Some("me"));
        assert!(Config::parse(&format!("{base}url = \"http://x\"\npassword = \"pw\"")).is_err());
        assert!(
            !format!(
                "{:?}",
                Config::parse(&format!(
                    "{base}url = \"http://x\"\nusername = \"me\"\npassword = \"pw\""
                ))
                .expect("valid")
            )
            .contains("\"pw\"")
        );
        assert!(Config::parse(&format!("{base}url = \"http://x\"\nusername = \"me\"")).is_err());
        assert!(Config::parse(&format!("{base}url = \"x\"")).is_err());
        assert!(Config::parse(&format!("{base}url = \"http://x\"\nresults = 0")).is_err());
    }

    #[test]
    fn brave_section_parses_and_excludes_searxng() {
        let base = "[opencode-go]\napi_key = \"k\"\n";
        let config = Config::parse(&format!(
            "{base}[tools.brave]\napi_key = \"bk\"\nresults = 8\n"
        ))
        .expect("valid");
        let Some(Search::Brave(brave)) = &config.search else {
            panic!("brave configured");
        };
        assert_eq!((brave.api_key.as_str(), brave.results), ("bk", 8));
        assert!(!format!("{config:?}").contains("\"bk\""));
        assert!(Config::parse(&format!("{base}[tools.brave]\napi_key = \" \"\n")).is_err());
        assert!(
            Config::parse(&format!(
                "{base}[tools.brave]\napi_key = \"bk\"\n[tools.searxng]\nurl = \"http://x\"\n"
            ))
            .is_err(),
            "only one search provider"
        );
    }

    #[test]
    fn voice_section_needs_a_websocket_url_and_a_key() {
        let base = "[opencode-go]\napi_key = \"k\"\n[voice]\nmodel = \"m\"\nvoice = \"marin\"\n\
                    language = \"English\"\nchannels = [7]\n";
        let config = Config::parse(&format!(
            "{base}api_key = \"sk\"\nurl = \"wss://x/v1/realtime\"\n"
        ))
        .expect("valid");
        let voice = config.voice.expect("voice");
        assert_eq!(
            (voice.model.as_str(), voice.channels.as_slice()),
            ("m", &[7][..])
        );
        assert!(Config::parse(&format!("{base}api_key = \"sk\"\nurl = \"https://x\"\n")).is_err());
        assert!(Config::parse(&format!("{base}api_key = \" \"\nurl = \"wss://x\"\n")).is_err());
        assert!(
            Config::parse(&format!("{base}api_key = \"sk\"\n")).is_err(),
            "url is required"
        );
    }

    #[test]
    fn current_version_files_must_spell_out_every_key() {
        let old = "[opencode-go]\napi_key = \"k\"\n";
        let full = migrate::migrate(old).expect("valid").expect("migrated");
        Config::parse(&full).expect("valid");
        for key in [
            "max_tokens = ",
            "base_url = ",
            "timezone = ",
            "viewer_port = ",
        ] {
            let line = full
                .lines()
                .find(|line| line.starts_with(key))
                .expect("written by the migration");
            let missing = full.replace(&format!("{line}\n"), "");
            assert!(Config::parse(&missing).is_err(), "{key} has no default");
        }
    }

    #[test]
    fn rejects_missing_key_unknown_fields_and_open_bot() {
        assert!(Config::parse("[model]\nname = \"x\"").is_err());
        let typo = "[model]\nnmae = \"x\"\n[opencode-go]\napi_key = \"k\"";
        assert!(Config::parse(typo).is_err());
        let missing = "[model]\nprovider = \"anthropic\"\n[opencode-go]\napi_key = \"k\"";
        assert!(Config::parse(missing).is_err(), "no [anthropic] section");
        let open_bot =
            "[opencode-go]\napi_key = \"k\"\n[discord]\ntoken = \"t\"\nallowed_users = []";
        assert!(Config::parse(open_bot).is_err());
    }

    #[test]
    fn discord_guilds_override_allowed_users() {
        let base =
            "[opencode-go]\napi_key = \"k\"\n[discord]\ntoken = \"t\"\nallowed_users = [1]\n";
        let config = Config::parse(&format!(
            "{base}[discord.guilds.100]\nallowed_users = [2, 3]\nmention_only = true\n\
             [discord.guilds.200]\nallowed_users = [1]\n"
        ))
        .expect("valid");
        let discord = config.discord.expect("discord configured");
        assert!(discord.can_talk(None, 1) && !discord.can_talk(None, 2));
        assert!(discord.can_talk(Some(100), 2) && !discord.can_talk(Some(100), 1));
        assert!(discord.can_approve(Some(100), 2) && !discord.can_approve(Some(100), 1));
        assert!(discord.mention_only(Some(100)) && !discord.mention_only(Some(200)));
        assert!(discord.can_talk(Some(300), 1) && !discord.mention_only(None));

        let open = Config::parse(&format!(
            "{base}[discord.guilds.100]\nallowed_users = \"*\"\n"
        ))
        .expect("valid")
        .discord
        .expect("discord configured");
        assert!(open.can_talk(Some(100), 99), "everyone may talk");
        assert!(
            !open.can_approve(Some(100), 99),
            "only the main list approves"
        );
        assert!(open.can_approve(Some(100), 1));
        assert!(!open.can_talk(None, 99), "DMs keep the main list");
        assert!(
            Config::parse(&format!(
                "{base}[discord.guilds.100]\nallowed_users = \"all\"\n"
            ))
            .is_err()
        );
        assert!(
            Config::parse(&format!("{base}[discord.guilds.100]\nallowed_users = []\n")).is_err()
        );
        assert!(
            Config::parse(&format!("{base}[discord.guilds.x]\nallowed_users = [1]\n")).is_err()
        );
        assert!(
            Config::parse(&format!(
                "{base}[discord.guilds.100]\nallowed_users = [1]\nmentoin = true\n"
            ))
            .is_err()
        );
    }
}
