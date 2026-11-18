use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use aion_types::subagent::{SubagentChange, SubagentEvent, SubagentStatus};
use aionui_api_types::SubagentUpdate;
use aionui_db::ISubagentRepository;
use aionui_db::models::{SubagentEntryRow, SubagentRunRow};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::protocol::events::AgentStreamEvent;
use crate::protocol::events::subagent::{entry_response, run_response};

enum Command {
    Event(String, Box<SubagentEvent>),
    Flush(oneshot::Sender<()>),
}

type LostEvents = Arc<Mutex<HashMap<String, (String, SubagentEvent)>>>;

/// One bounded writer per embedded runtime, independent of UI subscriptions.
pub(crate) struct SubagentRecorder {
    tx: mpsc::Sender<Command>,
    turn: RwLock<String>,
    lost: LostEvents,
}

impl SubagentRecorder {
    pub(crate) fn new(
        repo: Arc<dyn ISubagentRepository>,
        user_id: String,
        conversation_id: String,
        events: broadcast::Sender<AgentStreamEvent>,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(256);
        let lost = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(write_loop(rx, lost.clone(), repo, user_id, conversation_id, events));
        Arc::new(Self {
            tx,
            turn: RwLock::new(String::new()),
            lost,
        })
    }

    pub(crate) fn begin_turn(&self, turn_id: &str) {
        *self.turn.write().unwrap_or_else(|e| e.into_inner()) = turn_id.into();
    }

    pub(crate) fn emit(&self, event: SubagentEvent) {
        let turn = self.turn.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Err(error) = self.tx.try_send(Command::Event(turn, Box::new(event)))
            && let Command::Event(turn, event) = error.into_inner()
        {
            // Retain only the latest lost observation per child, never an unbounded delta queue.
            self.lost
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(event.run.child_run_id.clone(), (turn, *event));
        }
    }

    pub(crate) async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Command::Flush(tx)).await.is_err() || rx.await.is_err() {
            tracing::error!("Subagent record writer stopped before drain");
        }
    }
}

struct RunState {
    run: SubagentRunRow,
    entries: BTreeMap<String, SubagentEntryRow>,
    dirty: HashSet<String>,
    changed: bool,
}

impl RunState {
    fn new(conversation_id: &str, turn: String, event: &SubagentEvent) -> Self {
        let identity = &event.run;
        Self {
            run: SubagentRunRow {
                child_run_id: identity.child_run_id.clone(),
                conversation_id: conversation_id.into(),
                turn_id: turn,
                parent_call_id: format!("aionrs-{}", identity.parent_call_id),
                task_index: identity.task_index as i64,
                name: identity.name.clone(),
                prompt: identity.prompt.clone(),
                latest_activity: None,
                started_at: identity.started_at,
                status: "running".into(),
                ended_at: None,
                elapsed_ms: None,
                result: None,
                seq: 0,
                incomplete: false,
            },
            entries: BTreeMap::new(),
            dirty: HashSet::new(),
            changed: true,
        }
    }

    fn entry(&mut self, id: &str, kind: &str, name: &str, at: i64) -> &mut SubagentEntryRow {
        self.dirty.insert(id.into());
        let position = self.entries.len() as i64 + 1;
        self.entries.entry(id.into()).or_insert_with(|| SubagentEntryRow {
            child_run_id: self.run.child_run_id.clone(),
            entry_id: id.into(),
            position,
            kind: kind.into(),
            name: name.into(),
            content: String::new(),
            input: None,
            output: None,
            status: "running".into(),
            started_at: at,
            ended_at: None,
            seq: self.run.seq,
        })
    }

    fn apply(&mut self, event: SubagentEvent) {
        if self.run.status != "running" {
            return;
        }
        self.changed = true;
        let at = event.occurred_at;
        match event.change {
            SubagentChange::Started => {}
            SubagentChange::Text {
                entry_id,
                text,
                thinking,
            } => {
                let entry = self.entry(&entry_id, if thinking { "thinking" } else { "text" }, "", at);
                entry.content.push_str(&text);
                self.run.latest_activity = Some(entry.content.lines().next().unwrap_or("").chars().take(180).collect());
            }
            SubagentChange::ToolCall {
                tool_call_id,
                name,
                input,
            } => {
                self.run.latest_activity = Some(name.clone());
                self.entry(&format!("tool-{tool_call_id}"), "tool", &name, at).input = Some(input);
            }
            SubagentChange::ToolResult {
                tool_call_id,
                name,
                output,
                is_error,
            } => {
                self.run.latest_activity = Some(name.clone());
                let entry = self.entry(&format!("tool-{tool_call_id}"), "tool", &name, at);
                entry.output = Some(output);
                entry.status = if is_error { "failed" } else { "completed" }.into();
                entry.ended_at = Some(at);
            }
            SubagentChange::MessageEnd { msg_id } => {
                for id in [format!("text-{msg_id}"), format!("thinking-{msg_id}")] {
                    if let Some(entry) = self.entries.get_mut(&id) {
                        entry.status = "completed".into();
                        entry.ended_at = Some(at);
                        self.dirty.insert(id);
                    }
                }
            }
            SubagentChange::Finished {
                status,
                text,
                elapsed_ms,
            } => {
                if !text.is_empty() {
                    self.run.latest_activity = Some(text.lines().next().unwrap_or("").chars().take(180).collect());
                }
                self.run.status = match status {
                    SubagentStatus::Completed => "completed",
                    SubagentStatus::Failed => "failed",
                    SubagentStatus::Cancelled => "cancelled",
                }
                .into();
                self.run.result = (!text.is_empty()).then_some(text);
                self.run.ended_at = Some(at);
                self.run.elapsed_ms = Some(elapsed_ms.min(i64::MAX as u64) as i64);
                for (id, entry) in &mut self.entries {
                    if entry.status == "running" {
                        entry.status = self.run.status.clone();
                        entry.ended_at = Some(at);
                        self.dirty.insert(id.clone());
                    }
                }
            }
        }
    }
}

async fn write_loop(
    mut rx: mpsc::Receiver<Command>,
    lost: LostEvents,
    repo: Arc<dyn ISubagentRepository>,
    user_id: String,
    conversation_id: String,
    events: broadcast::Sender<AgentStreamEvent>,
) {
    let mut states: HashMap<String, RunState> = HashMap::new();
    while let Some(first) = rx.recv().await {
        // Coalesce high-frequency text before touching SQLite or the broadcast channel.
        tokio::time::sleep(Duration::from_millis(40)).await;
        let mut commands = vec![first];
        while commands.len() < 64 && !matches!(commands.last(), Some(Command::Flush(_))) {
            match rx.try_recv() {
                Ok(command) => {
                    let barrier = matches!(command, Command::Flush(_));
                    commands.push(command);
                    if barrier {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let mut touched = HashSet::new();
        let mut barriers = Vec::new();
        for command in commands {
            match command {
                Command::Event(turn, event) => {
                    let id = event.run.child_run_id.clone();
                    let state = states
                        .entry(id.clone())
                        .or_insert_with(|| RunState::new(&conversation_id, turn, &event));
                    state.apply(*event);
                    touched.insert(id);
                }
                Command::Flush(reply) => barriers.push(reply),
            }
        }
        let losses = std::mem::take(&mut *lost.lock().unwrap_or_else(|e| e.into_inner()));
        for (id, (turn, event)) in losses {
            let state = states
                .entry(id.clone())
                .or_insert_with(|| RunState::new(&conversation_id, turn, &event));
            state.run.incomplete = true;
            state.changed = true;
            // A dropped terminal remains authoritative; dropped deltas are explicitly incomplete.
            if matches!(event.change, SubagentChange::Finished { .. }) {
                state.apply(event);
            }
            touched.insert(id);
            tracing::warn!(
                conversation_id,
                "Subagent observation buffer overflow; record marked incomplete"
            );
        }
        if !barriers.is_empty() {
            touched.extend(
                states
                    .iter()
                    .filter(|(_, state)| state.changed)
                    .map(|(id, _)| id.clone()),
            );
        }
        for id in touched {
            let Some(state) = states.get_mut(&id) else {
                continue;
            };
            if !state.changed {
                continue;
            }
            let mut next = state.run.clone();
            next.seq += 1;
            let entries: Vec<_> = state
                .dirty
                .iter()
                .filter_map(|id| state.entries.get(id))
                .cloned()
                .map(|mut entry| {
                    entry.seq = next.seq;
                    entry
                })
                .collect();
            let durable = match repo.apply(&user_id, &next, &entries).await {
                Ok(()) => {
                    state.run = next;
                    state.dirty.clear();
                    state.changed = false;
                    true
                }
                Err(error) => {
                    state.run.incomplete = true;
                    tracing::error!(conversation_id, child_run_id = %state.run.child_run_id, error_kind = ?std::mem::discriminant(&error), "Subagent record commit failed");
                    false
                }
            };
            let mut run = run_response(state.run.clone());
            if !durable && run.status == "running" {
                run.status = "unknown".into();
            }
            let _ = events.send(AgentStreamEvent::Subagent(SubagentUpdate {
                run,
                entries: if durable {
                    entries.into_iter().map(entry_response).collect()
                } else {
                    Vec::new()
                },
                durable,
            }));
        }
        if !barriers.is_empty() {
            // All observations before this barrier are now committed or visibly incomplete.
            states.clear();
        }
        for reply in barriers {
            let _ = reply.send(());
        }
    }
}

#[cfg(test)]
#[path = "subagent_recorder_test.rs"]
mod tests;
