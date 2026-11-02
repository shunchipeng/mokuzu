//! Discord gateway: one agent per DM or thread. A message in a server text channel opens a new
//! thread for its conversation. Memory is shared by every conversation. Only allow-listed
//! users are heard, per server if `[discord.guilds]` says so, and they approve settings changes
//! with buttons. With `[voice]` set, `/voice` or joining one of its `channels` starts a voice call
//! (see `voice`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rig_core::message::{Image, ImageMediaType, MimeType as _};
use serenity::all::{
    Attachment, AutoArchiveDuration, ButtonStyle, ChannelId, ChannelType, Client, Command,
    CommandInteraction, CommandOptionType, ComponentInteraction, Context, CreateActionRow,
    CreateAllowedMentions, CreateAttachment, CreateButton, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateThread,
    EditMessage, EditThread, EventHandler, GatewayIntents, GuildId, Http, Interaction, Message,
    MessageId, Ready, UserId, VoiceState,
};
use serenity::async_trait;
use songbird::serenity::SerenityInit as _;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::agent::{self, Agent, Io, Stop, Stopped};
use crate::config::{Config, DiscordSection, VoiceSection};
use crate::cron;
use crate::db::{Db, Hook, Job, Todo};
use crate::document;
use crate::mcp::Mcp;
use crate::todo::{self, Quiet};
use crate::voice;
use crate::webhook;

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a turn waits for another message before starting, so a burst is answered once.
const MERGE_QUIET: Duration = Duration::from_millis(500);
/// Longest a turn waits to merge, however fast messages keep coming.
const MERGE_CAP: Duration = Duration::from_secs(1);
/// Discord rejects messages over 2000 characters.
const MAX_MESSAGE_CHARS: usize = 1_900;
/// Reaction added to each message the bot picks up, swapped for `DONE` or `FAILED` once handled.
const RECEIVED: char = '👀';
const DONE: char = '✅';
const FAILED: char = '❌';
/// Reaction to a `/stop` with no turn running.
const NOTHING_TO_STOP: char = '💤';
/// Stops the running turn of the conversation it is sent in.
const STOP_COMMAND: &str = "/stop";
const RUN: &str = "mokuzu:run";
const DENY: &str = "mokuzu:deny";
/// Longest status line kept; statuses are one-line summaries.
const MAX_STATUS_CHARS: usize = 200;
/// How often the scheduler looks for due jobs.
const CRON_TICK: Duration = Duration::from_secs(30);
/// Discord caps thread names at 100 characters.
const MAX_THREAD_NAME_CHARS: usize = 80;
/// Anthropic rejects images over 5 MB; larger attachments are left out.
const MAX_IMAGE_BYTES: u32 = 5 * 1024 * 1024;
/// Largest text attachment read, like the `message.txt` Discord makes from a long paste.
const MAX_TEXT_BYTES: u32 = 200 * 1024;
/// Characters of a document attachment's text put in the prompt.
const MAX_DOCUMENT_CHARS: usize = 50_000;

/// What a channel's agent task receives.
#[derive(Debug)]
enum Input {
    /// A user's message, and where it lives (a thread's first message sits in the parent channel)
    /// so its reaction can be updated once handled.
    Text {
        content: String,
        /// Image and text attachments, downloaded when the turn starts.
        attachments: Vec<Attachment>,
        origin: ChannelId,
        message: MessageId,
        /// Discord user who sent it; their memory is the one the turn uses.
        author: u64,
        /// Whether a typed `y` from them approves a waiting action.
        can_approve: bool,
        /// Set for a voice call's request: gets what the turn said, so the call can read it out.
        reply: Option<oneshot::Sender<String>>,
    },
    /// A Run or Deny click on the approval message `message`.
    Decision { message: MessageId, run: bool },
    /// A server message the bot skips, added to the history as context only. `who` wrote `text`.
    Overheard { who: String, text: String },
}

/// Connects to Discord and serves DMs until the connection fails.
pub async fn serve(config: Config) -> Result<()> {
    let Some(discord) = config.discord.clone() else {
        bail!("`mokuzu serve` needs a [discord] section in the config");
    };
    let db = Db::open(&config.database_path)?;
    // MESSAGE_CONTENT is privileged: enable it for the bot in the Developer Portal, or connecting fails.
    let mut intents = GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    if config.voice.is_some() {
        intents |= GatewayIntents::GUILD_VOICE_STATES;
    }
    let display = crate::browser::desktop(config.browser_screen).await;
    let mcp = Arc::new(Mcp::connect(&config.mcp, display.as_deref()).await);
    let token = discord.token.as_str().to_owned();
    let voice = config
        .voice
        .clone()
        .map(|section| (voice::songbird(), section));
    let handler = Handler {
        db: db.clone(),
        discord,
        bot: OnceLock::new(),
        inboxes: Arc::new(Inboxes {
            mcp: Arc::clone(&mcp),
            config: config.clone(),
            db: db.clone(),
            channels: Mutex::new(HashMap::new()),
        }),
        voice: voice.clone(),
        calls: Arc::new(Mutex::new(HashMap::new())),
    };
    let mut builder = Client::builder(&token, intents).event_handler(handler);
    if let Some((songbird, _)) = voice {
        builder = builder.register_songbird_with(songbird);
    }
    let mut client = builder.await.context("failed to build Discord client")?;
    if let Some(port) = config.webhook_port {
        let events = webhook::listen(port, db.clone()).await?;
        tokio::spawn(run_hooks(
            config.clone(),
            db.clone(),
            Arc::clone(&mcp),
            Arc::clone(&client.http),
            events,
        ));
    }
    tokio::spawn(run_scheduler(config, db, mcp, Arc::clone(&client.http)));
    client.start().await.context("Discord connection failed")
}

/// Jobs (`job:<id>`) and heartbeats (`heartbeat:<target>:<user>`) with a run in progress; one
/// that comes due again while running is skipped.
static RUNNING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Marks `key` running until dropped; `None` if it already is.
struct Running(String);

impl Running {
    fn start(key: String) -> Option<Self> {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        running
            .get_or_insert_default()
            .insert(key.clone())
            .then(|| Self(key))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(running) = running.as_mut() {
            running.remove(&self.0);
        }
    }
}

/// Starts every due job, and every `todo::HEARTBEAT` a check of open todos, forever. A job missed
/// while mokuzu was down runs once on startup; the first heartbeat waits a full interval.
async fn run_scheduler(config: Config, db: Db, mcp: Arc<Mcp>, http: Arc<Http>) {
    let mut tick = tokio::time::interval(CRON_TICK);
    let mut heartbeat = tokio::time::interval_at(
        tokio::time::Instant::now() + todo::HEARTBEAT,
        todo::HEARTBEAT,
    );
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(err) = start_due_jobs(&config, &db, &mcp, &http).await {
                    tracing::warn!("scheduler tick failed: {err:#}");
                }
            }
            _ = heartbeat.tick() => {
                if let Err(err) = start_heartbeats(&config, &db, &mcp, &http).await {
                    tracing::warn!("heartbeat failed: {err:#}");
                }
            }
        }
    }
}

/// Starts one heartbeat per person and Discord conversation with open todos. Nobody with no
/// todos means no model call.
async fn start_heartbeats(
    config: &Config,
    db: &Db,
    mcp: &Arc<Mcp>,
    http: &Arc<Http>,
) -> Result<()> {
    let mut groups: HashMap<(u64, String), Vec<Todo>> = HashMap::new();
    for todo in db.all_todos().await? {
        groups
            .entry((todo.user, todo.target.clone()))
            .or_default()
            .push(todo);
    }
    for ((user, target), todos) in groups {
        let key = format!("heartbeat:{target}:{user}");
        let Some(running) = Running::start(key.clone()) else {
            tracing::info!(key, "skipping a heartbeat: the last one has not finished");
            continue;
        };
        let (config, db, mcp, http) = (
            config.clone(),
            db.clone(),
            Arc::clone(mcp),
            Arc::clone(http),
        );
        tokio::spawn(async move {
            run_heartbeat(config, db, mcp, http, key, user, target, todos).await;
            drop(running);
        });
    }
    Ok(())
}

/// Works through `user`'s `todos` written in the conversation `target`, in a fresh conversation
/// `key`, and posts there only if the agent has something to say.
#[tracing::instrument(skip_all, fields(key))]
#[allow(clippy::too_many_arguments)]
async fn run_heartbeat(
    config: Config,
    db: Db,
    mcp: Arc<Mcp>,
    http: Arc<Http>,
    key: String,
    user: u64,
    target: String,
    todos: Vec<Todo>,
) {
    let Some(mut io) = unattended_io(http, &target) else {
        return;
    };
    let now = chrono::Utc::now().with_timezone(&config.timezone);
    let mut quiet = Quiet::new(&mut io);
    let run = async {
        let mut agent = Agent::new(config, db, mcp, &key).await?;
        agent.reset().await?;
        agent.posts_to(&target);
        agent
            .run_turn(
                &todo::heartbeat_prompt(&todos, now),
                Vec::new(),
                user,
                &mut quiet,
            )
            .await?;
        quiet.finish().await
    };
    match run.await {
        Ok(true) => {}
        Ok(false) => tracing::debug!("heartbeat had nothing to say"),
        Err(err) => {
            tracing::warn!("heartbeat failed: {err:#}");
            if let Err(err) = io.say(&format!("error: todo check failed: {err:#}")).await {
                tracing::error!("failed to report heartbeat failure: {err:#}");
            }
        }
    }
}

/// An `Io` posting to the Discord conversation `target` with nobody there to approve anything:
/// with the inbox's sender gone, `confirm` sees it closed and denies. Auto approval still
/// reviews and runs.
fn unattended_io(http: Arc<Http>, target: &str) -> Option<DiscordIo> {
    let Some(channel) = target
        .strip_prefix("discord:")
        .and_then(|id| id.parse().ok())
        .map(ChannelId::new)
    else {
        tracing::warn!(target, "not a Discord channel");
        return None;
    };
    let (_, inbox) = mpsc::unbounded_channel();
    Some(DiscordIo {
        http,
        web: reqwest::Client::new(),
        channel,
        inbox,
        approval: None,
        status: None,
        board: None,
        overheard: Vec::new(),
        reply: None,
    })
}

/// Moves each due job to its next run (or deletes a one-off) before starting it, so a slow run
/// can't start twice. A job whose last run is still going skips this one.
// ponytail: scans every job each tick; query by next_run if job counts grow large.
async fn start_due_jobs(config: &Config, db: &Db, mcp: &Arc<Mcp>, http: &Arc<Http>) -> Result<()> {
    let now = chrono::Utc::now().with_timezone(&config.timezone);
    for job in db.jobs().await? {
        if job.next_run > now.timestamp() {
            break; // Sorted soonest first.
        }
        match job.schedule.as_deref().map(|s| cron::next_run(s, now)) {
            Some(Ok(next)) => db.set_next_run(job.id, next).await?,
            Some(Err(err)) => {
                tracing::warn!(job = job.id, "dropping job: {err}");
                db.remove_job(job.id).await?;
                continue;
            }
            None => {
                db.remove_job(job.id).await?;
            }
        }
        let Some(running) = Running::start(format!("job:{}", job.id)) else {
            tracing::info!(
                job = job.id,
                "skipping a run: the last one has not finished"
            );
            continue;
        };
        let (config, db, mcp, http) = (
            config.clone(),
            db.clone(),
            Arc::clone(mcp),
            Arc::clone(http),
        );
        tokio::spawn(async move {
            run_job(config, db, mcp, http, job).await;
            drop(running);
        });
    }
    Ok(())
}

/// Runs one job in its own fresh conversation and posts the result where the job was made.
#[tracing::instrument(skip_all, fields(job = job.id))]
async fn run_job(config: Config, db: Db, mcp: Arc<Mcp>, http: Arc<Http>, job: Job) {
    let Some(mut io) = unattended_io(http, &job.target) else {
        return;
    };
    let run = async {
        io.note(&format!("⏰ job #{} {}", job.id, job.name)).await?;
        let mut agent = Agent::new(config, db, mcp, &format!("cron:{}", job.id)).await?;
        // The last run's transcript stays on disk until the next one starts.
        agent.reset().await?;
        agent.posts_to(&job.target);
        agent
            .run_turn(&cron::prompt(&job), Vec::new(), job.user, &mut io)
            .await
    };
    if let Err(err) = run.await {
        tracing::warn!("job failed: {err:#}");
        if let Err(err) = io
            .say(&format!(
                "error: job #{} {} failed: {err:#}",
                job.id, job.name
            ))
            .await
        {
            tracing::error!("failed to report job failure: {err:#}");
        }
    }
}

/// Runs each webhook request as it comes; a webhook's requests run one after another.
async fn run_hooks(
    config: Config,
    db: Db,
    mcp: Arc<Mcp>,
    http: Arc<Http>,
    mut events: mpsc::UnboundedReceiver<(Hook, String)>,
) {
    // One run at a time per webhook, so its runs never share a conversation.
    // ponytail: one lock per webhook id ever woken, never freed; prune if webhooks churn a lot.
    let mut locks: HashMap<i64, Arc<tokio::sync::Mutex<()>>> = HashMap::new();
    while let Some((hook, body)) = events.recv().await {
        let lock = Arc::clone(locks.entry(hook.id).or_default());
        let (config, db, mcp, http) = (
            config.clone(),
            db.clone(),
            Arc::clone(&mcp),
            Arc::clone(&http),
        );
        tokio::spawn(async move {
            let _turn = lock.lock().await;
            run_hook(config, db, mcp, http, hook, body).await;
        });
    }
}

/// Runs one webhook request in the webhook's own fresh conversation and posts the result where
/// the webhook was made.
#[tracing::instrument(skip_all, fields(hook = hook.id))]
async fn run_hook(
    config: Config,
    db: Db,
    mcp: Arc<Mcp>,
    http: Arc<Http>,
    hook: Hook,
    body: String,
) {
    let Some(mut io) = unattended_io(http, &hook.target) else {
        return;
    };
    let run = async {
        io.note(&format!("🪝 webhook #{} {}", hook.id, hook.name))
            .await?;
        let mut agent = Agent::new(config, db, mcp, &format!("webhook:{}", hook.id)).await?;
        agent.reset().await?;
        agent.posts_to(&hook.target);
        agent.ask_for_every_action();
        agent
            .run_turn(
                &webhook::prompt(&hook, &body),
                Vec::new(),
                hook.user,
                &mut io,
            )
            .await
    };
    if let Err(err) = run.await {
        tracing::warn!("webhook run failed: {err:#}");
        if let Err(err) = io
            .say(&format!(
                "error: webhook #{} {} failed: {err:#}",
                hook.id, hook.name
            ))
            .await
        {
            tracing::error!("failed to report webhook failure: {err:#}");
        }
    }
}

struct Handler {
    db: Db,
    /// Who the bot acts on, in DMs and in each server.
    discord: DiscordSection,
    /// The bot's own user, known once connected.
    bot: OnceLock<UserId>,
    inboxes: Arc<Inboxes>,
    /// The voice manager and `[voice]`, when calls are on.
    voice: Option<(Arc<songbird::Songbird>, VoiceSection)>,
    /// Each server's call in progress, ended by notifying it.
    calls: Arc<Mutex<HashMap<GuildId, Arc<Notify>>>>,
}

/// The agent task of each channel's conversation, shared so a voice call can hand it requests.
struct Inboxes {
    /// MCP servers, shared by every channel's agent.
    mcp: Arc<Mcp>,
    config: Config,
    db: Db,
    /// Inbox of the task that owns each channel's conversation, and what stops its turn.
    channels: Mutex<HashMap<ChannelId, ChannelTask>>,
}

/// A channel's agent task: its inbox, and what stops its running turn.
type ChannelTask = (mpsc::UnboundedSender<Input>, Arc<Stop>);

impl Inboxes {
    /// Hands `input` to the channel's agent task, starting one for text if none is running.
    fn deliver(&self, http: &Arc<Http>, channel: ChannelId, input: Input) {
        let Ok(mut channels) = self.channels.lock() else {
            tracing::error!("channel map lock poisoned");
            return;
        };
        if !channels.contains_key(&channel) && matches!(input, Input::Decision { .. }) {
            return; // A click on an old approval; nothing is waiting for it.
        }
        let (inbox, _) = channels.entry(channel).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let stop = Arc::new(Stop::default());
            tokio::spawn(run_channel(
                self.config.clone(),
                self.db.clone(),
                Arc::clone(&self.mcp),
                Arc::clone(http),
                channel,
                Arc::clone(&stop),
                rx,
            ));
            (tx, stop)
        });
        if inbox.send(input).is_err() {
            // The channel task died; drop it so the next message starts a fresh one.
            channels.remove(&channel);
        }
    }
}

impl Inboxes {
    /// Stops the turn running in `channel`; `false` if none is.
    fn stop(&self, channel: ChannelId) -> bool {
        lock(&self.channels)
            .get(&channel)
            .is_some_and(|(_, stop)| stop.stop())
    }
}

impl Handler {
    fn deliver(&self, http: &Arc<Http>, channel: ChannelId, input: Input) {
        self.inboxes.deliver(http, channel, input);
    }

    /// `/voice` joins the sender's voice channel for a call; `/hangup` ends the call. Requests
    /// the call hands to mokuzu run in `msg`'s channel.
    async fn voice_command(&self, ctx: &Context, msg: &Message, author: u64, content: &str) {
        let reply = match self.start_or_end_call(ctx, msg, author, content).await {
            Ok(reply) => reply,
            Err(err) => format!("error: {err:#}"),
        };
        if let Err(err) = msg.channel_id.say(&ctx.http, reply).await {
            tracing::warn!("failed to answer a voice command: {err:#}");
        }
    }

    async fn start_or_end_call(
        &self,
        ctx: &Context,
        msg: &Message,
        author: u64,
        content: &str,
    ) -> Result<String> {
        if self.voice.is_none() {
            return Ok("Voice calls need a [voice] section in the config.".to_owned());
        }
        let Some(guild) = msg.guild_id else {
            return Ok("Voice calls work in a server, not in DMs.".to_owned());
        };
        if content == voice::HANGUP_COMMAND {
            let stop = lock(&self.calls).remove(&guild);
            return Ok(match stop {
                Some(stop) => {
                    stop.notify_one();
                    "📴 Hanging up.".to_owned()
                }
                None => "No call in this server.".to_owned(),
            });
        }
        let state = guild
            .get_user_voice_state(&ctx.http, UserId::new(author))
            .await;
        let Some(channel) = state.ok().and_then(|state| state.channel_id) else {
            return Ok(format!(
                "Join a voice channel first, then send {}.",
                voice::VOICE_COMMAND
            ));
        };
        self.start_call(&ctx.http, guild, channel, author, msg.channel_id)
    }

    /// Joins `channel` for a call with `author`, unless the server has one going. The agent
    /// answers in `text_channel`.
    fn start_call(
        &self,
        http: &Arc<Http>,
        guild: GuildId,
        channel: ChannelId,
        author: u64,
        text_channel: ChannelId,
    ) -> Result<String> {
        let Some((songbird, section)) = &self.voice else {
            return Ok("Voice calls need a [voice] section in the config.".to_owned());
        };
        let stop = Arc::new(Notify::new());
        {
            let mut calls = lock(&self.calls);
            if calls.contains_key(&guild) {
                return Ok(format!(
                    "Already in a call here; send {} to end it.",
                    voice::HANGUP_COMMAND
                ));
            }
            calls.insert(guild, Arc::clone(&stop));
        }
        let ask = self.ask(http, text_channel, author, guild);
        let (songbird, section, http, calls, db) = (
            Arc::clone(songbird),
            section.clone(),
            Arc::clone(http),
            Arc::clone(&self.calls),
            self.db.clone(),
        );
        tokio::spawn(async move {
            // Loaded once per call, so the realtime model knows who it is talking to.
            let memory = match db.memories(author).await {
                Ok(entries) => entries.into_iter().map(|entry| entry.content).collect(),
                Err(err) => {
                    tracing::warn!("voice call goes without memory: {err:#}");
                    Vec::new()
                }
            };
            let caller = voice::Caller { id: author, memory };
            let ended = voice::call(
                songbird,
                section,
                guild,
                channel,
                caller,
                ask,
                Arc::clone(&stop),
            )
            .await;
            {
                let mut calls = lock(&calls);
                if calls.get(&guild).is_some_and(|s| Arc::ptr_eq(s, &stop)) {
                    calls.remove(&guild);
                }
            }
            let note = match ended {
                Ok(()) => "📴 Call ended.".to_owned(),
                Err(err) => {
                    tracing::warn!("voice call failed: {err:#}");
                    format!("📴 Call ended: {err:#}")
                }
            };
            if let Err(err) = text_channel.say(&http, note).await {
                tracing::warn!("failed to report the end of a call: {err:#}");
            }
        });
        Ok(format!(
            "📞 Joining <#{channel}>. Talk to me there; {} ends the call.",
            voice::HANGUP_COMMAND
        ))
    }

    /// `ask_mokuzu` for a call: posts the request in `channel` and runs it as `author`'s turn
    /// there, returning what the turn said.
    fn ask(&self, http: &Arc<Http>, channel: ChannelId, author: u64, guild: GuildId) -> voice::Ask {
        let (inboxes, http) = (Arc::clone(&self.inboxes), Arc::clone(http));
        let can_approve = self.discord.can_approve(Some(guild.get()), author);
        Arc::new(move |request: String| {
            let (inboxes, http) = (Arc::clone(&inboxes), Arc::clone(&http));
            Box::pin(async move {
                let posted = CreateMessage::new()
                    .content(format!("🎙️ {request}"))
                    .allowed_mentions(CreateAllowedMentions::new());
                let message = channel.send_message(&http, posted).await?.id;
                let (reply, said) = oneshot::channel();
                let input = Input::Text {
                    content: request,
                    attachments: Vec::new(),
                    origin: channel,
                    message,
                    author,
                    can_approve,
                    reply: Some(reply),
                };
                inboxes.deliver(&http, channel, input);
                said.await.context("mokuzu dropped the request")
            })
        })
    }

    /// Hands a skipped server message to its channel's history, if the bot has a conversation
    /// there, so a later turn knows what was said. Other channels are left alone.
    async fn overhear(&self, ctx: &Context, msg: Message) {
        let text = msg.content.trim();
        if msg.guild_id.is_none() || text.is_empty() {
            return;
        }
        match self
            .db
            .has_conversation(&format!("discord:{}", msg.channel_id))
            .await
        {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                tracing::warn!("failed to look up conversation: {err:#}");
                return;
            }
        }
        let input = Input::Overheard {
            who: msg.author.display_name().to_owned(),
            text: text.to_owned(),
        };
        self.deliver(&ctx.http, msg.channel_id, input);
    }

    /// `/stop` typed as text: stops the turn running in `msg`'s conversation.
    async fn stop(&self, ctx: &Context, msg: &Message) {
        let reaction = if self.inboxes.stop(msg.channel_id) {
            DONE
        } else {
            NOTHING_TO_STOP
        };
        if let Err(err) = msg.react(&ctx.http, reaction).await {
            tracing::warn!("failed to react to message: {err:#}");
        }
    }

    /// Starts a turn for `content`: acknowledges `msg`, opens a thread on it in a server text
    /// channel, and hands the prompt to that conversation.
    async fn accept(
        &self,
        ctx: &Context,
        msg: &Message,
        content: String,
        attachments: Vec<Attachment>,
        author: u64,
    ) {
        if [voice::VOICE_COMMAND, voice::HANGUP_COMMAND].contains(&content.trim()) {
            self.voice_command(ctx, msg, author, content.trim()).await;
            return;
        }
        // Acknowledge right away so the user knows the bot has it before the model answers.
        if let Err(err) = msg.react(&ctx.http, RECEIVED).await {
            tracing::warn!("failed to react to message: {err:#}");
        }
        let channel = match route(ctx, msg, &content, &self.inboxes.config).await {
            Ok(channel) => channel,
            Err(err) => {
                tracing::warn!("failed to open a thread, answering in place: {err:#}");
                msg.channel_id
            }
        };
        let input = Input::Text {
            content,
            attachments,
            origin: msg.channel_id,
            message: msg.id,
            author,
            can_approve: self
                .discord
                .can_approve(msg.guild_id.map(|g| g.get()), author),
            reply: None,
        };
        self.deliver(&ctx.http, channel, input);
    }

    /// A slash command picked from Discord's list: the bot answers it with the command as text,
    /// then treats that reply like the user's own message.
    async fn on_command(&self, ctx: &Context, command: &CommandInteraction) -> Result<()> {
        let author = command.user.id.get();
        if !self
            .discord
            .can_talk(command.guild_id.map(|g| g.get()), author)
        {
            let reply = CreateInteractionResponseMessage::new()
                .content("You are not allowed to talk to this bot.")
                .ephemeral(true);
            return Ok(command
                .create_response(&ctx.http, CreateInteractionResponse::Message(reply))
                .await?);
        }
        if command.data.name == STOP_COMMAND[1..] {
            let text = if self.inboxes.stop(command.channel_id) {
                "⏹️ stopping"
            } else {
                "Nothing is running here."
            };
            let reply = CreateInteractionResponseMessage::new()
                .content(text)
                .ephemeral(true);
            return Ok(command
                .create_response(&ctx.http, CreateInteractionResponse::Message(reply))
                .await?);
        }
        let mut content = format!("/{}", command.data.name);
        for option in &command.data.options {
            if let Some(text) = option.value.as_str() {
                content.push(' ');
                content.push_str(text);
            }
        }
        let reply = CreateInteractionResponseMessage::new()
            .content(&content)
            .allowed_mentions(CreateAllowedMentions::new());
        command
            .create_response(&ctx.http, CreateInteractionResponse::Message(reply))
            .await?;
        let msg = command.get_response(&ctx.http).await?;
        self.accept(ctx, &msg, content, Vec::new(), author).await;
        Ok(())
    }

    async fn on_click(&self, ctx: &Context, click: &ComponentInteraction) -> Result<()> {
        let run = match click.data.custom_id.as_str() {
            RUN => true,
            DENY => false,
            _ => return Ok(()),
        };
        if !self
            .discord
            .can_approve(click.guild_id.map(|g| g.get()), click.user.id.get())
        {
            let reply = CreateInteractionResponseMessage::new()
                .content("You are not allowed to approve changes.")
                .ephemeral(true);
            return Ok(click
                .create_response(&ctx.http, CreateInteractionResponse::Message(reply))
                .await?);
        }
        // Answer within Discord's 3-second window: drop the buttons and show the decision.
        let status = if run {
            "▶️ running…"
        } else {
            "✖️ denied"
        };
        let update = CreateInteractionResponseMessage::new()
            .content(format!("{}\n{status}", click.message.content))
            .components(Vec::new());
        click
            .create_response(&ctx.http, CreateInteractionResponse::UpdateMessage(update))
            .await?;
        self.deliver(
            &ctx.http,
            click.channel_id,
            Input::Decision {
                message: click.message.id,
                run,
            },
        );
        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        let guild = msg.guild_id.map(|g| g.get());
        let author = msg.author.id.get();
        if msg.author.bot {
            return;
        }
        let Some(&bot) = self.bot.get() else {
            return;
        };
        if !self.discord.can_talk(guild, author)
            || (self.discord.mention_only(guild) && !msg.mentions_user_id(bot))
        {
            self.overhear(&ctx, msg).await;
            return;
        }
        let content = strip_mention(&msg.content, bot);
        if content == STOP_COMMAND {
            self.stop(&ctx, &msg).await;
            return;
        }
        let attachments = msg
            .attachments
            .iter()
            .filter(|a| {
                image_type(a).is_some()
                    || is_text(a)
                    || is_document(a)
                    || (self.voice.is_some() && is_audio(a))
            })
            .cloned()
            .collect();
        self.accept(&ctx, &msg, content, attachments, author).await;
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        let _ = self.bot.set(ready.user.id);
        // Listed so Discord suggests them when the user types `/`; typing them as text still works.
        let mut commands = vec![
            CreateCommand::new("cast")
                .description("Send out a cast of crabs: many agents researching at once")
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "request",
                        "What to research",
                    )
                    .required(true),
                ),
            CreateCommand::new("browser")
                .description("Get a link to a live view of mokuzu's browser"),
            CreateCommand::new(&STOP_COMMAND[1..])
                .description("Stop what mokuzu is doing in this conversation"),
        ];
        if self.voice.is_some() {
            commands.push(
                CreateCommand::new("voice")
                    .description("Talk with mokuzu in the voice channel you are in"),
            );
            commands.push(CreateCommand::new("hangup").description("End the voice call"));
        }
        if let Err(err) = Command::set_global_commands(&ctx.http, commands).await {
            tracing::warn!("failed to register slash commands: {err:#}");
        }
        // Printed, not logged, so `mokuzu serve` shows it at the default log level.
        eprintln!("connected to Discord as {}", ready.user.name);
    }

    /// Someone allowed to talk to the bot joining one of `[voice] channels` starts a call there,
    /// answered in the channel's own text chat.
    async fn voice_state_update(&self, ctx: Context, old: Option<VoiceState>, new: VoiceState) {
        let (Some((_, section)), Some(guild), Some(channel)) =
            (&self.voice, new.guild_id, new.channel_id)
        else {
            return;
        };
        let author = new.user_id.get();
        let moved_in = old.and_then(|old| old.channel_id) != Some(channel);
        if !moved_in
            || !section.channels.contains(&channel.get())
            || self.bot.get() == Some(&new.user_id)
            || !self.discord.can_talk(Some(guild.get()), author)
            || lock(&self.calls).contains_key(&guild)
        {
            return;
        }
        if let Err(err) = self.start_call(&ctx.http, guild, channel, author, channel) {
            tracing::warn!("failed to start a voice call: {err:#}");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let result = match interaction {
            Interaction::Component(click) => self.on_click(&ctx, &click).await,
            Interaction::Command(command) => self.on_command(&ctx, &command).await,
            _ => Ok(()),
        };
        if let Err(err) = result {
            tracing::warn!("failed to handle interaction: {err:#}");
        }
    }
}

/// Where `msg` is answered: a new thread off a message in a server text channel, otherwise
/// the channel itself. A new thread starts with the message's first line as its name, and the
/// model renames it in the background.
async fn route(ctx: &Context, msg: &Message, content: &str, config: &Config) -> Result<ChannelId> {
    let here = msg.channel_id;
    if msg.guild_id.is_none() {
        return Ok(here);
    }
    // ponytail: one channel lookup per server message; enable serenity's cache if volume grows.
    let Some(channel) = here.to_channel(&ctx.http).await?.guild() else {
        return Ok(here);
    };
    Ok(match channel.kind {
        ChannelType::Text | ChannelType::News => {
            let thread = CreateThread::new(thread_name(content))
                .auto_archive_duration(AutoArchiveDuration::OneHour);
            let thread = here
                .create_thread_from_message(&ctx.http, msg.id, thread)
                .await?;
            let (http, config, content) = (ctx.http.clone(), config.clone(), content.to_owned());
            tokio::spawn(async move { rename(&http, thread.id, &config, &content).await });
            thread.id
        }
        _ => here,
    })
}

/// Renames `thread` to a title the model writes for `content`; failures are only logged, and
/// the thread keeps its first-line name.
async fn rename(http: &Http, thread: ChannelId, config: &Config, content: &str) {
    if content.trim().is_empty() {
        return;
    }
    let title = match agent::title(config, content).await {
        Ok(title) if !title.is_empty() => thread_name(&title),
        Ok(_) => return,
        Err(err) => {
            tracing::warn!("failed to title thread: {err:#}");
            return;
        }
    };
    if let Err(err) = thread
        .edit_thread(http, EditThread::new().name(title))
        .await
    {
        tracing::warn!("failed to rename thread: {err:#}");
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `content` without @mentions of `bot`, which only say who the message is for.
fn strip_mention(content: &str, bot: UserId) -> String {
    content
        .replace(&format!("<@{bot}>"), "")
        .replace(&format!("<@!{bot}>"), "")
        .trim()
        .to_owned()
}

/// The message's first line, shortened to fit a thread name.
fn thread_name(content: &str) -> String {
    let line = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut name: String = line.chars().take(MAX_THREAD_NAME_CHARS).collect();
    if name.len() < line.len() {
        name.push('…');
    }
    if name.is_empty() {
        name.push_str("mokuzu");
    }
    name
}

#[tracing::instrument(skip_all, fields(channel = %channel))]
async fn run_channel(
    config: Config,
    db: Db,
    mcp: Arc<Mcp>,
    http: Arc<Http>,
    channel: ChannelId,
    stop: Arc<Stop>,
    inbox: mpsc::UnboundedReceiver<Input>,
) {
    let key = format!("discord:{channel}");
    let voice = config.voice.clone();
    let mut agent = match Agent::new(config, db, mcp, &key).await {
        Ok(agent) => agent,
        Err(err) => {
            tracing::error!("failed to start agent: {err:#}");
            return;
        }
    };
    agent.stop_with(stop);
    let mut io = DiscordIo {
        http,
        web: reqwest::Client::new(),
        channel,
        inbox,
        approval: None,
        status: None,
        board: None,
        overheard: Vec::new(),
        reply: None,
    };
    // Input that ended a merged burst, handled next on its own.
    let mut next = None;
    loop {
        let input = match next.take() {
            Some(input) => input,
            None => match io.inbox.recv().await {
                Some(input) => input,
                None => break,
            },
        };
        let (mut content, mut attachments, origin, message, author, reply) = match input {
            Input::Text {
                content,
                attachments,
                origin,
                message,
                author,
                reply,
                ..
            } => (content, attachments, origin, message, author, reply),
            Input::Overheard { who, text } => {
                if let Err(err) = agent.overhear(&who, &text).await {
                    tracing::warn!("{err:#}");
                }
                continue;
            }
            Input::Decision { .. } => continue, // A click on an approval that already timed out.
        };
        let mut messages = vec![(origin, message)];
        // A call's request is answered on its own, so its reply holds only its answer.
        if reply.is_none() {
            next = merge_waiting(
                &mut io.inbox,
                author,
                &mut content,
                &mut attachments,
                &mut messages,
            )
            .await;
        }
        io.reply = reply.map(|reply| (reply, String::new()));
        let prompt = content.trim();
        if prompt.is_empty() && attachments.is_empty() {
            io.finish_all(&messages, true).await;
            continue;
        }
        io.status = None;
        let typing = io.channel.start_typing(&io.http);
        let result = match download(&io.web, &attachments, voice.as_ref()).await {
            Ok((text, images)) => {
                let prompt = format!("{prompt}{text}");
                agent.run_turn(prompt.trim(), images, author, &mut io).await
            }
            Err(err) => Err(err),
        };
        drop(typing);
        // Chat overheard while the turn waited for approval goes in after it.
        for (who, text) in std::mem::take(&mut io.overheard) {
            if let Err(err) = agent.overhear(&who, &text).await {
                tracing::warn!("{err:#}");
            }
        }
        if let Err(err) = &result {
            tracing::warn!("turn failed: {err:#}");
            let text = if err.is::<Stopped>() {
                "⏹️ stopped".to_owned()
            } else {
                format!("error: {err:#}")
            };
            if let Err(err) = io.say(&text).await {
                tracing::error!("failed to report error: {err:#}");
            }
        }
        if let Some((reply, said)) = io.reply.take() {
            let said = if said.trim().is_empty() {
                "(mokuzu did the request and had nothing to add)".to_owned()
            } else {
                said
            };
            let _ = reply.send(said);
        }
        io.finish_all(&messages, result.is_ok()).await;
    }
}

/// Folds text messages into one prompt until `inbox` stays quiet for `MERGE_QUIET` or `MERGE_CAP`
/// passes, so a burst,
/// or messages sent while a turn ran, is answered once. Stops at a message from someone other
/// than `author` or at overheard chat, which is returned to handle after this prompt.
async fn merge_waiting(
    inbox: &mut mpsc::UnboundedReceiver<Input>,
    author: u64,
    content: &mut String,
    attachments: &mut Vec<Attachment>,
    messages: &mut Vec<(ChannelId, MessageId)>,
) -> Option<Input> {
    let cap = tokio::time::Instant::now() + MERGE_CAP;
    loop {
        let quiet = tokio::time::Instant::now() + MERGE_QUIET;
        let Ok(Some(input)) = tokio::time::timeout_at(quiet.min(cap), inbox.recv()).await else {
            return None;
        };
        let Input::Text {
            content: more,
            attachments: more_attachments,
            origin,
            message,
            author: sender,
            can_approve,
            reply,
        } = input
        else {
            if matches!(input, Input::Overheard { .. }) {
                return Some(input);
            }
            continue;
        };
        // Each turn has one sender, so it loads one person's memory; a call's request goes alone.
        if sender != author || reply.is_some() {
            return Some(Input::Text {
                content: more,
                attachments: more_attachments,
                origin,
                message,
                author: sender,
                can_approve,
                reply,
            });
        }
        content.push('\n');
        content.push_str(&more);
        attachments.extend(more_attachments);
        messages.push((origin, message));
    }
}

struct DiscordIo {
    http: Arc<Http>,
    /// Downloads image attachments.
    web: reqwest::Client,
    channel: ChannelId,
    inbox: mpsc::UnboundedReceiver<Input>,
    /// The last approved command's message and its command block, where the output goes.
    approval: Option<(MessageId, String)>,
    /// The status message notes are being collected into, and its text so far.
    status: Option<(MessageId, String)>,
    /// The live board message, edited in place until the board is let go.
    board: Option<MessageId>,
    /// Chat overheard while a turn waits for approval, as `(who, text)`, added once it ends.
    overheard: Vec<(String, String)>,
    /// A voice call's request this turn answers, and everything the turn has said so far.
    reply: Option<(oneshot::Sender<String>, String)>,
}

impl DiscordIo {
    /// Sends a message that can't ping anyone, whatever the model or a search result put in it.
    async fn send(&self, message: CreateMessage) -> Result<MessageId> {
        let message = message.allowed_mentions(CreateAllowedMentions::new());
        Ok(self
            .channel
            .send_message(&self.http, message)
            .await
            .context("failed to send Discord message")?
            .id)
    }

    /// Swaps the bot's 👀 on `message` in `origin` for ✅ or ❌; failures are only logged.
    async fn finish(&self, origin: ChannelId, message: MessageId, ok: bool) {
        let swap = async {
            origin
                .delete_reaction(&self.http, message, None, RECEIVED)
                .await?;
            let done = if ok { DONE } else { FAILED };
            origin.create_reaction(&self.http, message, done).await
        };
        if let Err(err) = swap.await {
            tracing::warn!("failed to update reaction: {err:#}");
        }
    }

    async fn finish_all(&self, messages: &[(ChannelId, MessageId)], ok: bool) {
        for &(origin, message) in messages {
            self.finish(origin, message, ok).await;
        }
    }

    /// Replaces the approval message's text and removes its buttons; failures are only logged.
    async fn settle(&self, message: MessageId, content: String) {
        let edit = EditMessage::new().content(content).components(Vec::new());
        if let Err(err) = self.channel.edit_message(&self.http, message, edit).await {
            tracing::warn!("failed to update approval message: {err:#}");
        }
    }
}

impl Io for DiscordIo {
    async fn say(&mut self, text: &str) -> Result<()> {
        if let Some((_, said)) = &mut self.reply {
            said.push_str(text);
            said.push('\n');
        }
        // Notes after this reply start a new status message below it.
        self.status = None;
        for chunk in chunks(text, MAX_MESSAGE_CHARS) {
            self.send(CreateMessage::new().content(chunk)).await?;
        }
        Ok(())
    }

    /// Shows `text` as small gray subtext, merging back-to-back notes into one edited message.
    async fn note(&mut self, text: &str) -> Result<()> {
        let first_line = text.lines().next().unwrap_or_default();
        let line: String = format!("-# {first_line}")
            .chars()
            .take(MAX_STATUS_CHARS)
            .collect();
        if let Some((message, body)) = &mut self.status
            && body.chars().count() + line.chars().count() < MAX_MESSAGE_CHARS
        {
            body.push('\n');
            body.push_str(&line);
            let edit = EditMessage::new().content(body.as_str());
            if let Err(err) = self.channel.edit_message(&self.http, *message, edit).await {
                tracing::warn!("failed to update status message: {err:#}");
            }
            return Ok(());
        }
        let message = self.send(CreateMessage::new().content(&line)).await?;
        self.status = Some((message, line));
        Ok(())
    }

    async fn board(&mut self, text: &str) -> Result<()> {
        if text.is_empty() {
            self.board = None;
            return Ok(());
        }
        let body: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        if let Some(message) = self.board {
            let edit = EditMessage::new().content(body.as_str());
            if let Err(err) = self.channel.edit_message(&self.http, message, edit).await {
                tracing::warn!("failed to update board message: {err:#}");
            }
            return Ok(());
        }
        // Notes after the board start a new status message below it.
        self.status = None;
        self.board = Some(self.send(CreateMessage::new().content(body)).await?);
        Ok(())
    }

    async fn send_file(&mut self, name: &str, bytes: Vec<u8>) -> Result<()> {
        self.status = None;
        self.send(CreateMessage::new().add_file(CreateAttachment::bytes(bytes, name)))
            .await?;
        Ok(())
    }

    async fn confirm(&mut self, command: &str) -> Result<bool> {
        let block = code_block("", command, MAX_MESSAGE_CHARS / 2);
        let buttons = CreateActionRow::Buttons(vec![
            CreateButton::new(RUN)
                .label("Run")
                .style(ButtonStyle::Success),
            CreateButton::new(DENY)
                .label("Deny")
                .style(ButtonStyle::Danger),
        ]);
        self.status = None;
        let message = self
            .send(
                CreateMessage::new()
                    .content(&block)
                    .components(vec![buttons]),
            )
            .await
            .context("failed to send approval request")?;
        let deadline = tokio::time::Instant::now() + CONFIRM_TIMEOUT;
        let run = loop {
            match tokio::time::timeout_at(deadline, self.inbox.recv()).await {
                Err(_) => {
                    self.settle(message, format!("{block}\n⌛ timed out, not run"))
                        .await;
                    return Ok(false);
                }
                Ok(None) => return Ok(false),
                // The click handler already updated the message.
                Ok(Some(Input::Decision { message: id, run })) if id == message => break run,
                Ok(Some(Input::Decision { .. })) => {}
                Ok(Some(Input::Overheard { who, text })) => self.overheard.push((who, text)),
                // Someone who may not approve can't answer either; their message is dropped.
                Ok(Some(Input::Text {
                    origin,
                    message: reply,
                    can_approve: false,
                    ..
                })) => self.finish(origin, reply, false).await,
                // Typing an answer still works; anything but `y` is a no.
                Ok(Some(Input::Text {
                    content,
                    origin,
                    message: reply,
                    ..
                })) => {
                    let run = content.trim().eq_ignore_ascii_case("y");
                    self.finish(origin, reply, true).await;
                    let status = if run {
                        "▶️ running…"
                    } else {
                        "✖️ denied"
                    };
                    self.settle(message, format!("{block}\n{status}")).await;
                    break run;
                }
            }
        };
        if run {
            self.approval = Some((message, block));
        }
        Ok(run)
    }

    async fn ran(&mut self, output: &str, ok: bool) -> Result<()> {
        let Some((message, block)) = self.approval.take() else {
            return Ok(());
        };
        let status = if ok { "✅ done" } else { "❌ failed" };
        let mut content = format!("{block}\n{status}");
        let room = MAX_MESSAGE_CHARS.saturating_sub(content.chars().count() + 16);
        let output = output.trim();
        if !output.is_empty() && room > 100 {
            content.push('\n');
            content.push_str(&code_block("", &tail(output, room), room));
        }
        self.settle(message, content).await;
        Ok(())
    }
}

/// The attachment's image format, if it is an image the model can read.
fn image_type(attachment: &Attachment) -> Option<ImageMediaType> {
    let mime = attachment
        .content_type
        .as_deref()?
        .split(';')
        .next()?
        .trim();
    ImageMediaType::from_mime_type(mime)
}

/// Whether the attachment is text to read into the prompt.
fn is_text(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|mime| mime.starts_with("text/") || mime.starts_with("application/json"))
}

/// Whether the attachment is audio, like a Discord voice message, to transcribe into the prompt.
fn is_audio(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|mime| mime.starts_with("audio/"))
}

/// Whether the attachment is a PDF or Office document to read into the prompt as text.
fn is_document(attachment: &Attachment) -> bool {
    document::has_document_name(&attachment.filename)
}

/// Downloads `attachments` for the model: text files as prompt text to append, images as images.
/// Documents become their text, cut to `MAX_DOCUMENT_CHARS`, and a PDF's first pages also come as
/// images. Audio becomes its transcript with `voice`'s key, and is left out without one. Files
/// over `MAX_TEXT_BYTES`, `MAX_IMAGE_BYTES`, `document::MAX_BYTES`, or
/// `voice::MAX_TRANSCRIBE_BYTES` are left out.
async fn download(
    web: &reqwest::Client,
    attachments: &[Attachment],
    voice: Option<&VoiceSection>,
) -> Result<(String, Vec<Image>)> {
    let mut text = String::new();
    let mut images = Vec::new();
    for attachment in attachments {
        let media_type = image_type(attachment);
        let audio = is_audio(attachment).then_some(voice).flatten();
        if is_audio(attachment) && audio.is_none() {
            continue;
        }
        let limit = if media_type.is_some() {
            MAX_IMAGE_BYTES
        } else if audio.is_some() {
            voice::MAX_TRANSCRIBE_BYTES
        } else if is_document(attachment) {
            u32::try_from(document::MAX_BYTES).unwrap_or(u32::MAX)
        } else {
            MAX_TEXT_BYTES
        };
        if attachment.size > limit {
            tracing::warn!(
                "skipping {}: {} bytes",
                attachment.filename,
                attachment.size
            );
            continue;
        }
        let bytes = web
            .get(&attachment.url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("failed to download {}", attachment.filename))?
            .bytes()
            .await
            .with_context(|| format!("failed to download {}", attachment.filename))?;
        if media_type.is_some() {
            images.push(document::image(&bytes, media_type));
        } else if let Some(section) = audio {
            let body =
                match voice::transcribe(web, section, &attachment.filename, bytes.to_vec()).await {
                    Ok(transcript) => transcript,
                    Err(err) => format!("(could not transcribe this audio: {err:#})"),
                };
            text.push_str(&format!(
                "\n\n[voice message {}, transcribed]\n{body}",
                attachment.filename
            ));
        } else if is_document(attachment) {
            let body = match document::extract(bytes.to_vec()).await {
                Ok(body) => clip(&body, MAX_DOCUMENT_CHARS),
                Err(err) => format!("(could not read this document: {err:#})"),
            };
            text.push_str(&format!("\n\n[{}]\n{body}", attachment.filename));
            if bytes.starts_with(b"%PDF-")
                && let Some((note, pages)) =
                    document::preview(bytes.to_vec(), &attachment.url).await
            {
                text.push_str(&format!("\n{note}"));
                images.extend(pages);
            }
        } else {
            text.push_str(&format!(
                "\n\n[{}]\n{}",
                attachment.filename,
                String::from_utf8_lossy(&bytes)
            ));
        }
    }
    Ok((text, images))
}

/// `text` cut to `max` characters, saying how much was left out.
fn clip(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n[cut: {max} of {total} characters shown]")
}

/// Wraps `text` in a code fence, cut to `max` characters, so it can't break out of the fence.
fn code_block(lang: &str, text: &str, max: usize) -> String {
    let text = text.replace("```", "`\u{200b}``");
    let cut: String = text.chars().take(max).collect();
    let more = if cut.len() < text.len() { "\n…" } else { "" };
    format!("```{lang}\n{cut}{more}\n```")
}

/// The last `max` characters of `text`, marked when cut, since errors usually come last.
fn tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().skip(count - max + 2).collect();
    format!("…\n{kept}")
}

/// Splits `text` into pieces of at most `max` characters.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(max)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_job_cannot_start_again_until_it_ends() {
        let first = Running::start("test:7".to_owned()).expect("first run");
        assert!(Running::start("test:7".to_owned()).is_none());
        assert!(Running::start("test:8".to_owned()).is_some());
        drop(first);
        assert!(Running::start("test:7".to_owned()).is_some());
    }

    #[test]
    fn chunks_respect_limit_and_keep_all_text() {
        let text = "é".repeat(2 * MAX_MESSAGE_CHARS + 3);
        let parts = chunks(&text, MAX_MESSAGE_CHARS);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_MESSAGE_CHARS));
        assert_eq!(parts.concat(), text);
        assert!(chunks("", MAX_MESSAGE_CHARS).is_empty());
    }

    #[tokio::test]
    async fn merge_waiting_joins_burst_and_stops_at_new() {
        let text = |content: &str, id: u64| Input::Text {
            content: content.to_owned(),
            attachments: Vec::new(),
            origin: ChannelId::new(1),
            message: MessageId::new(id),
            author: 7,
            can_approve: true,
            reply: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        for input in [
            text("b", 2),
            Input::Decision {
                message: MessageId::new(9),
                run: true,
            },
            text("c", 3),
        ] {
            tx.send(input).unwrap();
        }
        drop(tx);
        let mut content = "a".to_owned();
        let mut attachments = Vec::new();
        let mut messages = vec![(ChannelId::new(1), MessageId::new(1))];
        let next = merge_waiting(&mut rx, 7, &mut content, &mut attachments, &mut messages).await;
        assert_eq!(content, "a\nb\nc");
        assert_eq!(messages.len(), 3);
        assert!(next.is_none());
    }

    #[tokio::test]
    async fn merge_waiting_stops_at_another_sender() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for (author, id) in [(7, 2), (8, 3)] {
            let input = Input::Text {
                content: "more".to_owned(),
                attachments: Vec::new(),
                origin: ChannelId::new(1),
                message: MessageId::new(id),
                author,
                can_approve: true,
                reply: None,
            };
            tx.send(input).unwrap();
        }
        drop(tx);
        let mut content = "a".to_owned();
        let mut messages = Vec::new();
        let next = merge_waiting(&mut rx, 7, &mut content, &mut Vec::new(), &mut messages).await;
        assert_eq!(content, "a\nmore");
        assert!(matches!(next, Some(Input::Text { author: 8, .. })));
    }

    #[tokio::test]
    async fn merge_waiting_stops_at_cap() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for id in 2.. {
                let input = Input::Text {
                    content: "more".to_owned(),
                    attachments: Vec::new(),
                    origin: ChannelId::new(1),
                    message: MessageId::new(id),
                    author: 7,
                    can_approve: true,
                    reply: None,
                };
                if tx.send(input).is_err() {
                    break;
                }
                tokio::time::sleep(MERGE_QUIET / 2).await;
            }
        });
        let start = tokio::time::Instant::now();
        let mut content = String::new();
        let mut messages = Vec::new();
        merge_waiting(&mut rx, 7, &mut content, &mut Vec::new(), &mut messages).await;
        assert!(start.elapsed() < MERGE_CAP + MERGE_QUIET / 2);
        assert!(messages.len() >= 2);
    }

    #[test]
    fn strip_mention_drops_only_the_bot() {
        let bot = UserId::new(42);
        assert_eq!(strip_mention("<@42> hi <@7>", bot), "hi <@7>");
        assert_eq!(strip_mention("hi <@!42>", bot), "hi");
    }

    #[test]
    fn thread_name_uses_first_line_within_limit() {
        assert_eq!(thread_name("\n  check disk  \nmore"), "check disk");
        assert_eq!(thread_name("   "), "mokuzu");
        let long = thread_name(&"磁".repeat(200));
        assert_eq!(long.chars().count(), MAX_THREAD_NAME_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn code_block_and_tail_stay_within_limits() {
        assert_eq!(code_block("sh", "ls", 10), "```sh\nls\n```");
        let fenced = code_block("", "a```b", 100);
        assert_eq!(fenced.matches("```").count(), 2, "{fenced}");
        assert!(code_block("", &"x".repeat(50), 10).contains("xxxxxxxxxx\n…"));
        assert_eq!(tail("short", 10), "short");
        let cut = tail(&"é".repeat(50), 10);
        assert!(cut.starts_with("…\n") && cut.chars().count() == 10, "{cut}");
    }
}
