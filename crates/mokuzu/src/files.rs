//! File tools: `read_file`, `list_dir`, and `send_file` read, and `write_file` writes only inside
//! Mokuzu's workspace folder. They run without approval, so they refuse hidden paths (any
//! component starting with `.`, like `~/.ssh` or `.env`) and Mokuzu's own config and database,
//! which hold API keys, tokens, and every conversation. The workspace sits next to the database,
//! itself under a hidden folder, so paths inside it are checked only below it.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rig_core::completion::ToolDefinition;
use serde_json::{Value, json};

use crate::config::Config;
use crate::document;
use crate::fetch::{self, Fetcher};

/// Characters returned per `read_file` call; `offset` pages through the rest.
const MAX_CHARS: usize = 15_000;
/// Largest text file `read_file` opens; documents may be up to `document::MAX_BYTES`.
const MAX_BYTES: u64 = 5_000_000;
/// Entries listed per `list_dir` call.
const MAX_ENTRIES: usize = 500;
/// Largest `content` one `write_file` call takes.
const MAX_WRITE_BYTES: usize = 1_000_000;

/// The folder `write_file` writes into: `workspace` next to the database.
pub fn workspace(config: &Config) -> PathBuf {
    config
        .database_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("workspace")
}

pub fn write_tool() -> ToolDefinition {
    ToolDefinition {
        name: "write_file".to_owned(),
        description: "Write a text file in your workspace folder, the only place you can write: \
             notes, drafts, CSV, Markdown, HTML, code. `path` is relative to the workspace, \
             like `reports/prices.csv`; folders are made as needed. Overwrites the file unless \
             `append` is true. read_file, list_dir, and send_file reach it by the absolute path \
             this returns, e.g. to send the user what you wrote."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path inside the workspace."},
                "content": {"type": "string", "description": "The text to write."},
                "append": {
                    "type": "boolean",
                    "description": "Add to the end of the file instead of replacing it. Default false.",
                },
            },
            "required": ["path", "content"],
        }),
    }
}

/// Runs one `write_file` call; failures come back as text for the model.
pub async fn write(config: &Config, args: &Value) -> String {
    match write_text(config, args).await {
        Ok(out) => out,
        Err(err) => format!("error: {err:#}"),
    }
}

async fn write_text(config: &Config, args: &Value) -> Result<String> {
    let relative = args["path"]
        .as_str()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .context("`path` is required")?;
    let content = args["content"].as_str().context("`content` is required")?;
    if content.len() > MAX_WRITE_BYTES {
        bail!("`content` is over {MAX_WRITE_BYTES} bytes");
    }
    let relative = Path::new(relative);
    // Only plain names, so the path can't climb out of the workspace or name a hidden file.
    if !relative
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
        || is_hidden(relative)
    {
        bail!("`path` must be relative to the workspace, without `..` or hidden names");
    }
    let root = workspace(config);
    let path = root.join(relative);
    let parent = path.parent().context("`path` names no file")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("cannot create {}", parent.display()))?;
    // Canonical, so a symlink inside the workspace can't lead the write elsewhere.
    let root = tokio::fs::canonicalize(&root).await?;
    if !tokio::fs::canonicalize(parent).await?.starts_with(&root) {
        bail!("{} leads out of the workspace", relative.display());
    }
    let append = args["append"].as_bool().unwrap_or(false);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(&path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    tokio::io::AsyncWriteExt::write_all(&mut file, content.as_bytes())
        .await
        .with_context(|| format!("cannot write {}", path.display()))?;
    let verb = if append { "appended to" } else { "wrote" };
    Ok(format!(
        "{verb} {} ({} bytes)",
        path.display(),
        content.len()
    ))
}

pub fn read_tool() -> ToolDefinition {
    ToolDefinition {
        name: "read_file".to_owned(),
        description: format!(
            "Read a text file on the user's machine. Returns up to {MAX_CHARS} characters; for longer \
             files, call again with the `offset` it gives you. Paths may be absolute, start with ~/, \
             or be relative to the working directory. Hidden files and directories are refused."
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to read."},
                "offset": {
                    "type": "integer",
                    "description": "Character offset to continue from, for long files. Default 0.",
                },
            },
            "required": ["path"],
        }),
    }
}

pub fn list_tool() -> ToolDefinition {
    ToolDefinition {
        name: "list_dir".to_owned(),
        description: format!(
            "List a directory on the user's machine: subdirectories end with /, files show their \
             size. Up to {MAX_ENTRIES} entries; hidden entries are left out."
        ),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "The directory to list."}},
            "required": ["path"],
        }),
    }
}

pub fn send_tool() -> ToolDefinition {
    ToolDefinition {
        name: "send_file".to_owned(),
        description: "Send the user a file as a chat attachment, e.g. an image they asked to see. \
             Give exactly one of `path` (a file on the user's machine) or `url` (a public file to \
             download and attach). For a public image, pasting its direct URL in the reply already \
             shows it inline; use `url` only when the link would not display, like a hotlinked image."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "The file to send."},
                "url": {"type": "string", "description": "The file to download and send."},
            },
        }),
    }
}

/// The file a `send_file` call names: a file name for the attachment, and its bytes.
pub async fn load(config: &Config, fetcher: &Fetcher, args: &Value) -> Result<(String, Vec<u8>)> {
    let (name, bytes) = match (args["path"].as_str(), args["url"].as_str()) {
        (Some(_), None) => {
            let (path, bytes) = read_bytes(config, &args["path"]).await?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned());
            (name, bytes)
        }
        (None, Some(url)) => {
            let (url, _, bytes) = fetcher.download(url.trim()).await?;
            if bytes.len() >= fetch::MAX_BYTES {
                bail!("the file is over {} bytes", fetch::MAX_BYTES);
            }
            (url_name(&url), bytes)
        }
        _ => bail!("give exactly one of `path` or `url`"),
    };
    Ok((name.unwrap_or_else(|| "file".to_owned()), bytes))
}

/// The last path segment of `url`, e.g. `cat.png`, if it has one.
fn url_name(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let name = url.path_segments()?.next_back()?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Runs one `read_file` call; failures come back as text for the model.
pub async fn read(config: &Config, args: &Value) -> String {
    let offset = args["offset"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    match read_text(config, &args["path"]).await {
        Ok((path, text)) => page(&path, &text, offset),
        Err(err) => format!("error: {err:#}"),
    }
}

/// Runs one `list_dir` call; failures come back as text for the model.
pub async fn list(config: &Config, args: &Value) -> String {
    match list_entries(config, &args["path"]).await {
        Ok(text) => text,
        Err(err) => format!("error: {err:#}"),
    }
}

async fn read_text(config: &Config, path: &Value) -> Result<(PathBuf, String)> {
    let (path, bytes) = read_bytes(config, path).await?;
    if document::is_document(&bytes) {
        let text = document::extract(bytes)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        return Ok((path, text));
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not a text file", path.display()))?;
    Ok((path, text))
}

/// The file at the `path` argument, if the tools may open it and it is within the size limit.
pub async fn read_bytes(config: &Config, path: &Value) -> Result<(PathBuf, Vec<u8>)> {
    let path = resolve(config, path).await?;
    let size = tokio::fs::metadata(&path).await?.len();
    let limit = if document::has_document_name(&path.to_string_lossy()) {
        document::MAX_BYTES as u64
    } else {
        MAX_BYTES
    };
    if size > limit {
        bail!(
            "{} is {size} bytes, over the {limit}-byte limit",
            path.display()
        );
    }
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok((path, bytes))
}

async fn list_entries(config: &Config, path: &Value) -> Result<String> {
    let path = resolve(config, path).await?;
    let mut dir = tokio::fs::read_dir(&path)
        .await
        .with_context(|| format!("cannot list {}", path.display()))?;
    let mut entries = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        entries.push(match entry.metadata().await {
            Ok(meta) if meta.is_dir() => format!("{name}/"),
            Ok(meta) => format!("{name}  ({} bytes)", meta.len()),
            Err(_) => name,
        });
    }
    entries.sort();
    let total = entries.len();
    entries.truncate(MAX_ENTRIES);
    let mut out = format!(
        "{}: {total} entries\n{}",
        path.display(),
        entries.join("\n")
    );
    if total > MAX_ENTRIES {
        out.push_str(&format!("\n[{} more not shown]", total - MAX_ENTRIES));
    }
    Ok(out)
}

/// The canonical form of the `path` argument, if the tools may open it.
async fn resolve(config: &Config, path: &Value) -> Result<PathBuf> {
    let Some(path) = path.as_str().map(str::trim).filter(|p| !p.is_empty()) else {
        bail!("`path` is required");
    };
    let path = match path.strip_prefix("~/").or((path == "~").then_some("")) {
        Some(rest) => std::env::home_dir()
            .context("cannot find home directory")?
            .join(rest),
        None => PathBuf::from(path),
    };
    // Canonical, so `..` and symlinks can't reach around the checks below.
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot open {}", path.display()))?;
    let workspace = tokio::fs::canonicalize(workspace(config)).await.ok();
    let checked = workspace
        .as_deref()
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(&path);
    if is_hidden(checked) {
        bail!("{} is hidden; hidden paths are refused", path.display());
    }
    for secret in [&config.path, &config.database_path] {
        if let Ok(secret) = tokio::fs::canonicalize(secret).await
            && secret == path
        {
            bail!("{} is Mokuzu's own config or database", path.display());
        }
    }
    Ok(path)
}

fn is_hidden(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => name.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn page(path: &Path, text: &str, offset: usize) -> String {
    let total = text.chars().count();
    if total > 0 && offset >= total {
        return format!("error: offset {offset} is past the end ({total} characters)");
    }
    let chunk: String = text.chars().skip(offset).take(MAX_CHARS).collect();
    let end = offset + chunk.chars().count();
    let mut out = format!(
        "File: {}\nCharacters {offset}-{end} of {total}\n\n{chunk}",
        path.display()
    );
    if end < total {
        out.push_str(&format!(
            "\n\n[{} more characters; call read_file again with offset={end}]",
            total - end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_components_are_detected() {
        assert!(is_hidden(Path::new("/home/me/.ssh/id_ed25519")));
        assert!(is_hidden(Path::new("/srv/app/.env")));
        assert!(!is_hidden(Path::new("/home/me/notes/todo.md")));
    }

    #[test]
    fn url_names_come_from_the_last_segment() {
        assert_eq!(
            url_name("https://example.com/img/cat.png?w=200").as_deref(),
            Some("cat.png")
        );
        assert_eq!(url_name("https://example.com/"), None);
        assert_eq!(url_name("https://example.com"), None);
    }

    #[tokio::test]
    async fn tools_refuse_hidden_and_own_files_and_page_long_ones() {
        let dir = std::env::temp_dir().join(format!("mokuzu-files-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("create dirs");
        std::fs::write(dir.join("long.txt"), "é".repeat(MAX_CHARS + 5)).expect("write");
        std::fs::write(dir.join("config.toml"), "secret").expect("write");
        std::fs::write(dir.join("sub/.env"), "secret").expect("write");
        let config = Config {
            path: dir.join("config.toml"),
            ..Config::parse("[opencode-go]\napi_key = \"k\"\n").expect("valid")
        };
        let path = |p: &str| json!({ "path": dir.join(p) });

        let first = read(&config, &path("long.txt")).await;
        assert!(first.contains(&format!("offset={MAX_CHARS}")), "{first}");
        let args = json!({ "path": dir.join("long.txt"), "offset": MAX_CHARS });
        assert!(read(&config, &args).await.ends_with("ééééé"));
        assert!(
            read(&config, &path("config.toml"))
                .await
                .starts_with("error:")
        );
        assert!(read(&config, &path("sub/.env")).await.starts_with("error:"));
        assert!(
            read(&config, &path("sub/../sub/.env"))
                .await
                .starts_with("error:")
        );

        let listing = list(&config, &path("")).await;
        assert!(
            listing.contains("sub/") && listing.contains("long.txt"),
            "{listing}"
        );
        assert!(!list(&config, &path("sub")).await.contains(".env"));
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[tokio::test]
    async fn write_stays_in_the_workspace_and_reads_back() {
        let dir = std::env::temp_dir()
            .join(format!("mokuzu-write-{}", std::process::id()))
            .join(".hidden");
        let config = Config {
            database_path: dir.join("mokuzu.db"),
            ..Config::parse("[opencode-go]\napi_key = \"k\"\n").expect("valid")
        };
        let write_args =
            |p: &str, append: bool| json!({ "path": p, "content": "hi\n", "append": append });

        let out = write(&config, &write_args("notes/a.md", false)).await;
        assert!(out.starts_with("wrote "), "{out}");
        write(&config, &write_args("notes/a.md", true)).await;
        let file = workspace(&config).join("notes/a.md");
        // Readable though the workspace sits under a hidden folder.
        assert!(
            read(&config, &json!({ "path": file }))
                .await
                .ends_with("hi\nhi\n")
        );
        for bad in ["../escape.md", "/tmp/abs.md", "notes/.env", ""] {
            assert!(
                write(&config, &write_args(bad, false))
                    .await
                    .starts_with("error:"),
                "{bad}"
            );
        }
        std::fs::remove_dir_all(dir.parent().expect("parent")).expect("clean up");
    }
}
