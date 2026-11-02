//! SQLite storage: migrations from `migrations/` run by rusqlite_migration (tracked by
//! `PRAGMA user_version`), hand-written entities.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use include_dir::{Dir, include_dir};
use rusqlite::{Connection, OptionalExtension as _, Row, params};
use rusqlite_migration::Migrations;
use serde_json::Value;

use crate::cron::NewJob;
use crate::memory::Edit;
use crate::todo::Edit as TodoEdit;

/// One `<number>-<name>/up.sql` per migration, applied in number order; `user_version` records how
/// many have run. Add a directory with the next number; never edit one that shipped.
static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

fn migrations() -> Result<Migrations<'static>> {
    Ok(Migrations::from_directory(&MIGRATIONS)?)
}

/// Row of `conversations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub id: i64,
    pub key: String,
}

impl Conversation {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            key: row.get("key")?,
        })
    }
}

/// Row of `messages`; `content` is the whole message as JSON, `role` is copied out for queries.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content: Value,
}

impl Message {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let content: String = row.get("content")?;
        Ok(Self {
            id: row.get("id")?,
            conversation_id: row.get("conversation_id")?,
            role: row.get("role")?,
            content: serde_json::from_str(&content).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })?,
        })
    }
}

/// Row of `memories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    pub id: i64,
    pub content: String,
}

impl Memory {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            content: row.get("content")?,
        })
    }
}

/// Row of `skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    /// One line, listed in the system prompt.
    pub description: String,
    pub content: String,
}

impl Skill {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            description: row.get("description")?,
            content: row.get("content")?,
        })
    }
}

/// Row of `jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    /// Empty for jobs made before names existed.
    pub name: String,
    /// Conversation key the job was made in, where its runs post.
    pub target: String,
    /// Cron expression; `None` runs once.
    pub schedule: Option<String>,
    /// Discord user who made it, whose memory its runs load.
    pub user: u64,
    pub prompt: String,
    /// Unix seconds.
    pub next_run: i64,
}

/// Row of `webhooks`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub id: i64,
    pub name: String,
    /// Conversation key the webhook was made in, where its runs post.
    pub target: String,
    /// Discord user who made it, whose memory its runs load.
    pub user: u64,
    pub prompt: String,
    /// Path segment after `/hook/` that wakes it.
    pub secret: String,
}

impl Hook {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            target: row.get("target")?,
            user: row.get::<_, i64>("user_id")?.cast_unsigned(),
            prompt: row.get("prompt")?,
            secret: row.get("secret")?,
        })
    }
}

impl Job {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            target: row.get("target")?,
            schedule: row.get("schedule")?,
            user: row.get::<_, i64>("user_id")?.cast_unsigned(),
            prompt: row.get("prompt")?,
            next_run: row.get("next_run")?,
        })
    }
}

/// A search_history match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// When it was saved, in UTC.
    pub at: String,
    pub role: String,
    pub text: String,
    /// Whether it is from the conversation searched from, rather than another of the user's.
    pub here: bool,
    /// For a user message, the text of the model's next message after it: usually its answer.
    pub reply: Option<String>,
}

/// Row of `todos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub id: i64,
    /// Discord user it belongs to.
    pub user: u64,
    /// Conversation key it was written in, where the heartbeat posts about it.
    pub target: String,
    pub content: String,
}

impl Todo {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            user: row.get::<_, i64>("user_id")?.cast_unsigned(),
            target: row.get("target")?,
            content: row.get("content")?,
        })
    }
}

/// A Discord user ID as SQLite's signed integer; the cast keeps every bit, so it reads back exactly.
fn sql_user(user: u64) -> i64 {
    user.cast_signed()
}

/// Shared handle; every query runs on Tokio's blocking pool.
#[derive(Debug, Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Opens (or creates) the database file and applies pending migrations.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open database {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().map_err(|_| anyhow!("database lock poisoned"))?;
            f(&mut conn)
        })
        .await
        .context("database task panicked")?
    }

    /// Returns the conversation for `key`, creating it on first use.
    pub async fn conversation(&self, key: &str) -> Result<Conversation> {
        let key = key.to_owned();
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO conversations (key) VALUES (?1) ON CONFLICT (key) DO NOTHING",
                params![key],
            )?;
            Ok(conn.query_row(
                "SELECT id, key FROM conversations WHERE key = ?1",
                params![key],
                Conversation::from_row,
            )?)
        })
        .await
    }

    /// Whether a conversation for `key` was ever started.
    pub async fn has_conversation(&self, key: &str) -> Result<bool> {
        let key = key.to_owned();
        self.with_conn(move |conn| {
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM conversations WHERE key = ?1)",
                params![key],
                |row| row.get(0),
            )?)
        })
        .await
    }

    pub async fn messages(&self, conversation_id: i64) -> Result<Vec<Message>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, conversation_id, role, content FROM messages
                 WHERE conversation_id = ?1 AND archived = 0 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![conversation_id], Message::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Appends messages (JSON objects with a `role`) in one transaction. `user` is whose turn
    /// wrote them, `None` for chat nobody asked the bot about.
    pub async fn append(
        &self,
        conversation_id: i64,
        user: Option<u64>,
        messages: Vec<Value>,
    ) -> Result<()> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            insert_messages(&tx, conversation_id, user, &messages)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Replaces the conversation's live history with `messages`, archiving the old rows. The new
    /// rows belong to nobody: they are a summary, or copies of turns whose archived originals
    /// already show up in their sender's searches.
    pub async fn rewrite(&self, conversation_id: i64, messages: Vec<Value>) -> Result<()> {
        self.with_conn(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE messages SET archived = 1 WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            insert_messages(&tx, conversation_id, None, &messages)?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Archives every message in the conversation, so it starts empty but stays searchable.
    pub async fn clear(&self, conversation_id: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE messages SET archived = 1 WHERE conversation_id = ?1",
                params![conversation_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Messages out of the conversation's context whose text contains every one of `terms`
    /// (ignoring ASCII case), newest first: its archived ones, and `user`'s in any other
    /// conversation.
    pub async fn search(
        &self,
        conversation_id: i64,
        user: u64,
        terms: Vec<String>,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        self.with_conn(move |conn| {
            // Tool rounds have no text, so the next assistant row with text is the answer.
            let mut sql = "SELECT messages.created_at, messages.role, message_text.text,
                     messages.conversation_id = ?1,
                     CASE messages.role WHEN 'user' THEN (
                         SELECT next_text.text
                         FROM messages AS next JOIN message_text AS next_text
                             ON next_text.rowid = next.id
                         WHERE next.conversation_id = messages.conversation_id
                             AND next.id > messages.id AND next.role = 'assistant'
                         ORDER BY next.id LIMIT 1
                     ) END
                 FROM message_text JOIN messages ON messages.id = message_text.rowid
                 WHERE ((messages.conversation_id = ?1 AND messages.archived = 1)
                     OR (messages.user_id = ?2 AND messages.conversation_id != ?1))"
                .to_owned();
            // The trigram index serves LIKE for terms of three or more characters; shorter
            // ones scan the candidate rows.
            let mut values = vec![
                rusqlite::types::Value::from(conversation_id),
                sql_user(user).into(),
            ];
            for term in terms {
                sql.push_str(" AND message_text.text LIKE ?");
                let term = term.replace(['%', '_'], "");
                values.push(format!("%{term}%").into());
            }
            sql.push_str(" ORDER BY messages.id DESC LIMIT ?");
            values.push(i64::try_from(limit)?.into());
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
                Ok(Hit {
                    at: row.get(0)?,
                    role: row.get(1)?,
                    text: row.get(2)?,
                    here: row.get(3)?,
                    reply: row.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}

impl Db {
    /// `user`'s entries.
    pub async fn memories(&self, user: u64) -> Result<Vec<Memory>> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, content FROM memories WHERE user_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![sql_user(user)], Memory::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    // ponytail: edits are planned against a read taken just before; two processes editing
    // memory at the same instant could overshoot the budget slightly.
    /// Applies `edit` to `user`'s entries; ids of someone else's entries match nothing.
    pub async fn save_memory(&self, user: u64, edit: Edit) -> Result<()> {
        self.with_conn(move |conn| {
            match edit {
                Edit::Add(content) => conn.execute(
                    "INSERT INTO memories (content, user_id) VALUES (?1, ?2)",
                    params![content, sql_user(user)],
                )?,
                Edit::Replace(id, content) => conn.execute(
                    "UPDATE memories SET content = ?2 WHERE id = ?1 AND user_id = ?3",
                    params![id, content, sql_user(user)],
                )?,
                Edit::Remove(id) => conn.execute(
                    "DELETE FROM memories WHERE id = ?1 AND user_id = ?2",
                    params![id, sql_user(user)],
                )?,
            };
            Ok(())
        })
        .await
    }

    /// `user`'s skills by name.
    pub async fn skills(&self, user: u64) -> Result<Vec<Skill>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT name, description, content FROM skills WHERE user_id = ?1 ORDER BY name",
            )?;
            let rows = stmt.query_map(params![sql_user(user)], Skill::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Creates `skill` for `user`, or replaces their one with its name.
    pub async fn save_skill(&self, user: u64, skill: Skill) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO skills (user_id, name, description, content) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (ifnull(user_id, 0), name) DO UPDATE SET description = excluded.description,
                     content = excluded.content,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                params![sql_user(user), skill.name, skill.description, skill.content],
            )?;
            Ok(())
        })
        .await
    }

    /// Deletes `user`'s skill named `name`; false if they had none.
    pub async fn remove_skill(&self, user: u64, name: String) -> Result<bool> {
        self.with_conn(move |conn| {
            let sql = "DELETE FROM skills WHERE name = ?1 AND user_id = ?2";
            Ok(conn.execute(sql, params![name, sql_user(user)])? > 0)
        })
        .await
    }

    /// Every scheduled job, soonest first.
    pub async fn jobs(&self) -> Result<Vec<Job>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, target, schedule, prompt, next_run, user_id FROM jobs
                 ORDER BY next_run, id",
            )?;
            let rows = stmt.query_map([], Job::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Stores a job that posts to the conversation `target` and returns its id.
    pub async fn add_job(&self, target: String, user: u64, job: NewJob) -> Result<i64> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO jobs (name, target, schedule, prompt, next_run, user_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    job.name,
                    target,
                    job.schedule,
                    job.prompt,
                    job.next_run,
                    sql_user(user)
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub async fn set_next_run(&self, id: i64, next_run: i64) -> Result<()> {
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE jobs SET next_run = ?2 WHERE id = ?1",
                params![id, next_run],
            )?;
            Ok(())
        })
        .await
    }

    /// Saves `job`'s prompt and timing; false if there was none with its id.
    pub async fn update_job(&self, job: &Job) -> Result<bool> {
        let job = job.clone();
        self.with_conn(move |conn| {
            Ok(conn.execute(
                "UPDATE jobs SET prompt = ?2, schedule = ?3, next_run = ?4 WHERE id = ?1",
                params![job.id, job.prompt, job.schedule, job.next_run],
            )? > 0)
        })
        .await
    }

    /// Deletes a job; false if there was none with that id.
    pub async fn remove_job(&self, id: i64) -> Result<bool> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM jobs WHERE id = ?1", params![id])? > 0)
        })
        .await
    }

    /// Every webhook, oldest first.
    pub async fn hooks(&self) -> Result<Vec<Hook>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, target, user_id, prompt, secret FROM webhooks ORDER BY id",
            )?;
            let rows = stmt.query_map([], Hook::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// The webhook woken by `secret`, if any.
    pub async fn hook_by_secret(&self, secret: String) -> Result<Option<Hook>> {
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, name, target, user_id, prompt, secret FROM webhooks
                     WHERE secret = ?1",
                    params![secret],
                    Hook::from_row,
                )
                .optional()?)
        })
        .await
    }

    /// Stores a webhook that posts to the conversation `target` and returns its id.
    pub async fn add_hook(
        &self,
        target: String,
        user: u64,
        name: String,
        prompt: String,
        secret: String,
    ) -> Result<i64> {
        self.with_conn(move |conn| {
            conn.execute(
                "INSERT INTO webhooks (name, target, user_id, prompt, secret)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, target, sql_user(user), prompt, secret],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
    }

    /// Deletes a webhook; false if there was none with that id.
    pub async fn remove_hook(&self, id: i64) -> Result<bool> {
        self.with_conn(move |conn| {
            Ok(conn.execute("DELETE FROM webhooks WHERE id = ?1", params![id])? > 0)
        })
        .await
    }
}

impl Db {
    /// `user`'s todos, oldest first.
    pub async fn todos(&self, user: u64) -> Result<Vec<Todo>> {
        self.with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, user_id, target, content FROM todos WHERE user_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![sql_user(user)], Todo::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Everyone's todos, oldest first, for the heartbeat.
    pub async fn all_todos(&self) -> Result<Vec<Todo>> {
        self.with_conn(move |conn| {
            let mut stmt =
                conn.prepare("SELECT id, user_id, target, content FROM todos ORDER BY id")?;
            let rows = stmt.query_map([], Todo::from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Applies `edit` to `user`'s todos; a new one is posted about in `target`. Ids of someone
    /// else's todos match nothing.
    pub async fn save_todo(&self, user: u64, target: String, edit: TodoEdit) -> Result<()> {
        self.with_conn(move |conn| {
            match edit {
                TodoEdit::Add(content) => conn.execute(
                    "INSERT INTO todos (user_id, target, content) VALUES (?1, ?2, ?3)",
                    params![sql_user(user), target, content],
                )?,
                TodoEdit::Update { id, content } => conn.execute(
                    "UPDATE todos SET content = ?2 WHERE id = ?1 AND user_id = ?3",
                    params![id, content, sql_user(user)],
                )?,
                TodoEdit::Done(id) => conn.execute(
                    "DELETE FROM todos WHERE id = ?1 AND user_id = ?2",
                    params![id, sql_user(user)],
                )?,
            };
            Ok(())
        })
        .await
    }
}

fn insert_messages(
    conn: &Connection,
    conversation_id: i64,
    user: Option<u64>,
    messages: &[Value],
) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO messages (conversation_id, role, content, user_id) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for message in messages {
        let Some(role) = message["role"].as_str() else {
            bail!("message without a role: {message}");
        };
        stmt.execute(params![
            conversation_id,
            role,
            message.to_string(),
            user.map(sql_user)
        ])?;
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    migrations()?
        .to_latest(conn)
        .context("failed to migrate the database")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migrations_are_valid() {
        migrations().expect("load").validate().expect("valid");
    }

    #[tokio::test]
    async fn webhooks_are_found_by_secret_and_removed() {
        let db = Db::open_in_memory().expect("open");
        let id = db
            .add_hook(
                "discord:1".into(),
                7,
                "CI".into(),
                "report".into(),
                "s3cret".into(),
            )
            .await
            .expect("add");
        let hook = db.hook_by_secret("s3cret".into()).await.expect("query");
        assert_eq!(hook.as_ref().map(|h| (h.id, h.user)), Some((id, 7)));
        assert_eq!(
            db.hook_by_secret("other".into()).await.expect("query"),
            None
        );
        assert!(db.remove_hook(id).await.expect("remove"));
        assert!(db.hooks().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_messages_round_trip() {
        let db = Db::open_in_memory().expect("open");
        {
            let mut conn = db.conn.lock().expect("lock");
            migrate(&mut conn).expect("second migrate is a no-op");
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |row| row.get(0))
                .expect("version");
            assert_eq!(
                usize::try_from(version).expect("positive"),
                MIGRATIONS.dirs().count()
            );
        }

        let conversation = db.conversation("t").await.expect("create");
        assert_eq!(db.conversation("t").await.expect("reuse"), conversation);

        let turn = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "hello"}]}),
        ];
        db.append(conversation.id, Some(7), turn.clone())
            .await
            .expect("append");
        let stored: Vec<Value> = db
            .messages(conversation.id)
            .await
            .expect("load")
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(stored, turn);

        db.clear(conversation.id).await.expect("clear");
        assert!(db.messages(conversation.id).await.expect("load").is_empty());
    }

    #[tokio::test]
    async fn search_finds_archived_text_only() {
        let db = Db::open_in_memory().expect("open");
        let conversation = db.conversation("discord:1").await.expect("conversation");
        let other = db.conversation("discord:2").await.expect("conversation");
        let text = |role: &str, text: &str| json!({"role": role, "content": [{"type": "text", "text": text}]});
        let tool = json!({"role": "user", "content": [{"type": "toolresult", "content": [{"type": "text", "text": "貓咪 tool output"}]}]});
        db.append(
            conversation.id,
            Some(7),
            vec![
                text("user", "我家的貓咪叫 Mochi"),
                tool,
                text("assistant", "記住了"),
            ],
        )
        .await
        .expect("append");
        db.append(other.id, Some(8), vec![text("user", "別人的貓咪")])
            .await
            .expect("append");
        let search = |terms: &[&str]| {
            db.search(
                conversation.id,
                7,
                terms.iter().map(|t| (*t).to_owned()).collect(),
                10,
            )
        };
        assert!(
            search(&["貓咪"]).await.expect("search").is_empty(),
            "live history is already in context"
        );

        db.clear(conversation.id).await.expect("clear");
        let hits = search(&["貓咪", "mochi"]).await.expect("search");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            (
                hits[0].role.as_str(),
                hits[0].text.as_str(),
                hits[0].here,
                hits[0].reply.as_deref()
            ),
            ("user", "我家的貓咪叫 Mochi", true, Some("記住了"))
        );
        assert!(search(&["貓咪", "Luna"]).await.expect("search").is_empty());
        assert_eq!(
            search(&["記"]).await.expect("search").len(),
            1,
            "short terms scan"
        );
    }

    #[tokio::test]
    async fn search_reaches_the_users_other_conversations() {
        let db = Db::open_in_memory().expect("open");
        let here = db.conversation("discord:1").await.expect("conversation").id;
        let thread = db.conversation("discord:2").await.expect("conversation").id;
        let text =
            |text: &str| json!({"role": "user", "content": [{"type": "text", "text": text}]});
        db.append(thread, Some(7), vec![text("那家拉麵店叫一蘭")])
            .await
            .expect("append");
        db.append(thread, Some(8), vec![text("我推薦的拉麵店是天下一品")])
            .await
            .expect("append");
        db.append(thread, None, vec![text("聽說拉麵店很多")])
            .await
            .expect("append");
        let hits = db
            .search(here, 7, vec!["拉麵店".to_owned()], 10)
            .await
            .expect("search");
        assert_eq!(hits.len(), 1, "only their own messages: {hits:?}");
        assert_eq!(
            (hits[0].text.as_str(), hits[0].here),
            ("那家拉麵店叫一蘭", false)
        );
        assert!(
            db.search(here, 9, vec!["拉麵店".to_owned()], 10)
                .await
                .expect("search")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn memories_round_trip_per_user() {
        let db = Db::open_in_memory().expect("open");
        let contents = |user| {
            let db = db.clone();
            async move {
                db.memories(user)
                    .await
                    .expect("load")
                    .into_iter()
                    .map(|m| (m.id, m.content))
                    .collect::<Vec<_>>()
            }
        };
        for (user, text) in [(6, "a"), (6, "b"), (7, "mine")] {
            db.save_memory(user, Edit::Add(text.to_owned()))
                .await
                .expect("add");
        }
        let theirs = contents(6).await;
        let mine = contents(7).await;
        assert_eq!(mine.len(), 1);
        assert!(contents(8).await.is_empty());

        db.save_memory(6, Edit::Replace(theirs[0].0, "a2".to_owned()))
            .await
            .expect("replace");
        db.save_memory(6, Edit::Remove(theirs[1].0))
            .await
            .expect("remove");
        // Someone else's entry id changes nothing.
        db.save_memory(8, Edit::Remove(mine[0].0))
            .await
            .expect("remove");
        let texts =
            |entries: Vec<(i64, String)>| entries.into_iter().map(|e| e.1).collect::<Vec<_>>();
        assert_eq!(texts(contents(6).await), ["a2"]);
        assert_eq!(texts(contents(7).await), ["mine"]);
    }

    #[tokio::test]
    async fn skills_round_trip_per_user() {
        let db = Db::open_in_memory().expect("open");
        let skill = |name: &str, content: &str| Skill {
            name: name.to_owned(),
            description: "d".to_owned(),
            content: content.to_owned(),
        };
        db.save_skill(6, skill("b", "one")).await.expect("save");
        db.save_skill(6, skill("a", "x")).await.expect("save");
        db.save_skill(6, skill("b", "two")).await.expect("replace");
        db.save_skill(7, skill("b", "mine")).await.expect("save");
        db.save_skill(7, skill("b", "mine2"))
            .await
            .expect("replace");
        let skills = db.skills(6).await.expect("load");
        assert_eq!(skills, [skill("a", "x"), skill("b", "two")]);
        assert_eq!(db.skills(7).await.expect("load"), [skill("b", "mine2")]);
        assert!(
            !db.remove_skill(7, "a".to_owned())
                .await
                .expect("not theirs")
        );
        assert!(db.remove_skill(6, "a".to_owned()).await.expect("remove"));
        assert!(
            !db.remove_skill(6, "a".to_owned())
                .await
                .expect("remove again")
        );
        assert_eq!(db.skills(6).await.expect("load").len(), 1);
        assert_eq!(db.skills(7).await.expect("load").len(), 1);
    }

    #[tokio::test]
    async fn jobs_round_trip() {
        let job = |name: &str, schedule: Option<&str>, next_run| NewJob {
            name: name.to_owned(),
            schedule: schedule.map(str::to_owned),
            prompt: "do it".to_owned(),
            next_run,
        };
        let db = Db::open_in_memory().expect("open");
        let later = db
            .add_job("discord:1".to_owned(), 7, job("a", Some("0 9 * * *"), 20))
            .await
            .expect("add");
        let sooner = db
            .add_job("discord:1".to_owned(), 6, job("b", None, 10))
            .await
            .expect("add");
        let ids = |jobs: Vec<Job>| jobs.into_iter().map(|j| j.id).collect::<Vec<_>>();
        let jobs = db.jobs().await.expect("load");
        assert_eq!(jobs[0].name, "b");
        assert_eq!((jobs[0].user, jobs[1].user), (6, 7));
        assert_eq!(ids(jobs), [sooner, later]);
        db.set_next_run(later, 5).await.expect("update");
        assert_eq!(ids(db.jobs().await.expect("load")), [later, sooner]);
        let mut revised = db.jobs().await.expect("load")[1].clone();
        revised.prompt = "do it better".to_owned();
        assert!(db.update_job(&revised).await.expect("update"));
        assert_eq!(db.jobs().await.expect("load")[1], revised);
        assert!(db.remove_job(sooner).await.expect("remove"));
        assert!(!db.remove_job(sooner).await.expect("remove again"));
        assert_eq!(ids(db.jobs().await.expect("load")), [later]);
    }

    #[tokio::test]
    async fn todos_are_per_user() {
        let db = Db::open_in_memory().expect("open");
        let add = |content: &str| TodoEdit::Add(content.to_owned());
        db.save_todo(7, "discord:1".to_owned(), add("report"))
            .await
            .expect("add");
        db.save_todo(6, "discord:2".to_owned(), add("mine"))
            .await
            .expect("add");
        let todos = db.todos(7).await.expect("load");
        assert_eq!(todos.len(), 1);
        let todo = &todos[0];
        assert_eq!(
            (todo.target.as_str(), todo.content.as_str()),
            ("discord:1", "report")
        );
        let other = db.todos(6).await.expect("load")[0].id;
        db.save_todo(7, String::new(), TodoEdit::Done(other))
            .await
            .expect("someone else's");
        assert_eq!(db.all_todos().await.expect("load").len(), 2);
        let update = TodoEdit::Update {
            id: todo.id,
            content: "report, draft sent".to_owned(),
        };
        db.save_todo(7, String::new(), update)
            .await
            .expect("update");
        let todos = db.todos(7).await.expect("load");
        assert_eq!(todos[0].content, "report, draft sent");
        db.save_todo(7, String::new(), TodoEdit::Done(todo.id))
            .await
            .expect("done");
        assert!(db.todos(7).await.expect("load").is_empty());
    }

    #[test]
    fn memories_go_per_conversation_and_back_to_global() {
        let migrations = migrations().expect("load");
        let mut conn = Connection::open_in_memory().expect("open");
        conn.pragma_update(None, "foreign_keys", true).expect("fk");
        migrations.to_version(&mut conn, 3).expect("old migrations");
        let tx = conn.transaction().expect("tx");
        tx.execute_batch(
            "INSERT INTO conversations (key) VALUES ('terminal'), ('discord:1');
             INSERT INTO memories (content) VALUES ('fact');",
        )
        .expect("seed");
        tx.commit().expect("commit");
        migrations.to_version(&mut conn, 4).expect("migration 4");
        let tx = conn.transaction().expect("tx");
        let count: i64 = tx
            .query_row(
                "SELECT count(*) FROM memories WHERE content = 'fact'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 2);

        tx.execute_batch(
            "INSERT INTO memories (conversation_id, content) VALUES (2, 'only in discord');",
        )
        .expect("seed");
        tx.commit().expect("commit");
        migrations
            .to_version(&mut conn, 6)
            .expect("later migrations");
        let mut stmt = conn
            .prepare("SELECT content FROM memories ORDER BY id")
            .expect("prepare");
        let merged: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<rusqlite::Result<_>>()
            .expect("rows");
        assert_eq!(merged, ["fact", "only in discord"]);
    }

    #[tokio::test]
    async fn rewrite_archives_old_rows() {
        let db = Db::open_in_memory().expect("open");
        let id = db.conversation("t").await.expect("create").id;
        let old = vec![
            json!({"role": "user", "content": "old"}),
            json!({"role": "assistant", "content": "reply"}),
        ];
        db.append(id, Some(7), old).await.expect("append");
        let summary = vec![json!({"role": "user", "content": "summary"})];
        db.rewrite(id, summary.clone()).await.expect("rewrite");
        let live: Vec<Value> = db
            .messages(id)
            .await
            .expect("load")
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(live, summary);
        let conn = db.conn.lock().expect("lock");
        let archived: i64 = conn
            .query_row(
                "SELECT count(*) FROM messages WHERE archived = 1",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(archived, 2);
    }

    #[tokio::test]
    async fn append_rejects_bad_role_atomically() {
        let db = Db::open_in_memory().expect("open");
        let id = db.conversation("t").await.expect("create").id;
        let turn = vec![
            json!({"role": "user", "content": "ok"}),
            json!({"role": "system", "content": "not allowed"}),
        ];
        assert!(db.append(id, Some(7), turn).await.is_err());
        assert!(db.messages(id).await.expect("load").is_empty());
    }
}
