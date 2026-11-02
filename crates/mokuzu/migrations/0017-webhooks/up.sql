-- Webhooks: a prompt that runs in a fresh conversation each time something posts to
-- `/hook/<secret>`, and posts its result to `target`, the conversation it was made in.
CREATE TABLE webhooks (
    id         INTEGER PRIMARY KEY,
    name       TEXT    NOT NULL,
    target     TEXT    NOT NULL,
    user_id    INTEGER NOT NULL,
    prompt     TEXT    NOT NULL,
    secret     TEXT    NOT NULL UNIQUE,
    created_at TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
