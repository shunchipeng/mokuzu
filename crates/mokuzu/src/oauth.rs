//! Subscription providers use Rig's OAuth clients and Mokuzu-owned credential files.
//! Only explicit CLI or TUI sign-in may start a device flow; background requests fail promptly.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use rig_core::client::CompletionClient;
use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
};
use rig_core::providers::{chatgpt, copilot};

use crate::config::{Config, Provider};

enum Client {
    Copilot(copilot::Client),
    Chatgpt(chatgpt::Client),
}

// Clones share Rig's authentication mutex. Reviewers, conversation titles, and swarm
// workers must not each refresh the same rotating ChatGPT token independently.
struct CachedClient {
    client: Client,
    // Rig serializes refreshes within a client, but not between processes. Keep one
    // process per credential directory without serializing inference requests.
    _lease: fs::File,
}

type Clients = BTreeMap<(PathBuf, Provider), CachedClient>;
static CLIENTS: OnceLock<Mutex<Clients>> = OnceLock::new();

pub(crate) enum Model {
    Copilot(<copilot::Client as CompletionClient>::CompletionModel),
    Chatgpt(<chatgpt::Client as CompletionClient>::CompletionModel),
}

impl Model {
    pub fn new(config: &Config) -> Result<Self> {
        let dir = credentials_dir(&config.path, config.provider)?;
        let mut clients = CLIENTS
            .get_or_init(Mutex::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("OAuth client cache is unavailable"))?;
        let cached = match clients.entry((dir.clone(), config.provider)) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => entry.insert(CachedClient {
                _lease: lease(&dir)?,
                client: client(config.provider, &dir, false)?,
            }),
        };
        Ok(match &cached.client {
            Client::Copilot(client) => Self::Copilot(client.completion_model(&config.model)),
            Client::Chatgpt(client) => Self::Chatgpt(client.completion_model(&config.model)),
        })
    }

    pub async fn complete(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        // Responses backends cannot replay another provider's assistant-message ID.
        for message in &mut request.chat_history {
            if let rig_core::message::Message::Assistant { id, .. } = message
                && id.as_deref().is_some_and(|id| !id.starts_with("msg"))
            {
                *id = None;
            }
        }
        match self {
            Self::Copilot(model) => model.completion(request).await,
            Self::Chatgpt(model) => model.completion(request).await,
        }
    }
}

fn lease(dir: &Path) -> Result<fs::File> {
    let path = dir.join(".lock");
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (!metadata.is_file() || metadata.file_type().is_symlink())
    {
        bail!("OAuth lock must be a regular file: {}", path.display());
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.try_lock().context("OAuth credentials are already in use; stop the running Mokuzu process before logging in or starting another one")?;
    Ok(file)
}

fn client(provider: Provider, dir: &Path, interactive: bool) -> Result<Client> {
    client_with_prompt(provider, dir, interactive, |url, code| {
        eprintln!("Open {url} and enter {code}");
    })
}

fn client_with_prompt(
    provider: Provider,
    dir: &Path,
    interactive: bool,
    on_prompt: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<Client> {
    Ok(match provider {
        Provider::GithubCopilot => Client::Copilot(
            copilot::Client::builder()
                .oauth()
                .base_url(provider.default_base_url())
                .token_dir(dir)
                .allow_device_flow(interactive)
                .on_device_code(move |prompt| on_prompt(prompt.verification_uri, prompt.user_code))
                .build()
                .context("failed to build GitHub Copilot OAuth client")?,
        ),
        Provider::Chatgpt => Client::Chatgpt(
            chatgpt::Client::builder()
                .oauth()
                .base_url(provider.default_base_url())
                .token_dir(dir)
                .originator("mokuzu")
                .default_instructions("")
                .user_agent(concat!("mokuzu/", env!("CARGO_PKG_VERSION")))
                .allow_device_flow(interactive)
                .on_device_code(move |prompt| on_prompt(prompt.verification_uri, prompt.user_code))
                .build()
                .context("failed to build ChatGPT OAuth client")?,
        ),
        _ => bail!("{} does not use OAuth", provider.section()),
    })
}

/// Keep credentials beside the chosen config, never in Rig's shared default cache.
/// Rig overwrites these files during renewal, so precreate them with mode 0600.
fn credentials_dir(config: &Path, provider: Provider) -> Result<PathBuf> {
    if !provider.uses_oauth() {
        bail!("{} does not use OAuth", provider.section());
    }
    let parent = config
        .parent()
        .context("config path has no parent directory")?;
    let root = parent.join("oauth");
    let dir = root.join(provider.section());
    for path in [&root, &dir] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "OAuth credential directory must be a real directory: {}",
                path.display()
            );
        }
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let files: &[(&str, &[u8])] = match provider {
        Provider::GithubCopilot => &[("access-token", b""), ("api-key.json", b"{}")],
        Provider::Chatgpt => &[("auth.json", b"{}")],
        _ => unreachable!(),
    };
    for (name, initial) in files {
        let path = dir.join(name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(initial)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!(
                        "OAuth credential file must be a regular file: {}",
                        path.display()
                    );
                }
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
    Ok(fs::canonicalize(dir)?)
}

pub async fn login(config: &Path, provider: Provider) -> Result<()> {
    login_with_prompt(config, provider, |url, code| {
        eprintln!("Open {url} and enter {code}");
    })
    .await?;
    eprintln!(
        "Signed in to {}. Select it with `mokuzu configure`.",
        provider.section()
    );
    Ok(())
}

pub async fn login_with_prompt(
    config: &Path,
    provider: Provider,
    on_prompt: impl Fn(String, String) + Send + Sync + 'static,
) -> Result<()> {
    let dir = credentials_dir(config, provider)?;
    let _lease = lease(&dir)?;
    match client_with_prompt(provider, &dir, true, on_prompt)? {
        Client::Copilot(client) => client
            .authorize()
            .await
            .context("GitHub Copilot login failed")?,
        Client::Chatgpt(client) => client.authorize().await.context(
            "ChatGPT login failed; enable device code authorization in ChatGPT settings if needed",
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::message::{AssistantContent, Message};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn config_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "mokuzu-oauth-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("config.toml")
    }

    #[test]
    fn credentials_are_private_and_isolated_and_existing_tokens_survive() {
        let config = config_path();
        let copilot = credentials_dir(&config, Provider::GithubCopilot).unwrap();
        let chatgpt = credentials_dir(&config, Provider::Chatgpt).unwrap();
        assert_ne!(copilot, chatgpt);
        assert_eq!(
            fs::metadata(&copilot).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let token = chatgpt.join("auth.json");
        fs::write(&token, "saved credentials").unwrap();
        fs::set_permissions(&token, fs::Permissions::from_mode(0o644)).unwrap();
        credentials_dir(&config, Provider::Chatgpt).unwrap();
        assert_eq!(fs::read_to_string(&token).unwrap(), "saved credentials");
        assert_eq!(
            fs::metadata(token).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn credential_symlinks_are_rejected() {
        let config = config_path();
        let dir = credentials_dir(&config, Provider::Chatgpt).unwrap();
        let target = dir.join("outside");
        fs::write(&target, "private").unwrap();
        fs::remove_file(dir.join("auth.json")).unwrap();
        std::os::unix::fs::symlink(&target, dir.join("auth.json")).unwrap();
        assert!(credentials_dir(&config, Provider::Chatgpt).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "private");
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_credential_directory_cannot_have_two_refresh_owners() {
        let config = config_path();
        let dir = credentials_dir(&config, Provider::Chatgpt).unwrap();
        let guard = lease(&dir).unwrap();
        assert!(lease(&dir).is_err());
        drop(guard);
        assert!(lease(&dir).is_ok());
        fs::remove_dir_all(config.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn background_clients_fail_without_starting_a_device_flow() {
        crate::install_tls_provider();
        for provider in [Provider::GithubCopilot, Provider::Chatgpt] {
            let config = config_path();
            let dir = credentials_dir(&config, provider).unwrap();
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                match client(provider, &dir, false).unwrap() {
                    Client::Copilot(client) => client.authorize().await.is_err(),
                    Client::Chatgpt(client) => client.authorize().await.is_err(),
                }
            })
            .await
            .expect("a service must not wait for an interactive login");
            assert!(result);
            fs::remove_dir_all(config.parent().unwrap()).unwrap();
        }
    }

    /// A local HTTP peer exercises Rig's real request conversion and reply parsing.
    async fn reply_server(
        body: String,
        sse: bool,
    ) -> (String, tokio::task::JoinHandle<(String, Value)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let (headers, body_start) = loop {
                let mut chunk = [0; 8192];
                let read = stream.read(&mut chunk).await.unwrap();
                assert!(read > 0, "HTTP request was cut short");
                bytes.extend_from_slice(&chunk[..read]);
                assert!(bytes.len() < 1_000_000);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break (headers, end + 4);
                    }
                }
            };
            let request = serde_json::from_slice(&bytes[body_start..]).unwrap();
            let content_type = if sse {
                "text/event-stream"
            } else {
                "application/json"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            (headers, request)
        });
        (url, task)
    }

    #[tokio::test]
    async fn both_subscription_transports_return_tool_calls_for_mokuzu_to_approve() {
        crate::install_tls_provider();
        for (chatgpt, responses) in [(false, false), (false, true), (true, true)] {
            let tool = json!({"name": "read_file", "arguments": "{\"path\":\"/tmp/note\"}"});
            let responses_reply = json!({
                "id": "resp_test", "object": "response", "created_at": 1,
                "status": "completed", "model": "gpt-5.3-codex",
                "output": [{"type": "function_call", "id": "fc_test", "call_id": "call_test",
                    "name": tool["name"], "arguments": tool["arguments"], "status": "completed"}],
                "tools": [], "usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}
            });
            let response = if chatgpt {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({
                        "type": "response.completed",
                        "response": responses_reply
                    })
                )
            } else if responses {
                responses_reply.to_string()
            } else {
                json!({"id": "chatcmpl-test", "object": "chat.completion", "created": 1, "model": "claude-sonnet-4.6",
                    "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                        "role": "assistant", "content": null,
                        "tool_calls": [{"id": "call_test", "type": "function", "function": tool}]
                    }}], "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
                }).to_string()
            };
            let (url, server) = reply_server(response, chatgpt).await;
            let model = if chatgpt {
                Model::Chatgpt(
                    chatgpt::Client::builder()
                        .api_key(chatgpt::ChatGPTAuth::AccessToken {
                            access_token: "test-token".to_owned(),
                            account_id: None,
                        })
                        .base_url(&url)
                        .default_instructions("")
                        .build()
                        .unwrap()
                        .completion_model("gpt-5.3-codex"),
                )
            } else {
                Model::Copilot(
                    copilot::Client::builder()
                        .api_key("test-token")
                        .base_url(&url)
                        .build()
                        .unwrap()
                        .completion_model(if responses {
                            "gpt-5.3-codex"
                        } else {
                            "claude-sonnet-4.6"
                        }),
                )
            };
            let config = Config::parse("[opencode-go]\napi_key = \"unused\"\n").unwrap();
            let mut previous = Message::assistant("Earlier answer");
            if let Message::Assistant { id, .. } = &mut previous {
                *id = Some("foreign-message-id".to_owned());
            }
            let request = crate::agent::request(
                &config,
                "Mokuzu owns tools",
                &[previous, Message::user("Read the note")],
                &[],
            );
            let tool_count = request.tools.len();
            let response =
                tokio::time::timeout(std::time::Duration::from_secs(5), model.complete(request))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
                response.provider,
                if chatgpt { "chatgpt" } else { "copilot" }
            );
            assert!(response.choice.iter().any(|content| matches!(content,
                AssistantContent::ToolCall(call) if call.function.name == "read_file"
                    && call.function.arguments["path"] == "/tmp/note")));
            let (headers, body) = server.await.unwrap();
            assert!(headers.starts_with(if responses {
                "POST /responses "
            } else {
                "POST /chat/completions "
            }));
            assert_eq!(body["tools"].as_array().unwrap().len(), tool_count);
            if chatgpt {
                assert_eq!(body["stream"], true);
                assert_eq!(body["store"], false);
                assert_eq!(body["instructions"], "Mokuzu owns tools");
                assert!(body.get("max_output_tokens").is_none());
                assert!(!body.to_string().contains("foreign-message-id"));
            }
        }
    }
}
