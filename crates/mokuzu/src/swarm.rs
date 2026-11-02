//! `cast_crabs` tool: the calling model acts as an orchestrator and dispatches a wave of subagents, each
//! with its own role, task, and history, that work in parallel with the read-only web and file
//! tools, plus MCP tools of servers with `approve = false`, and report back. The orchestrator reads the reports and sends further waves as needed.

use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

use anyhow::Result;
use futures::StreamExt;
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::message::{AssistantContent, Message, ToolCall, ToolResultContent, UserContent};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::{self, Io, Model};
use crate::config::Config;
use crate::document;
use crate::fetch::{self, Fetcher};
use crate::files;
use crate::mcp::Mcp;
use crate::search::{self, WebSearch};

/// Subagents one call may dispatch; more go in another wave.
const MAX_AGENTS: usize = 10;
/// Subagents running at once; the rest wait their turn.
// ponytail: fixed; make it a config key if a provider's rate limit needs it lower or allows more.
const CONCURRENCY: usize = 10;
/// Model calls one subagent makes before it must report.
const AGENT_STEPS: usize = 8;
/// Characters kept from each report, so a full wave fits the orchestrator's context.
const MAX_REPORT_CHARS: usize = 1_500;
/// Longest one crab may work, so a hung model call or page can't stall the whole swarm.
const CRAB_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How often the live board is redrawn; Discord rate-limits message edits.
const BOARD_EVERY: Duration = Duration::from_millis(1_500);
/// Characters of a crab's current step shown on the board.
const MAX_STEP_CHARS: usize = 80;

/// Names handed to crabs, from a random starting point so each swarm meets different ones.
const CRAB_NAMES: &[&str] = &[
    "Pincher",
    "Snapper",
    "Clawdia",
    "Sidestep",
    "Shelly",
    "Bubbles",
    "Nipper",
    "Scuttle",
    "Mitts",
    "Fuzzy",
    "Krusty",
    "Barnacle",
    "Coral",
    "Reef",
    "Tidepool",
    "Driftwood",
    "Sandy",
    "Pebble",
    "Shellby",
    "Clacker",
    "Captain Claws",
    "Sergeant Snip",
    "Doctor Pinch",
    "Professor Shell",
    "Detective Scuttlebutt",
    "Admiral Crab",
    "Sir Pinchalot",
    "Lady Claw",
    "Madam Mitten",
    "Judge Pincer",
    "Hermit",
    "Fiddler",
    "Dungeness",
    "Kingsley",
    "Bluey",
    "Rocky",
    "Jonah",
    "Stone",
    "Velvet",
    "Ghost",
    "Mossy",
    "Reedy",
    "Marsh",
    "Lakeside",
    "Delta",
    "Estuary",
    "Brook",
    "Ripple",
    "Puddle",
    "Seaweed",
    "Kelp",
    "Plankton",
    "Foam",
    "Spray",
    "Breaker",
    "Surf",
    "Tide",
    "Ebb",
    "Flow",
    "Moonlight",
    "Starfish",
    "Lantern",
    "Compass",
    "Anchor",
    "Harbor",
    "Buoy",
    "Dock",
    "Pier",
    "Lighthouse",
    "Mariner",
    "Sideways",
    "Scurry",
    "Skitter",
    "Clatter",
    "Crunch",
    "Crackle",
    "Biscuit",
    "Dumpling",
    "Ginger",
    "Saffron",
    "Butter",
    "Pepper",
    "Chili",
    "Noodle",
    "Bisque",
    "Chowder",
    "Gumbo",
    "Paella",
    "Tempura",
    "Wasabi",
    "Pinchy",
    "Snippy",
    "Clawson",
    "Shellington",
    "Crabby",
    "Mittens",
    "Tuft",
    "Whisker",
    "Bristle",
    "Tenlegs",
];

/// The `n`th crab name; past the end of the list the names come round again, numbered.
fn crab_name(n: usize) -> String {
    let base = CRAB_NAMES[n % CRAB_NAMES.len()];
    match n / CRAB_NAMES.len() {
        0 => base.to_owned(),
        lap => format!("{base}{}", lap + 1),
    }
}

/// Starts a `/cast` turn, ahead of the user's request.
pub const COMMAND: &str = "/cast";

/// System prompt section, included only when `cast_crabs` is available.
pub const GUIDANCE: &str = "\
# Swarm
For large work that splits into many independent parts (broad research, comparing many products \
or sources, checking a long list of items), you may call cast_crabs to send out a cast of crabs \
(mokuzu is a mitten crab) that work on the parts in parallel. It is slow and costly, so use it \
only when the parts are many and each needs its own searching and reading; answer quick \
questions, and work where each step depends on the last, yourself. The user can also ask for it \
by starting a message with /cast, or by name: when they ask for a cast of crabs or swarm mode, in any \
language, call cast_crabs for that request.";

/// Put before the request of a `/cast` turn, which asks for `cast_crabs` outright.
pub const INSTRUCTION: &str = "\
[Cast of crabs mode (a group of crabs is a cast, and mokuzu is a mitten crab), started by the user with /cast.] \
Act as an orchestrator for the request below: split it into independent parts and call cast_crabs \
with many subagents at once instead of doing the parts yourself. Scale the swarm to the job: up \
to 10 subagents in one wave, and more waves when a broad survey needs more. Give each a role and one narrow, self-contained task with all the context it needs, since subagents see nothing but \
their task. Read the reports, then send another wave for gaps, follow-ups, or fact checks if \
needed, and finally write the answer yourself, resolving disagreements and naming the URLs it \
relies on.";

/// The request after a leading `/cast`, or `None` if `prompt` doesn't start with it.
pub fn request(prompt: &str) -> Option<&str> {
    let rest = prompt.trim_start().strip_prefix(COMMAND)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

const AGENT: &str = "\
You are a subagent in a swarm; an orchestrator gave you one task and will combine your report \
with others. Do only your task, using web_search and fetch_url (and read_file or list_dir for \
local files, view_pdf to look at a PDF's pages, and any other tools you are given). Prefer primary and official sources, check dates, and read promising pages rather \
than relying on snippets. Reply with a report under 200 words: the findings, the source URL after \
each claim, and what you could not find or verify. Never fabricate.";

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "cast_crabs".to_owned(),
        description: format!(
            "Dispatch a wave of up to {MAX_AGENTS} subagents that work in parallel, each searching \
             the web, reading pages or local files, and calling MCP tools that need no approval \
             for its own task, and return their reports. They cannot spawn subagents or see \
             images, so work that needs a browser, such as a signed-in site or one that blocks \
             plain fetches, you do yourself."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "maxItems": MAX_AGENTS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "role": {"type": "string", "description": "Who the subagent is and how it should work."},
                            "task": {"type": "string", "description": "One self-contained task with all needed context."},
                        },
                        "required": ["task"],
                    },
                },
            },
            "required": ["agents"],
        }),
    }
}

/// What a subagent may use: the web, local files, and MCP servers that need no approval.
#[derive(Debug, Clone, Copy)]
pub struct Tools<'a> {
    pub config: &'a Config,
    pub search: &'a WebSearch,
    pub fetcher: &'a Fetcher,
    pub mcp: &'a Mcp,
}

#[derive(Debug, PartialEq)]
struct Agent {
    name: String,
    role: String,
    task: String,
}

/// Runs one swarm tool call. `now` is the current time, so subagents judge how recent a source
/// is. A subagent's model failure becomes its report; only `io` errors fail the call.
pub async fn run(
    model: &Model,
    tools: Tools<'_>,
    now: &str,
    args: &Value,
    io: &mut impl Io,
) -> Result<String> {
    let first = usize::try_from(RandomState::new().hash_one(()) % CRAB_NAMES.len() as u64)
        .unwrap_or_default();
    let agents = match parse(args, first) {
        Ok(agents) => agents,
        Err(problem) => return Ok(format!("error: {problem}")),
    };
    let total = agents.len();
    io.note(&format!(
        "🦀 cast of crabs: {total} crabs, {CONCURRENCY} at a time"
    ))
    .await?;
    let mut board = Board::new(agents.iter().map(|agent| agent.name.clone()).collect());
    let (steps, mut step_rx) = mpsc::unbounded_channel();
    let mut running = futures::stream::iter(agents.into_iter().enumerate())
        .map(|(index, agent)| {
            let steps = steps.clone();
            async move {
                let step = |text: String| {
                    // The board only goes away after every crab is done, so a send can't fail
                    // in a way that matters.
                    let _ = steps.send((index, Some(text)));
                };
                let report = tokio::time::timeout(
                    CRAB_TIMEOUT,
                    work(model, tools, now, &agent, step),
                )
                .await
                .unwrap_or_else(|_| {
                    format!(
                        "error: this crab timed out after {} minutes and its findings were lost",
                        CRAB_TIMEOUT.as_secs() / 60
                    )
                });
                let _ = steps.send((index, None));
                (index, format!("{}: {}", agent.name, agent.task), report)
            }
        })
        .buffer_unordered(CONCURRENCY);
    let mut redraw = tokio::time::interval(BOARD_EVERY);
    let mut reports = Vec::with_capacity(total);
    let mut changed = false;
    loop {
        tokio::select! {
            finished = running.next() => match finished {
                Some(finished) => reports.push(finished),
                None => break,
            },
            Some((index, step)) = step_rx.recv() => {
                board.set(index, step);
                changed = true;
            }
            _ = redraw.tick(), if changed => {
                io.board(&board.render()).await?;
                changed = false;
            }
        }
    }
    drop(running);
    while let Ok((index, step)) = step_rx.try_recv() {
        board.set(index, step);
    }
    io.board(&board.render()).await?;
    io.board("").await?;
    reports.sort_by_key(|(index, ..)| *index);
    Ok(reports
        .into_iter()
        .map(|(_, name, report)| format!("## {name}\n{}", clip(report.trim())))
        .collect::<Vec<_>>()
        .join("\n\n"))
}

/// Crabs are named from `crab_name(first)` on.
fn parse(args: &Value, first: usize) -> Result<Vec<Agent>, String> {
    let list = args["agents"]
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or("`agents` must be a non-empty array")?;
    if list.len() > MAX_AGENTS {
        return Err(format!(
            "at most {MAX_AGENTS} subagents per call; send the rest in another wave"
        ));
    }
    list.iter()
        .enumerate()
        .map(|(index, agent)| {
            let field = |key: &str| agent[key].as_str().map(str::trim).unwrap_or_default();
            let task = field("task");
            if task.is_empty() {
                return Err(format!("subagent {} has no task", index + 1));
            }
            Ok(Agent {
                name: crab_name(first + index),
                role: field("role").to_owned(),
                task: task.to_owned(),
            })
        })
        .collect()
}

/// One subagent: a small tool loop over its own history, ending in a text report.
/// `step` hears what the crab is doing, for the board.
async fn work(
    model: &Model,
    tools: Tools<'_>,
    now: &str,
    agent: &Agent,
    step: impl Fn(String),
) -> String {
    let system = if agent.role.is_empty() {
        AGENT.to_owned()
    } else {
        format!("{AGENT}\n\nYour role: {}", agent.role)
    };
    let mut messages = vec![Message::user(format!(
        "Today is {now}.\nYour task: {}",
        agent.task
    ))];
    let mut last = String::new();
    for round in 1..=AGENT_STEPS {
        step("💭 thinking".to_owned());
        let response = match model
            .complete(agent_request(tools, &system, &messages))
            .await
        {
            Ok(response) => response,
            Err(err) => return format!("error: model request failed: {err:#}"),
        };
        let mut calls: Vec<ToolCall> = Vec::new();
        last.clear();
        for content in &response.choice {
            match content {
                AssistantContent::Text(text) => last.push_str(text.text()),
                AssistantContent::ToolCall(call) => calls.push(call.clone()),
                _ => {}
            }
        }
        messages.push(Message::Assistant {
            id: response.message_id.clone(),
            content: response.choice,
        });
        if calls.is_empty() {
            return last;
        }
        let mut results = Vec::new();
        // Images can't go inside a tool result on every provider, so they follow the results.
        let mut images = Vec::new();
        for call in &calls {
            let args = &call.function.arguments;
            step(describe(&call.function.name, args));
            let output = match call.function.name.as_str() {
                "web_search" => tools.search.run(args).await,
                "fetch_url" => {
                    let (output, pages) = tools.fetcher.run(args).await;
                    images.extend(pages.into_iter().map(UserContent::Image));
                    output
                }
                "view_pdf" => {
                    let (output, pages) = document::view(tools.config, tools.fetcher, args).await;
                    images.extend(pages.into_iter().map(UserContent::Image));
                    output
                }
                "read_file" => files::read(tools.config, args).await,
                "list_dir" => files::list(tools.config, args).await,
                other => match tools.mcp.call_unapproved(other, args).await {
                    Some(output) => output,
                    None => format!("error: unknown tool `{other}`"),
                },
            };
            results.push(UserContent::tool_result_for(
                call.id.clone(),
                call.provider.clone(),
                call.function.name.clone(),
                vec![ToolResultContent::text(output)],
            ));
        }
        results.extend(images);
        if round + 1 == AGENT_STEPS {
            results.push(UserContent::text(
                "This is your last step: write your report now, without more tool calls.",
            ));
        }
        messages.push(Message::User { content: results });
    }
    format!("{last}\n(stopped at the step limit)")
}

fn agent_request(tools: Tools<'_>, system: &str, messages: &[Message]) -> CompletionRequest {
    let mut request = agent::request(tools.config, system, messages, &[]);
    request.tools = vec![
        search::tool(),
        fetch::tool(),
        files::read_tool(),
        files::list_tool(),
        document::view_tool(),
    ];
    request
        .tools
        .extend(tools.mcp.unapproved_tools().iter().cloned());
    request
}

/// What a tool call is doing, in a few words for the board.
fn describe(tool: &str, args: &Value) -> String {
    let arg = |key: &str| args[key].as_str().unwrap_or_default().to_owned();
    match tool {
        "web_search" => format!("🔎 {}", arg("query")),
        "fetch_url" => format!("🌐 {}", arg("url")),
        "read_file" => format!("📄 {}", arg("path")),
        "list_dir" => format!("📁 {}", arg("path")),
        "view_pdf" => format!("🖼️ {}{}", arg("path"), arg("url")),
        other => format!("🔌 {other}"),
    }
}

/// What every crab in a swarm is doing.
#[derive(Debug)]
struct Board {
    names: Vec<String>,
    /// Each crab's current step: `None` before it starts, `Some(None)` once done.
    steps: Vec<Option<Option<String>>>,
}

impl Board {
    fn new(names: Vec<String>) -> Self {
        let steps = vec![None; names.len()];
        Self { names, steps }
    }

    /// `step` is the crab's new step, or `None` when it is done.
    fn set(&mut self, index: usize, step: Option<String>) {
        if let Some(slot) = self.steps.get_mut(index) {
            *slot = Some(step);
        }
    }

    /// A summary line, then one line per working crab; the rest only count.
    fn render(&self) -> String {
        let done = self
            .steps
            .iter()
            .filter(|s| matches!(s, Some(None)))
            .count();
        let working: Vec<String> = self
            .names
            .iter()
            .zip(&self.steps)
            .filter_map(|(name, step)| {
                let step: String = step
                    .as_ref()?
                    .as_ref()?
                    .chars()
                    .take(MAX_STEP_CHARS)
                    .collect();
                Some(format!("🦀 {name}: {step}"))
            })
            .collect();
        let waiting = self.steps.len() - done - working.len();
        let mut text = format!(
            "🦀 cast of crabs: {done}/{} done, {} working, {waiting} waiting",
            self.steps.len(),
            working.len()
        );
        for line in working {
            text.push('\n');
            text.push_str(&line);
        }
        text
    }
}

/// `report` cut to `MAX_REPORT_CHARS`, marked when cut.
fn clip(report: &str) -> String {
    match report.char_indices().nth(MAX_REPORT_CHARS) {
        Some((end, _)) => format!("{}… (cut)", &report[..end]),
        None => report.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_agents_and_rejects_bad_waves() {
        let agents = parse(
            &json!({"agents": [
                {"role": "analyst", "task": " compare plans "},
                {"task": "check dates"},
            ]}),
            CRAB_NAMES.len() - 1,
        )
        .unwrap_or_default();
        assert_eq!(
            agents,
            [
                Agent {
                    name: "Tenlegs".into(),
                    role: "analyst".into(),
                    task: "compare plans".into()
                },
                Agent {
                    name: "Pincher2".into(),
                    role: String::new(),
                    task: "check dates".into()
                },
            ]
        );
        assert!(parse(&json!({"agents": []}), 0).is_err());
        assert!(parse(&json!({"agents": [{"role": "x"}]}), 0).is_err());
        let too_many = vec![json!({"task": "t"}); MAX_AGENTS + 1];
        assert!(parse(&json!({ "agents": too_many }), 0).is_err());
    }

    #[test]
    fn request_reads_only_a_leading_cast_command() {
        assert_eq!(request("/cast compare plans"), Some("compare plans"));
        assert_eq!(request(" /cast\n list "), Some("list"));
        assert_eq!(request("/cast"), Some(""));
        assert_eq!(request("/castle"), None);
        assert_eq!(request("ask /cast"), None);
    }

    #[test]
    fn board_lists_working_crabs_and_counts_the_rest() {
        let mut board = Board::new(vec!["Pincher".into(), "Snapper".into(), "Clawdia".into()]);
        board.set(0, Some("🔎 llm prices".into()));
        board.set(1, Some("💭 thinking".into()));
        board.set(1, None);
        assert_eq!(
            board.render(),
            "🦀 cast of crabs: 1/3 done, 1 working, 1 waiting\n🦀 Pincher: 🔎 llm prices"
        );
    }

    #[test]
    fn clip_cuts_on_char_boundary() {
        let long = "字".repeat(MAX_REPORT_CHARS + 5);
        let clipped = clip(&long);
        assert!(clipped.ends_with("… (cut)"));
        assert_eq!(
            clipped.chars().count(),
            MAX_REPORT_CHARS + "… (cut)".chars().count()
        );
        assert_eq!(clip("short"), "short");
    }
}
