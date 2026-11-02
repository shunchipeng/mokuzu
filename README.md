<p align="center">
  <img src="assets/icon.png" alt="mokuzu" width="200">
</p>

# mokuzu

A personal agent that runs on your machine and talks with you over Discord, in text and in voice
channels. It reads files and web pages, searches the web, remembers what you tell it, calls tools on MCP
servers such as Linear, and runs scheduled jobs.

The name is モクズ (mokuzu), from モクズガニ, the Japanese mitten crab; its hairy claws look like
they hold a tuft of seaweed scraps (藻屑).

Mokuzu's own tools only read, except that it can write files, and run code in a sandbox, in its
own workspace folder.
Anything else that changes something (an MCP call or a settings change) asks you first, or goes
past a reviewer model in auto mode.

## Install

Linux on arm64 (glibc 2.35 or newer), e.g. a Raspberry Pi. Elsewhere, build from source with
`cargo install --git https://github.com/xianyuntang/mokuzu mokuzu`.

```sh
curl -fsSL https://raw.githubusercontent.com/xianyuntang/mokuzu/main/install.sh | sh
```

The script downloads the release for your platform, checks its SHA-256, and puts `mokuzu` in
`~/.local/bin`. Options:

| Variable | Default | Meaning |
| --- | --- | --- |
| `MOKUZU_VERSION` | latest | Release tag to install, e.g. `v0.1.0` |
| `MOKUZU_INSTALL_DIR` | `~/.local/bin` | Where the binary goes |

To upgrade later, run `mokuzu update`. It reinstalls the latest release over the current binary
and restarts the background service if one is installed.

### Upgrading from mitten

Mokuzu was called mitten before 0.5.0. Run `mitten update`, or the install command above, and
`mitten` becomes another name for `mokuzu`. Its first start moves `~/.config/mitten` and
`~/.local/share/mitten` to the mokuzu paths, and replaces mitten's background service with
mokuzu's. Installs made with `cargo install` need `cargo install` again and one `mokuzu` run.

Or build from source with Rust 1.95 or newer:

```sh
cargo install --git https://github.com/xianyuntang/mokuzu mokuzu
```

## Quick start

```sh
mokuzu configure   # full-screen setup; ←/→ switch pages, Save on any page
mokuzu install     # run the Discord bot in the background
```

`mokuzu configure` writes `~/.config/mokuzu/config.toml`. Its **Discord** page takes the bot token
and your user ID (see [Discord](#discord)), and its **Model** page your provider, API key, and
model: OpenCode Go, or Anthropic's API for Claude with an Anthropic API key. Each provider keeps
its own key, so switching between them never reuses one. Those two pages are required; voice
calls, web search, MCP servers, and the rest are optional. `mokuzu` alone runs the bot in the
foreground, as `mokuzu serve` does.

## Discord

Mokuzu can answer you in Discord, by DM or in any server channel it can read.

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications),
   add a bot, and copy its token.
2. Under **Bot**, turn on **Message Content Intent**. Without it the bot cannot connect.
3. Invite the bot to your server with the permissions **View Channels**, **Send Messages**,
   **Send Messages in Threads**, **Create Public Threads**, **Read Message History**,
   **Add Reactions**, **Attach Files**, and **Embed Links**, plus **Connect** and **Speak** for
   voice calls.
4. Find your Discord user ID: turn on **Developer Mode** in Discord's settings, then right-click
   your name and choose **Copy User ID**.
5. Run `mokuzu configure`, open the **Discord** page, and enter the token and your user ID.
6. Run `mokuzu install` to keep the bot running in the background.

How it behaves:

- Only the user IDs you list are heard; everyone else is ignored.
- Each server can have its own list instead, set under **Add server** on the Discord page of
  `mokuzu configure`: the server ID, its user IDs (or `*` for everyone there), and whether it
  replies to every message or only when @mentioned. In config, that is
  `[discord.guilds.<server ID>]` with `allowed_users` (a list, or `"*"`) and `mention_only = true`.
  Other servers and DMs use the main list. In a server open to everyone, only the main list can
  approve actions, but anyone there can make the bot read files on this machine, so open a server
  only if you trust everyone in it. With `mention_only`, messages in the bot's threads need a
  mention too. Messages it skips in its threads, from anyone, are still saved to that thread's
  conversation, marked as overheard chat to use as context and never as instructions. To copy a
  server ID, right-click the server with Developer Mode on.
- A message in a server channel opens a thread, and each thread is its own conversation.
  Idle threads archive after an hour.
- Each person has their own memory, shared by all of their threads, DMs, and voice calls.
  Messages from different people are never merged into one turn.
- 👀 means received, ✅ done, ❌ failed. `/cast <request>` sends out a cast of crabs, `/browser` gives a link to a live view of mokuzu's desktop, and `/stop` cuts short what mokuzu is doing in that conversation (💤 if nothing is running). They show up in Discord's `/` menu, and typing them as text works too.
- Messages sent in quick succession, or while mokuzu is still answering, are merged and answered
  once.
- Images, text files (including the `message.txt` Discord makes from a long paste), and PDF,
  Word (`.docx`), and PowerPoint (`.pptx`) documents you attach go to the model; documents as
  their text, up to 50,000 characters. A PDF's first 3 pages also go as images, and the model can
  look at any other page with `view_pdf`, so it sees the tables, figures, and scanned pages its
  text misses. With `[voice]` set, voice messages and other audio files go as their transcript,
  made with OpenAI's `gpt-4o-mini-transcribe` in the call `language`.
- Approvals show up as **Run** and **Deny** buttons.

### Voice calls

With a `[voice]` section (an OpenAI API key; set it on the **Voice** page of `mokuzu
configure`), send `/voice` in a server while you are in one of its voice channels: mokuzu joins
and talks with you through OpenAI's Realtime API. The realtime model keeps the conversation going
and hands anything past small talk to mokuzu's agent, which answers in the channel you sent
`/voice` from, as text too, with approvals as buttons there; the call reads the answer out.
`/hangup`, leaving the channel, or telling mokuzu it can go ends the call. You can talk over a
reply to cut it short.

List voice channel IDs in `channels` to skip `/voice` there: mokuzu joins as soon as someone
allowed to talk to it joins one, and the agent answers in that channel's text chat.

The bot needs the **Connect** and **Speak** permissions in that voice channel. Discord's voice
end-to-end encryption (DAVE), which Discord requires, is new in the library mokuzu uses, and a
join sometimes cannot hear you; mokuzu rejoins on its own when it has not heard you within 15
seconds, until it can. If it cannot be heard, leave and join again. Audio is billed by OpenAI per
minute of the call.

## Running in the background

```sh
mokuzu install     # launchd on macOS, a systemd user service on Linux
mokuzu restart     # after editing the config by hand
mokuzu uninstall
```

The service runs `mokuzu serve` and restarts it if it exits. `mokuzu install` records your
shell's `PATH`, so `npx` and `uvx` resolve the way they do in your terminal, whether
they come from Homebrew, asdf, nvm, or elsewhere. `mokuzu update` restarts it for you; run
`mokuzu install` again after installing new tools.

Logs:

- macOS: `~/Library/Logs/mokuzu.log`
- Linux: `journalctl --user -u mokuzu -f`

## Tools

| Tool | What it does | Needs approval |
| --- | --- | --- |
| `read_file`, `list_dir` | Reads files, including the text of PDF, `.docx`, and `.pptx` documents up to 20 MB, and lists directories. Hidden paths (like `~/.ssh` or `.env`) and Mokuzu's own config and database are refused. | No |
| `fetch_url` | Reads a web page as text, or a linked PDF, `.docx`, or `.pptx` document up to 20 MB; a PDF's first 3 pages also come as images. Private network addresses are refused. | No |
| `view_pdf` | Shows the model pages of a local or linked PDF as images, up to 10 per call, for layout, tables, figures, and scans that the text misses. Same path and address rules as `read_file` and `fetch_url`. | No |
| `write_file` | Writes or appends to a text file in mokuzu's workspace (`workspace/` next to the database), the only place it can write; `read_file`, `list_dir`, and `send_file` reach it too, so it can hand you a CSV or report it made. | No |
| `run_code` | Runs Python 3 or shell code in a [bubblewrap](https://github.com/containers/bubblewrap) sandbox, for calculations, data analysis, file conversion, and charts. The sandbox sees the system's programs read-only, writes only to the workspace, and has no network and no view of your home folder or other processes; runs are killed after 60 seconds (up to 10 minutes if the model asks), and limited to 4 GB of memory. Python packages must be installed system-wide (e.g. `apt install python3-pandas python3-matplotlib`). Linux only: offered when `bwrap` is installed (`apt install bubblewrap`) and can make a sandbox, checked when `mokuzu serve` starts. | No |
| `send_file` | Sends a local file or a downloaded one (under 3 MB) as an attachment, e.g. an image the user asked to see. Same path and address rules as `read_file` and `fetch_url`. | No |
| `web_search` | Searches through your SearXNG instance or the Brave Search API (pick one in `mokuzu configure`), optionally only recent results (`time_range`), news sites (`category`), or a date span (`after`, `before`; on SearXNG only Google engines honor it). Results show their publish date when known. | No |
| `cast_crabs` | Cast of crabs mode (螃蟹群; a group of crabs is a cast, and mokuzu is モクズガニ, a mitten crab), modeled on Kimi's Agent Swarm and used when the model judges a task big enough, or whenever a message starts with `/cast <request>`. The model acts as an orchestrator and dispatches up to 10 named crabs per call, each with its own role and task, all running at once, while a live board shows what each one is doing. Crabs use only the read-only tools above, plus tools of MCP servers set to `approve = false`, and report back; the model sends more waves as needed and writes the answer. Needs web search configured. | No |
| `memory` | Saves short notes about the person talking, loaded into their later conversations. | No |
| `todo` | Keeps a todo list per person of things to follow until a condition is met. The agent adds them on its own when it starts something whose outcome isn't in yet (after opening a PR it follows the CI/CD, review, or merge; after a deploy or long job, its end; after sending a message, the reply), or when you ask it to watch something. Under `mokuzu serve`, a heartbeat wakes every 30 minutes for each Discord conversation with open todos, checks them in a fresh conversation, and posts there only when one is met, has failed, or needs you, then marks it done; otherwise it stays silent. No open todos, no model call. Anything at a set time (a reminder in 5 minutes, a task tomorrow) is a `cron` job instead. | No |
| `search_history` | Searches messages that are no longer in context: earlier ones of the same conversation that a summary replaced, and the person's own messages and the replies to them in their other conversations (threads, DMs, voice calls, scheduled runs). Other people's messages stay out of it. Matches what the user and the model wrote, not tool output; Chinese and Japanese work without word splitting. Messages saved before 0.5.32 are found only in their own conversation. | No |
| `skill` | Loads, saves, or removes skills: saved instructions for recurring tasks, kept in the database, one set per person like memory. Their names and descriptions are listed in each of that person's conversations; the model loads one when a task comes close to one, saves a finished multi-step task as a new one, and updates a skill when following it went wrong or you corrected it. Saving runs without asking and says what it saved. | Remove |
| `cron` | Schedules a named prompt to run later, once (`at`) or on a cron expression, in the config's `timezone` (restart `mokuzu serve` after changing it). Each run starts a fresh conversation and posts its reply in the Discord channel or thread where the job was made. Discord only; jobs run while `mokuzu serve` is up, and a run missed while it was down happens once on startup. Approval prompts during a run are denied unless approval is set to auto. A job's instructions are shown when it is made or changed; ask to change its instructions or timing, remove it, or give it a trial run now (posted where the job was made, schedule unchanged). | No |
| `webhook` | With `[webhook]` set, makes a named webhook: a secret URL, `http://<this machine>:<port>/hook/<secret>`, that outside services POST to. Each request runs the webhook's instructions with the request body (up to 64 KB, given to the model as untrusted data) in a fresh conversation, and posts the reply where the webhook was made. Requests to one webhook run one at a time. Approval prompts during a run are denied, even in auto mode. The port is on your local network; put a tunnel in front of it for services on the internet. Discord only. | No |
| `settings` | Reads or changes the model and a few limits. | Yes |
| MCP tools | Any tool from the MCP servers you connect. | Yes, per server |

### MCP servers

Add servers on the **MCP** page of `mokuzu configure`, or in the config. Their tools reach the
model as `<server>__<tool>`.

```toml
# A remote server. Linear accepts a personal API key as the bearer token.
[mcp.servers.linear]
url = "https://mcp.linear.app/mcp"
token = "lin_api_..."

# A local server started with npx.
[mcp.servers.github]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "ghp_..." }
approve = true   # ask before each call
```

Servers connect once at startup and are shared by every conversation. Restart after changing
them.

### Browser

Mokuzu has no browser of its own; give it one over MCP. [invisible-playwright-mcp](https://github.com/feder-cr/invisible_playwright_mcp)
drives a Firefox patched so sites see an ordinary browser, and keeps sign-ins in a profile:

```sh
uvx --from invisible-playwright invisible-playwright fetch   # downloads its Firefox once
```

```toml
[mcp.servers.stealth]
command = "uvx"
args = ["invisible-playwright-mcp"]
env = { STEALTHFOX_HEADLESS = "0", STEALTHFOX_PROFILE_DIR = "/home/you/.local/share/mokuzu/stealthfox" }
```

On Linux without a screen, mokuzu starts a virtual Xvfb desktop (install `xvfb`) and gives local
MCP servers its `DISPLAY` unless their `env` sets one, so a browser opened headed draws there.
`/browser` replies with a link to a live view of that desktop on your local network (port 22050,
or `[browser]` `viewer_port`), to watch; nothing done there reaches the browser. The view needs
`ffmpeg`. `[browser]` `width` and `height` set the desktop's size.

## Approval

Every action that needs approval shows you what will run. There are two modes:

- **Ask**: you approve each action.
- **Auto**: a reviewer model reads your request and the proposed action. Reading, and small
  changes you asked for, go ahead with a 🛡️ note. Anything that deletes, publishes, messages
  other people, changes permissions, touches many items, or that the reviewer is unsure about is
  put to you with its reason. A failed review also falls back to asking.

```toml
[approval]
mode = "auto"
model = "glm-5.3-flash"   # the reviewer; can be the main model
```

The model cannot turn on auto mode itself; only `mokuzu configure` or the config file can.

## Configuration

Everything lives in `~/.config/mokuzu/config.toml` (or pass `--config PATH`).
[`config.example.toml`](config.example.toml) documents every option, and `mokuzu configure`
can set each of them (the less common ones under **Advanced**). The file spells out every
setting; nothing falls back to a hidden default. Its `version` is the file's format: when an
update changes the format, mokuzu migrates the file on start, keeping the old one as
`config.toml.bak`. MCP servers you added by hand are kept as written. Most changes apply from the next
message; MCP servers, web search, and Discord need a restart.

## Development

```sh
cargo test                    # unit tests
cargo test -- --ignored       # also the tests that call real services (npx, your model)
cargo clippy --all-targets -- -D warnings
```

Pushing a `v*` tag that matches the version in `crates/mokuzu/Cargo.toml` builds every platform
and publishes a GitHub release.

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
