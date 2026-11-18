CREATE TABLE IF NOT EXISTS subagent_runs (
    child_run_id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    parent_call_id TEXT NOT NULL,
    task_index INTEGER NOT NULL,
    name TEXT NOT NULL,
    prompt TEXT NOT NULL,
    latest_activity TEXT,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    elapsed_ms INTEGER,
    result TEXT,
    seq INTEGER NOT NULL,
    incomplete INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_subagent_runs_conversation
    ON subagent_runs(conversation_id, started_at, child_run_id);
CREATE TABLE IF NOT EXISTS subagent_entries (
    child_run_id TEXT NOT NULL REFERENCES subagent_runs(child_run_id) ON DELETE CASCADE,
    entry_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    content TEXT NOT NULL,
    input TEXT,
    output TEXT,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    seq INTEGER NOT NULL,
    PRIMARY KEY (child_run_id, entry_id)
);
CREATE INDEX IF NOT EXISTS idx_subagent_entries_position ON subagent_entries(child_run_id, position);
