//! Voice calls over Discord: `/voice` joins the caller's voice channel and talks with them through
//! OpenAI's Realtime API. The realtime model only holds the conversation; anything past small talk
//! it hands to mokuzu's agent with `ask_mokuzu`, which runs as an ordinary turn in the text
//! channel the call was started from (approvals included), and it reads the answer back.
//!
//! Discord decodes the caller's voice to 24 kHz mono, the rate the Realtime API takes, and the
//! model's 24 kHz replies play as one endless track that is silent between them.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures::future::BoxFuture;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, GuildId};
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::input::RawAdapter;
use songbird::input::core::io::MediaSource;
use songbird::{CoreEvent, Event, EventContext, Songbird};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use crate::config::VoiceSection;

/// Starts a call with the sender, in the voice channel they are in.
pub const VOICE_COMMAND: &str = "/voice";
/// Ends the call in this server.
pub const HANGUP_COMMAND: &str = "/hangup";

/// Sample rate of audio both ways.
/// OpenAI's speech-to-text endpoint, for voice messages sent as attachments.
const TRANSCRIBE_URL: &str = "https://api.openai.com/v1/audio/transcriptions";
const TRANSCRIBE_MODEL: &str = "gpt-4o-mini-transcribe";
/// OpenAI rejects audio files over 25 MB.
pub const MAX_TRANSCRIBE_BYTES: u32 = 25 * 1024 * 1024;

/// Transcribes the audio file `bytes`, named `name`, with the `[voice]` key; the call's
/// `language` steers the script (e.g. Traditional over Simplified Chinese).
pub async fn transcribe(
    web: &reqwest::Client,
    section: &VoiceSection,
    name: &str,
    bytes: Vec<u8>,
) -> Result<String> {
    let file = reqwest::multipart::Part::bytes(bytes).file_name(name.to_owned());
    let form = reqwest::multipart::Form::new()
        .text("model", TRANSCRIBE_MODEL)
        .text("response_format", "json")
        .text(
            "prompt",
            format!("The speaker most likely talks in {}.", section.language),
        )
        .part("file", file);
    let body: Value = web
        .post(TRANSCRIBE_URL)
        .bearer_auth(section.api_key.as_str())
        .multipart(form)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .context("transcription request failed")?
        .json()
        .await
        .context("transcription response unreadable")?;
    body["text"]
        .as_str()
        .map(|text| text.trim().to_owned())
        .context("transcription response had no text")
}

const RATE: usize = 24_000;
/// Samples in one 20 ms Discord frame at `RATE`.
const FRAME: usize = RATE / 50;
/// Audio sent to the Realtime API per message: 100 ms.
const CHUNK: usize = FRAME * 5;
/// Silent frames still sent after the caller stops talking, so turn detection hears them stop.
// ponytail: 3 s of trailing silence; stream it all the time if turn detection still misses ends.
const QUIET_TAIL: usize = 150;
/// Longest an `ask_mokuzu` waits for the agent, approvals included.
const ASK_LIMIT: Duration = Duration::from_secs(600);
/// How long a call waits to hear the caller before rejoining.
const DEAF_LIMIT: Duration = Duration::from_secs(15);

const INSTRUCTIONS: &str = "You are mokuzu, the user's personal assistant, on a Discord voice \
call with them. Speak {language}, briefly and naturally, as on a phone call. You have no tools, \
and you know only the saved facts below about the user: for anything past small talk (questions, \
tasks, their notes, schedules, the web), call ask_mokuzu with their request, say a few words while \
it works, then tell them the answer in a sentence or two. Never make up what mokuzu found. If the \
answer says something waits for approval, tell them to approve it in Discord. When they say you can \
go or the call is over, say a short goodbye and call hang_up.";

/// Who a call is with: their Discord user ID, and what mokuzu's memory holds about them.
pub struct Caller {
    pub id: u64,
    pub memory: Vec<String>,
}

/// Runs one `ask_mokuzu`: hands the request to the agent and returns what it said.
pub type Ask = Arc<dyn Fn(String) -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// The voice manager, decoding what people say to 24 kHz mono.
pub fn songbird() -> Arc<Songbird> {
    let decode = DecodeConfig::new(Channels::Mono, SampleRate::Hz24000);
    Songbird::serenity_from_config(
        songbird::Config::default().decode_mode(DecodeMode::Decode(decode)),
    )
}

/// Talks with `caller` in voice `channel` until they leave it or `stop` fires, then leaves.
///
/// With Discord's voice encryption (DAVE), a join sometimes never learns which audio stream is the
/// caller's, so nothing they say gets through (songbird#310). A call that has not heard them
/// within `DEAF_LIMIT` rejoins, until one does.
pub async fn call(
    voice: Arc<Songbird>,
    config: VoiceSection,
    guild: GuildId,
    channel: ChannelId,
    caller: Caller,
    ask: Ask,
    stop: Arc<Notify>,
) -> Result<()> {
    for attempt in 1.. {
        let (heard, ended) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let (frames, playback) = join(&voice, guild, channel, caller.id, &heard, &ended).await?;
        let outcome = tokio::select! {
            () = stop.notified() => Some(Ok(())),
            talked = talk(&config, &caller.memory, frames, &playback, &ask, &ended) => Some(talked),
            Err(_) = tokio::time::timeout(DEAF_LIMIT, heard.notified()) => None,
        };
        if let Err(err) = voice.remove(guild).await {
            tracing::warn!("failed to leave the voice channel: {err:#}");
        }
        match outcome {
            Some(talked) => return talked,
            None => tracing::warn!(attempt, "voice call cannot hear the caller; rejoining"),
        }
    }
    Ok(())
}

/// Joins `channel` with a listener for `user` and a track for replies. `heard` fires once their
/// audio stream is known, `ended` when they leave or the connection drops.
async fn join(
    voice: &Songbird,
    guild: GuildId,
    channel: ChannelId,
    user: u64,
    heard: &Arc<Notify>,
    ended: &Arc<Notify>,
) -> Result<(
    mpsc::UnboundedReceiver<Option<Vec<i16>>>,
    Arc<Mutex<Playback>>,
)> {
    // Listen before joining: Discord says who is on which audio stream right as the bot joins,
    // and a listener added after `join` returns can miss it.
    let call = voice.get_or_insert(guild);
    let (frames_tx, frames) = mpsc::unbounded_channel();
    let playback = Arc::new(Mutex::new(Playback::default()));
    {
        let mut call = call.lock().await;
        let listener = Listener {
            user,
            ssrc: Arc::new(Mutex::new(None)),
            frames: frames_tx,
            heard: Arc::clone(heard),
            ended: Arc::clone(ended),
        };
        for event in [
            CoreEvent::SpeakingStateUpdate,
            CoreEvent::VoiceTick,
            CoreEvent::ClientDisconnect,
            CoreEvent::DriverDisconnect,
        ] {
            call.add_global_event(Event::Core(event), listener.clone());
        }
        let speaker = Speaker(Arc::clone(&playback));
        let rate = u32::try_from(RATE)?;
        call.play_input(RawAdapter::new(speaker, rate, 1).into());
    }
    let joining = call.lock().await.join(channel).await;
    let joined = match joining {
        Ok(join) => join.await,
        Err(err) => Err(err),
    };
    if let Err(err) = joined {
        let _ = voice.remove(guild).await;
        return Err(err).context("failed to join the voice channel");
    }
    Ok((frames, playback))
}

/// Relays the caller's audio to the Realtime API and its replies to `playback` until `stop`.
async fn talk(
    config: &VoiceSection,
    memory: &[String],
    mut frames: mpsc::UnboundedReceiver<Option<Vec<i16>>>,
    playback: &Mutex<Playback>,
    ask: &Ask,
    stop: &Notify,
) -> Result<()> {
    let mut request = format!("{}?model={}", config.url, config.model).into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", config.api_key.as_str()).parse()?,
    );
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .context("failed to connect to the Realtime API")?;
    let (mut write, mut read) = socket.split();
    write.send(text(&session(config, memory))).await?;
    tracing::info!("voice call connected");

    let (answers_tx, mut answers) = mpsc::unbounded_channel::<(String, String)>();
    let mut chunk: Vec<i16> = Vec::with_capacity(CHUNK);
    let mut quiet = QUIET_TAIL;
    // The server reports a bad key or model as an error event, then closes.
    let mut last_error = None;
    // Set by `hang_up`: the call ends once the goodbye has played.
    let mut leaving = false;
    loop {
        tokio::select! {
            () = stop.notified() => break,
            frame = frames.recv() => {
                let Some(frame) = frame else { break };
                if leaving && lock(playback).queue.is_empty() {
                    break;
                }
                match frame {
                    Some(pcm) => {
                        quiet = 0;
                        chunk.extend(pcm);
                    }
                    None if quiet < QUIET_TAIL => {
                        quiet += 1;
                        chunk.extend(std::iter::repeat_n(0, FRAME));
                    }
                    None => {}
                }
                if chunk.len() >= CHUNK {
                    let audio = encode(&chunk);
                    chunk.clear();
                    write.send(text(&json!({"type": "input_audio_buffer.append", "audio": audio}))).await?;
                }
            }
            answer = answers.recv() => {
                let Some((call_id, output)) = answer else { continue };
                let item = json!({
                    "type": "conversation.item.create",
                    "item": {"type": "function_call_output", "call_id": call_id, "output": output},
                });
                write.send(text(&item)).await?;
                write.send(text(&json!({"type": "response.create"}))).await?;
            }
            message = read.next() => {
                let message = match message {
                    Some(Ok(Message::Close(_))) | None => match last_error {
                        Some(error) => bail!("the Realtime API closed the call: {error}"),
                        None => bail!("the Realtime API closed the call"),
                    },
                    Some(message) => message.context("the Realtime API connection broke")?,
                };
                let Message::Text(body) = message else { continue };
                let event: Value = serde_json::from_str(&body)?;
                leaving |= hangs_up(&event);
                if event["type"] == "error" {
                    last_error = event["error"]["message"].as_str().map(str::to_owned);
                }
                for reply in handle(&event, playback, ask, &answers_tx) {
                    write.send(text(&reply)).await?;
                }
            }
        }
    }
    let _ = write.close().await;
    Ok(())
}

/// The session: the caller's language and saved facts, the `ask_mokuzu` and `hang_up` tools, PCM
/// both ways, and turn detection that lets the caller interrupt.
fn session(config: &VoiceSection, memory: &[String]) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "type": "realtime",
            "instructions": instructions(&config.language, memory),
            "output_modalities": ["audio"],
            "audio": {
                "input": {
                    "format": {"type": "audio/pcm", "rate": RATE},
                    "turn_detection": {"type": "semantic_vad"},
                },
                "output": {
                    "format": {"type": "audio/pcm", "rate": RATE},
                    "voice": config.voice,
                },
            },
            "tools": [{
                "type": "function",
                "name": "ask_mokuzu",
                "description": "Hand the user's request to mokuzu, the agent with their memory, \
                    notes, schedules, files, MCP tools, and web search. Use it for anything past \
                    small talk. Returns mokuzu's answer as text, which may be long; tell the user \
                    the gist.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "request": {
                            "type": "string",
                            "description": "What the user asked, in their words and language, \
                                with any context from the call it needs.",
                        },
                    },
                    "required": ["request"],
                },
            }, {
                "type": "function",
                "name": "hang_up",
                "description": "Leave the call, when the user says you can go or they are done. \
                    Say goodbye first; the call ends once you finish speaking.",
                "parameters": {"type": "object", "properties": {}},
            }],
            "tool_choice": "auto",
        },
    })
}

/// `INSTRUCTIONS` in `language`, with the caller's saved facts.
fn instructions(language: &str, memory: &[String]) -> String {
    let facts = if memory.is_empty() {
        "\n\nSaved facts: none yet.".to_owned()
    } else {
        format!("\n\nSaved facts:\n- {}", memory.join("\n- "))
    };
    INSTRUCTIONS.replace("{language}", language) + &facts
}

/// Acts on one server event; returns events to send back.
fn handle(
    event: &Value,
    playback: &Mutex<Playback>,
    ask: &Ask,
    answers: &mpsc::UnboundedSender<(String, String)>,
) -> Vec<Value> {
    let kind = event["type"].as_str().unwrap_or_default();
    if kind != "response.output_audio.delta" {
        tracing::debug!(kind, "Realtime API event");
    }
    match kind {
        "response.output_audio.delta" => {
            let item = event["item_id"].as_str().unwrap_or_default();
            match decode(event["delta"].as_str().unwrap_or_default()) {
                Ok(samples) => lock(playback).push(item, &samples),
                Err(err) => tracing::warn!("bad audio from the Realtime API: {err:#}"),
            }
            Vec::new()
        }
        // The caller started talking over the reply: the server cancels it; cut what they did not
        // hear from the conversation too.
        "input_audio_buffer.speech_started" => lock(playback)
            .interrupt()
            .map(|(item, heard_ms)| {
                json!({
                    "type": "conversation.item.truncate",
                    "item_id": item,
                    "content_index": 0,
                    "audio_end_ms": heard_ms,
                })
            })
            .into_iter()
            .collect(),
        "response.done" => {
            for call in calls(event) {
                let (ask, answers) = (Arc::clone(ask), answers.clone());
                tokio::spawn(async move {
                    let (call_id, request) = call;
                    tracing::info!(%request, "voice asks mokuzu");
                    let output = match tokio::time::timeout(ASK_LIMIT, ask(request)).await {
                        Ok(Ok(answer)) => answer,
                        Ok(Err(err)) => format!("error: {err:#}"),
                        Err(_) => "error: mokuzu did not answer in time".to_owned(),
                    };
                    let _ = answers.send((call_id, output));
                });
            }
            Vec::new()
        }
        "error" => {
            tracing::warn!("Realtime API error: {}", event["error"]);
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `ask_mokuzu` calls in a finished response, as `(call_id, request)`.
fn calls(event: &Value) -> Vec<(String, String)> {
    let Some(items) = event["response"]["output"].as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|item| item["type"] == "function_call" && item["name"] == "ask_mokuzu")
        .filter_map(|item| {
            let call_id = item["call_id"].as_str()?.to_owned();
            let args: Value = serde_json::from_str(item["arguments"].as_str()?).ok()?;
            Some((call_id, args["request"].as_str()?.to_owned()))
        })
        .collect()
}

/// Whether a finished response asks to end the call.
fn hangs_up(event: &Value) -> bool {
    event["type"] == "response.done"
        && event["response"]["output"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"] == "function_call" && item["name"] == "hang_up")
        })
}

fn text(event: &Value) -> Message {
    Message::text(event.to_string())
}

/// 16-bit little-endian PCM, base64 as the Realtime API takes it.
fn encode(samples: &[i16]) -> String {
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode(audio: &str) -> Result<Vec<i16>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(audio)?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| i16::from_le_bytes(pair))
        .collect())
}

fn lock(playback: &Mutex<Playback>) -> std::sync::MutexGuard<'_, Playback> {
    playback.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reply audio waiting to play, and how much of the current reply has played.
#[derive(Default)]
struct Playback {
    queue: VecDeque<f32>,
    /// The reply item playing, and samples of it handed to Discord.
    item: Option<String>,
    played: usize,
    /// The reply last cut short; its audio still on the way is dropped.
    cut: Option<String>,
}

impl Playback {
    fn push(&mut self, item: &str, samples: &[i16]) {
        if self.cut.as_deref() == Some(item) {
            return;
        }
        if self.item.as_deref() != Some(item) {
            self.item = Some(item.to_owned());
            self.played = 0;
        }
        self.queue
            .extend(samples.iter().map(|&s| f32::from(s) / 32_768.0));
    }

    /// Drops the unplayed rest of the reply; returns its item and the milliseconds heard, if any
    /// was still waiting.
    fn interrupt(&mut self) -> Option<(String, usize)> {
        if self.queue.is_empty() {
            return None;
        }
        self.queue.clear();
        let item = self.item.take()?;
        self.cut = Some(item.clone());
        Some((item, self.played * 1000 / RATE))
    }
}

/// The reply track: raw little-endian f32 samples, silence whenever no reply is waiting, never
/// ending.
struct Speaker(Arc<Mutex<Playback>>);

impl Read for Speaker {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut playback = lock(&self.0);
        let samples = buf.len() / 4;
        let mut taken = 0;
        for slot in buf.as_chunks_mut::<4>().0.iter_mut() {
            let sample = playback.queue.pop_front().inspect(|_| taken += 1);
            slot.copy_from_slice(&sample.unwrap_or(0.0).to_le_bytes());
        }
        playback.played += taken;
        Ok(samples * 4)
    }
}

impl Seek for Speaker {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

impl MediaSource for Speaker {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Forwards the caller's decoded voice, 20 ms at a time (`None` for a frame they were silent),
/// fires `heard` once their audio stream is known, and `ended` when they leave or the connection
/// drops.
#[derive(Clone)]
struct Listener {
    user: u64,
    /// The caller's audio stream, learned from their first speaking update.
    ssrc: Arc<Mutex<Option<u32>>>,
    frames: mpsc::UnboundedSender<Option<Vec<i16>>>,
    heard: Arc<Notify>,
    ended: Arc<Notify>,
}

#[serenity::async_trait]
impl songbird::EventHandler for Listener {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::SpeakingStateUpdate(speaking)
                if speaking.user_id.is_some_and(|id| id.0 == self.user) =>
            {
                let known = self
                    .ssrc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .replace(speaking.ssrc);
                if known.is_none() {
                    tracing::info!(ssrc = speaking.ssrc, "voice call hears the caller");
                    self.heard.notify_one();
                }
            }
            EventContext::VoiceTick(tick) => {
                let ssrc = *self.ssrc.lock().unwrap_or_else(|e| e.into_inner());
                let pcm = ssrc
                    .and_then(|ssrc| tick.speaking.get(&ssrc))
                    .and_then(|data| data.decoded_voice.clone());
                let _ = self.frames.send(pcm);
            }
            EventContext::ClientDisconnect(left) if left.user_id.0 == self.user => {
                self.ended.notify_one();
            }
            EventContext::DriverDisconnect(_) => self.ended.notify_one(),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_round_trips_through_base64() {
        let samples = [0, 1, -1, i16::MAX, i16::MIN];
        assert_eq!(decode(&encode(&samples)).expect("valid"), samples);
    }

    #[test]
    fn interrupting_reports_what_was_heard() {
        let mut playback = Playback::default();
        assert!(playback.interrupt().is_none(), "nothing playing");
        playback.push("item_1", &vec![100; RATE]);
        let mut speaker = Speaker(Arc::new(Mutex::new(std::mem::take(&mut playback))));
        let mut buf = vec![0; RATE / 2 * 4];
        assert_eq!(speaker.read(&mut buf).expect("read"), buf.len());
        let (item, heard_ms) = lock(&speaker.0).interrupt().expect("cut short");
        assert_eq!((item.as_str(), heard_ms), ("item_1", 500));
        assert!(lock(&speaker.0).queue.is_empty());
        lock(&speaker.0).push("item_1", &[100; 10]);
        assert!(
            lock(&speaker.0).queue.is_empty(),
            "late audio of a cut reply"
        );
        assert_eq!(speaker.read(&mut buf).expect("silence"), buf.len());
        assert!(buf.iter().all(|&b| b == 0), "silence once the reply is cut");
    }

    #[test]
    fn finished_responses_yield_their_ask_calls() {
        let event = json!({"type": "response.done", "response": {"output": [
            {"type": "message"},
            {"type": "function_call", "name": "ask_mokuzu", "call_id": "c1",
             "arguments": "{\"request\":\"明天天氣\"}"},
            {"type": "function_call", "name": "other", "call_id": "c2", "arguments": "{}"},
        ]}});
        assert_eq!(calls(&event), [("c1".to_owned(), "明天天氣".to_owned())]);
        assert!(!hangs_up(&event));
        let bye = json!({"type": "response.done", "response": {"output": [
            {"type": "function_call", "name": "hang_up", "call_id": "c3", "arguments": "{}"},
        ]}});
        assert!(hangs_up(&bye) && calls(&bye).is_empty());
    }

    #[test]
    fn instructions_carry_language_and_saved_facts() {
        let text = instructions(
            "繁體中文",
            &["Name: Alex".to_owned(), "Lives in Taipei".to_owned()],
        );
        assert!(text.contains("Speak 繁體中文,"), "{text}");
        assert!(
            text.ends_with("Saved facts:\n- Name: Alex\n- Lives in Taipei"),
            "{text}"
        );
        assert!(instructions("English", &[]).ends_with("Saved facts: none yet."));
    }

    #[tokio::test]
    async fn the_reply_track_is_playable() {
        use songbird::input::{Input, LiveInput, codecs};
        let speaker = Speaker(Arc::new(Mutex::new(Playback::default())));
        let input: Input = RawAdapter::new(speaker, 24000, 1).into();
        let playable = input
            .make_playable_async(codecs::get_codec_registry(), codecs::get_probe())
            .await
            .expect("songbird can decode raw PCM");
        let Input::Live(LiveInput::Parsed(mut parsed), _) = playable else {
            panic!("not parsed");
        };
        let packet = parsed.format.next_packet().expect("a packet");
        let frames = parsed.decoder.decode(&packet).expect("decodes").frames();
        assert_eq!(frames, FRAME);
    }
}
