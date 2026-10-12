//! Affordances: one cross-harness vocabulary for what an agent does besides talk — plans, goals,
//! schedules, background jobs, sub-agent spawns and returns, skills, hooks, questions, approvals,
//! checkpoints, branches, forks and model switches (`docs/design/AFFORDANCES.md`).
//!
//! This is a **derived view**, not IR: [`of_session`] reads it off what the IR already says (tool
//! names through the one [`classify_tool`] table, message kinds and origins, lineage, a short
//! record table), and [`Adapter::store_affordances`](crate::harness::Adapter::store_affordances)
//! adds what lives only in a side store (Claude `tasks/`, Grok `plan.json`, …). The one IR-side
//! piece is [`AffordanceSink`], which adapters wrap around their sink: it unwraps deferred-tool
//! calls (`tool_call{name, arguments}`) to the tool they call, and makes a turn that calls a spawn
//! tool a [`MessageKind::SubagentSpawn`] and a synchronous spawn's result a
//! [`MessageKind::SubagentReturn`].

mod text;

use crate::ir::{Block, Harness, Message, MessageKind, Origin, Role, Session};
use crate::stream::{Flow, MessageSink};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;

macro_rules! kinds {
    ($( $(#[$m:meta])* $name:ident => $s:literal ),+ $(,)?) => {
        /// One word of the vocabulary. Serialized as its snake_case name.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum Kind { $( $(#[$m])* $name, )+ }

        impl Kind {
            /// Every kind, in vocabulary order.
            pub const ALL: &'static [Kind] = &[$(Kind::$name),+];

            pub fn as_str(self) -> &'static str {
                match self { $( Kind::$name => $s, )+ }
            }
        }
    };
}

kinds! {
    /// A todo / task list, or a plan-mode switch.
    Plan => "plan",
    /// A standing objective the harness tracks.
    Goal => "goal",
    /// Cron jobs, wakeups, loops, heartbeats — and their fires.
    Schedule => "schedule",
    /// A shell or task left running, polled, stopped, completed.
    BackgroundJob => "background_job",
    /// A sub-agent launched.
    SubagentSpawn => "subagent_spawn",
    /// A sub-agent's result delivered.
    SubagentReturn => "subagent_return",
    /// A skill loaded or managed.
    Skill => "skill",
    /// A user hook's output reached the model.
    Hook => "hook",
    /// The agent asked the person.
    Question => "question",
    /// A permission request and its outcome, or the approval mode the session ran under.
    Approval => "approval",
    /// A restorable point (a file snapshot, a shadow-git commit, a saved chat) and its restore.
    Checkpoint => "checkpoint",
    /// Rewind / undo / reset / clear.
    Branch => "branch",
    /// This session was forked from another.
    Fork => "fork",
    /// The model or effort changed.
    ModelChange => "model_change",
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.iter().copied().find(|k| k.as_str() == s)
    }
}

/// Where a row was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A tool call in the transcript.
    ToolCall,
    /// A message the harness recorded (a notification, a marker, a hook's output).
    Message,
    /// A side store beside the transcript (a todo file, a table, a state row).
    Store,
}

/// One entry of a plan or goal, normalized from whatever list shape the harness writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// One affordance row.
#[derive(Debug, Clone, Serialize)]
pub struct Affordance {
    pub kind: Kind,
    /// What happened: `write`, `create`, `update`, `read`, `delete`, `list`, `enter`, `exit`,
    /// `start`, `stop`, `complete`, `fire`, `spawn`, `return`, `load`, `manage`, `output`, `ask`,
    /// `deny`, `restore`, `branch`, `fork`, `change`, `state` (a side store's current contents, or a
    /// fact the harness keeps once per session).
    pub action: &'static str,
    /// The message index, for a row read from the transcript; `None` for a store row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// The harness's own name for it: the tool, or the store / record it came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// One line: the prompt, command, schedule, skill name, or a count of items. Transcript text
    /// (untrusted): a terminal seam sanitizes it.
    pub summary: String,
    /// A plan's or goal's entries, when the call or store carries them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<Item>,
    pub source: Source,
}

impl Affordance {
    /// A row from a side store (no message index).
    pub fn store(kind: Kind, tool: impl Into<String>, summary: impl Into<String>, items: Vec<Item>) -> Affordance {
        Affordance {
            kind,
            action: "state",
            index: None,
            timestamp: None,
            tool: Some(tool.into()),
            tool_use_id: None,
            summary: summary.into(),
            items,
            source: Source::Store,
        }
    }
}

impl Affordance {
    /// An event a side store logged (a checkpoint commit, a permission ask): a store row with its
    /// own action and time, still without a message index.
    pub fn store_event(
        kind: Kind,
        action: &'static str,
        timestamp: Option<DateTime<Utc>>,
        tool: impl Into<String>,
        summary: &str,
    ) -> Affordance {
        Affordance {
            action,
            timestamp,
            summary: crate::ir::truncate(summary, 120),
            ..Affordance::store(kind, tool, "", Vec::new())
        }
    }
}

/// A side store's plan (or goal) as one row: its entries and their count by status. `None` when it
/// holds none: an empty todo file is a plan that is not there.
pub fn plan_state(kind: Kind, tool: impl Into<String>, items: Vec<Item>) -> Option<Affordance> {
    (!items.is_empty()).then(|| Affordance::store(kind, tool, items_summary(&items), items))
}

/// The JSON files of `dir` (sorted by name, numerically when the stems are numbers), parsed;
/// unreadable ones skipped. The shared reader behind the per-file side stores.
pub fn json_files(dir: &std::path::Path) -> Vec<(String, Value)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, Value)> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            let v = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
            Some((stem, v))
        })
        .collect();
    files.sort_by(|a, b| match (a.0.parse::<u64>(), b.0.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.0.cmp(&b.0),
    });
    files
}

/// The session-bag key under which an adapter lists the approval modes a session ran under, in
/// order of first appearance (Claude `permissionMode`, Codex `approval_policy`, Kimi Code
/// `permission.set_mode`, Qwen `approvalMode`, Kimi `state.json` `approval`).
pub const MODES_KEY: &str = "permission_modes";

/// The session-bag key under which an adapter tallies the restorable points it read past:
/// `{count, files?, tag?}` (Claude `file-history-snapshot`, Kimi Code `file_history.checkpoint`,
/// Kimi `_checkpoint`, a Gemini `/chat save` file).
pub const CHECKPOINTS_KEY: &str = "checkpoints";

/// Note one approval mode in a session bag ([`MODES_KEY`]); a repeat of a listed mode is a no-op.
pub fn note_mode(bag: &mut serde_json::Map<String, Value>, mode: &str) {
    if mode.is_empty() {
        return;
    }
    let list = bag.entry(MODES_KEY).or_insert_with(|| Value::Array(Vec::new()));
    if let Some(a) = list.as_array_mut() {
        if !a.iter().any(|v| v.as_str() == Some(mode)) {
            a.push(Value::String(mode.to_string()));
        }
    }
}

/// Note restorable points in a session bag ([`CHECKPOINTS_KEY`]): `points` more of them, and the
/// files they hold (each listed once).
pub fn note_checkpoints<'f>(
    bag: &mut serde_json::Map<String, Value>,
    points: u64,
    files: impl IntoIterator<Item = &'f str>,
) {
    let entry = bag
        .entry(CHECKPOINTS_KEY)
        .or_insert_with(|| serde_json::json!({"count": 0}));
    let Some(o) = entry.as_object_mut() else {
        return;
    };
    let n = o.get("count").and_then(Value::as_u64).unwrap_or(0) + points;
    o.insert("count".into(), n.into());
    for f in files {
        let list = o.entry("files").or_insert_with(|| Value::Array(Vec::new()));
        if let Some(a) = list.as_array_mut() {
            if !a.iter().any(|v| v.as_str() == Some(f)) {
                a.push(Value::String(f.to_string()));
            }
        }
    }
}

/// How a spawn tool's result relates to the sub-agent's return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Returns {
    /// The tool result IS the sub-agent's final answer (it ran synchronously).
    Result,
    /// The result is the answer unless its text contains this marker of a background launch.
    ResultUnless(&'static str),
    /// The result is only an acknowledgement; the return arrives later as its own record.
    Later,
}

fn flag(input: &Value, key: &str) -> bool {
    input.get(key).and_then(Value::as_bool) == Some(true)
}

fn str_at<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// The cron-style tools that take an `action` argument (Hermes `cronjob`, OpenClaw `cron`).
fn action_verb(input: &Value) -> &'static str {
    match str_at(input, "action").unwrap_or("") {
        "create" | "add" | "new" | "schedule" => "create",
        "remove" | "delete" | "cancel" | "rm" => "delete",
        "list" | "ls" | "status" | "get" | "show" => "list",
        "run" | "run_now" | "trigger" => "fire",
        "kill" | "stop" | "terminate" => "stop",
        _ => "update",
    }
}

/// The one table: `(harness, tool name, input)` → `(kind, action)`, or `None` for a tool that is not
/// an affordance. Scoped by harness only where a name means different things in different
/// harnesses (`task` is a sub-agent in OpenCode; `Bash` is a background job only when told to be).
pub fn classify_tool(h: Harness, name: &str, input: &Value) -> Option<(Kind, &'static str)> {
    use Harness as H;
    use Kind::*;
    // OpenClaw names a deferred core tool `openclaw:core:<tool>`.
    let name = name.rsplit(':').next().unwrap_or(name);
    Some(match (h, name) {
        // plan: whole-list writes
        (
            _,
            "TodoWrite" | "todo_write" | "todowrite" | "write_todos" | "todo__todo_write" | "SetTodoList" | "TodoList",
        )
        | (_, "update_plan" | "Checklist" | "update_todo_list")
        | (H::Hermes, "todo") => (Plan, "write"),
        (H::Hermes, "todo_list") => (Plan, "read"),
        (_, "todoread") => (Plan, "read"),
        // plan: item-wise task lists
        (_, "TaskCreate") | (H::Qwen, "task_create") | (_, "tracker_create_task") => (Plan, "create"),
        (_, "TaskUpdate") | (H::Qwen, "task_update") | (_, "tracker_update_task") => (Plan, "update"),
        (_, "TaskList" | "TaskGet") if h != H::Kimi && h != H::KimiCode => (Plan, "read"),
        (H::Qwen, "task_list" | "task_get") | (_, "tracker_list_tasks" | "tracker_get_task" | "tracker_visualize") => {
            (Plan, "read")
        }
        (_, n) if n.starts_with("tracker_") => (Plan, "update"),
        // plan mode
        (_, "EnterPlanMode" | "enter_plan_mode") => (Plan, "enter"),
        (_, "ExitPlanMode" | "exit_plan_mode") => (Plan, "exit"),
        // goal
        (_, "CreateGoal") => (Goal, "create"),
        (_, "UpdateGoal" | "update_goal" | "SetGoalBudget") => (Goal, "update"),
        (_, "GetGoal" | "get_goal") => (Goal, "read"),
        // schedule
        (_, "CronCreate" | "cron_create" | "scheduler_create" | "ScheduleWakeup" | "loop_wakeup") => {
            (Schedule, "create")
        }
        (_, "CronDelete" | "cron_delete" | "scheduler_delete") => (Schedule, "delete"),
        (_, "CronList" | "cron_list" | "scheduler_list") => (Schedule, "list"),
        (H::Hermes, "cronjob" | "cronjob_manage") | (H::OpenClaw, "cron" | "automations") => {
            (Schedule, action_verb(input))
        }
        // background jobs
        (_, "Bash") if flag(input, "run_in_background") => (BackgroundJob, "start"),
        (_, "run_terminal_command") if flag(input, "background") || flag(input, "is_background") => {
            (BackgroundJob, "start")
        }
        (_, "run_shell_command") if flag(input, "is_background") => (BackgroundJob, "start"),
        (H::Hermes, "terminal") | (H::OpenClaw, "exec") if flag(input, "background") => (BackgroundJob, "start"),
        (_, "Monitor" | "monitor") => (BackgroundJob, "start"),
        (_, "BashOutput" | "TaskOutput" | "get_command_or_subagent_output" | "WaitFor" | "CheckBackgroundJob") => {
            (BackgroundJob, "read")
        }
        (H::Kimi | H::KimiCode, "TaskList") => (BackgroundJob, "read"),
        (_, "KillShell" | "TaskStop" | "kill_command_or_subagent") => (BackgroundJob, "stop"),
        (H::Hermes, "process" | "process_manage") | (H::OpenClaw, "process") => match action_verb(input) {
            "stop" => (BackgroundJob, "stop"),
            _ => (BackgroundJob, "read"),
        },
        // sub-agents
        (_, n) if spawn_returns(h, n).is_some() => (SubagentSpawn, "spawn"),
        // skills
        (_, "Skill" | "activate_skill" | "load_skill" | "Skills") | (H::OpenCode | H::Qwen | H::Roo, "skill") => {
            (Skill, "load")
        }
        (_, "skill_manage") => (Skill, "manage"),
        // questions to the person
        (_, "AskUserQuestion" | "ask_user" | "ask_user_question" | "ask_followup_question" | "ask_question")
        | (_, "AskQuestion")
        | (H::Hermes, "clarify")
        | (H::OpenCode, "question") => (Question, "ask"),
        _ => return None,
    })
}

/// Whether `(h, name)` spawns a sub-agent, and how its result relates to the return.
pub fn spawn_returns(h: Harness, name: &str) -> Option<Returns> {
    use Harness as H;
    Some(match (h, name) {
        // Claude marks a synchronous return itself (from `toolUseResult`), per record.
        (H::Claude, "Agent" | "Task" | "Workflow") => Returns::Later,
        (H::OpenCode, "task") => Returns::Result,
        (H::OpenCode, "subagent") => Returns::ResultUnless("in the background"),
        (H::Hermes, "delegate_task") => Returns::ResultUnless("\"dispatched\""),
        (H::OpenClaw, "sessions_spawn") => Returns::Later,
        (H::Goose, "delegate") => Returns::Result,
        (H::Grok, "spawn_subagent") => Returns::Later,
        (H::Kimi, "Agent") => Returns::ResultUnless("background"),
        (H::Roo, "new_task") => Returns::Result,
        (H::Cline, "spawn_agent") => Returns::Result,
        (H::Cline, "team_spawn_teammate") => Returns::Later,
        (H::Devin, "run_subagent") => Returns::Result,
        (H::Continue, "Subagent") => Returns::Result,
        _ => return None,
    })
}

/// The deferred-tool wrapper Hermes, OpenClaw 10.1 and Qwen route lazily loaded tools through.
/// Hermes and Qwen spell it `tool_call{name, arguments}`, OpenClaw `tool_call{id, args}`. In the
/// IR the call is the inner tool's, with this namespace.
pub const DEFERRED: &str = "tool_call";

/// The `(name key, arguments key)` of `h`'s deferred-tool wrapper.
fn deferred_keys(h: Harness) -> (&'static str, &'static str) {
    match h {
        Harness::OpenClaw => ("id", "args"),
        _ => ("name", "arguments"),
    }
}

/// `tool_call{name, arguments}` (or OpenClaw's `{id, args}`) → the inner tool's call, namespaced
/// [`DEFERRED`]. Anything else (including a wrapper without a string name) is left alone.
pub fn unwrap_deferred(h: Harness, b: &mut Block) {
    if let Block::ToolUse {
        name, input, namespace, ..
    } = b
    {
        if name != DEFERRED || namespace.is_some() {
            return;
        }
        let (name_key, args_key) = deferred_keys(h);
        let Some(inner) = input.get(name_key).and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let args = input.get(args_key).cloned().unwrap_or(Value::Null);
        *name = inner;
        *input = args;
        *namespace = Some(DEFERRED.to_string());
    }
}

/// The inverse of [`unwrap_deferred`], for an emitter whose store spells the wrapper: an unwrapped
/// call goes back to `h`'s `tool_call` shape. `None` for any other block.
pub fn rewrap_deferred(h: Harness, b: &Block) -> Option<Block> {
    match b {
        Block::ToolUse {
            id,
            name,
            input,
            namespace: Some(ns),
        } if ns == DEFERRED => {
            let (name_key, args_key) = deferred_keys(h);
            let mut wrapped = serde_json::Map::new();
            wrapped.insert(name_key.into(), Value::String(name.clone()));
            wrapped.insert(args_key.into(), input.clone());
            Some(Block::ToolUse {
                id: id.clone(),
                name: DEFERRED.to_string(),
                input: Value::Object(wrapped),
                namespace: None,
            })
        }
        _ => None,
    }
}

/// A sink wrapper an adapter puts around its sink. It unwraps `tool_call` (for the harnesses whose
/// reader does not: Hermes, OpenClaw), then: a `Reply` that calls a spawn tool becomes a
/// [`MessageKind::SubagentSpawn`], and the tool-result turn answering a synchronous spawn a
/// [`MessageKind::SubagentReturn`] with [`Origin::Subagent`]. The state is the set of open
/// synchronous spawn ids, so it stays O(spawns).
pub struct AffordanceSink<'a> {
    harness: Harness,
    inner: &'a mut dyn MessageSink,
    open: std::collections::HashMap<String, Returns>,
}

impl<'a> AffordanceSink<'a> {
    pub fn new(harness: Harness, inner: &'a mut dyn MessageSink) -> Self {
        AffordanceSink {
            harness,
            inner,
            open: Default::default(),
        }
    }
}

impl MessageSink for AffordanceSink<'_> {
    fn message(&mut self, mut m: Message) -> Flow {
        if matches!(self.harness, Harness::Hermes | Harness::OpenClaw) {
            let h = self.harness;
            m.content.iter_mut().for_each(|b| unwrap_deferred(h, b));
        }
        refine_spawn(self.harness, &mut m, &mut self.open);
        self.inner.message(m)
    }

    fn meta(&mut self, s: &Session) {
        self.inner.meta(s);
    }

    /// Never while a synchronous spawn waits for its result: which result returns it is state no
    /// checkpoint carries.
    fn wants_checkpoint(&mut self, offset: u64) -> bool {
        self.open.is_empty() && self.inner.wants_checkpoint(offset)
    }

    fn checkpoint(&mut self, cp: &mut crate::stream::Checkpoint) {
        self.inner.checkpoint(cp);
    }
}

fn refine_spawn(h: Harness, m: &mut Message, open: &mut std::collections::HashMap<String, Returns>) {
    match (m.role, m.kind) {
        (Role::Assistant, MessageKind::Reply) => {
            let mut spawned = false;
            for b in &m.content {
                if let Block::ToolUse { id, name, .. } = b {
                    if let Some(ret) = spawn_returns(h, name) {
                        spawned = true;
                        if ret != Returns::Later {
                            open.insert(id.clone(), ret);
                        }
                    }
                }
            }
            if spawned {
                m.kind = MessageKind::SubagentSpawn;
            }
        }
        _ if !open.is_empty() && !m.content.is_empty() => {
            let all_results = m.content.iter().all(|b| matches!(b, Block::ToolResult { .. }));
            if !all_results {
                return;
            }
            let mut returned = false;
            for b in &m.content {
                if let Block::ToolResult {
                    tool_use_id, content, ..
                } = b
                {
                    match open.remove(tool_use_id.as_str()) {
                        Some(Returns::Result) => returned = true,
                        // A lazy span is a large result: an answer, not a launch receipt.
                        Some(Returns::ResultUnless(marker)) => {
                            returned |= match content {
                                crate::lazy::Text::Inline(t) => !t.contains(marker),
                                crate::lazy::Text::Span(_) => true,
                            }
                        }
                        _ => {}
                    }
                }
            }
            if returned {
                m.kind = MessageKind::SubagentReturn;
                m.origin = Origin::Subagent;
            }
        }
        _ => {}
    }
}

/// The entries of a plan/goal tool's input (or a store's JSON), whatever the list is called:
/// the first array under `todos`/`plan`/`items`/`tasks`/`checklist`, each element's text under
/// the first of `content`/`step`/`description`/`title`/`subject`/`text`/`task`/`name`; or a
/// markdown checklist / numbered list in a string under one of those keys.
pub fn items_of(v: &Value) -> Vec<Item> {
    const LISTS: [&str; 7] = ["todos", "plan", "items", "tasks", "checklist", "content", "markdown"];
    for key in LISTS {
        match v.get(key) {
            Some(Value::Array(a)) => return a.iter().filter_map(item_of).collect(),
            // Keyed by id (Grok `plan.json` `todos: {id: {...}}`).
            Some(Value::Object(o)) if key == "todos" => return o.values().filter_map(item_of).collect(),
            Some(Value::String(s)) => {
                // A JSON list in a string (Roo `new_task{todos: "[]"}`) or a markdown list.
                if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(s) {
                    return a.iter().filter_map(item_of).collect();
                }
                let md = markdown_items(s);
                if !md.is_empty() {
                    return md;
                }
            }
            _ => {}
        }
    }
    if let Value::Array(a) = v {
        return a.iter().filter_map(item_of).collect();
    }
    item_of(v).into_iter().collect()
}

/// One element of a plan list: an object with a text field and maybe a status, or a bare string.
pub fn item_of(v: &Value) -> Option<Item> {
    const TEXT: [&str; 9] = [
        "content",
        "subject",
        "title",
        "step",
        "description",
        "text",
        "task",
        "name",
        "goal",
    ];
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(Item {
            content: s.trim().to_string(),
            status: None,
        }),
        Value::Object(o) => {
            let content = TEXT
                .iter()
                .find_map(|k| o.get(*k).and_then(Value::as_str))
                .filter(|c| !c.trim().is_empty())?;
            let status = o
                .get("status")
                .or_else(|| o.get("state"))
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(Item {
                content: content.to_string(),
                status,
            })
        }
        _ => None,
    }
}

/// `- [x] done`, `[ ] todo`, `[-] doing`, `1. step`, `* step` lines.
fn markdown_items(s: &str) -> Vec<Item> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut t = line.trim();
        let bullet = t.strip_prefix("- ").or_else(|| t.strip_prefix("* "));
        let numbered = t
            .split_once(". ")
            .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            .map(|(_, rest)| rest);
        let listed = bullet.is_some() || numbered.is_some();
        t = bullet.or(numbered).unwrap_or(t).trim_start();
        let (status, rest) = if let Some(r) = t.strip_prefix("[x]").or_else(|| t.strip_prefix("[X]")) {
            (Some("completed"), r)
        } else if let Some(r) = t.strip_prefix("[ ]") {
            (Some("pending"), r)
        } else if let Some(r) = t.strip_prefix("[-]").or_else(|| t.strip_prefix("[~]")) {
            (Some("in_progress"), r)
        } else if listed {
            (None, t)
        } else {
            continue;
        };
        let rest = rest.trim();
        if !rest.is_empty() {
            out.push(Item {
                content: rest.to_string(),
                status: status.map(str::to_string),
            });
        }
    }
    out
}

/// A one-line count of a plan's entries by status: `3 items: 1 completed, 2 pending`.
pub fn items_summary(items: &[Item]) -> String {
    let mut by: std::collections::BTreeMap<&str, usize> = Default::default();
    for i in items {
        *by.entry(i.status.as_deref().unwrap_or("listed")).or_default() += 1;
    }
    let parts: Vec<String> = by.iter().map(|(s, n)| format!("{n} {s}")).collect();
    let n = items.len();
    format!("{n} item{}: {}", if n == 1 { "" } else { "s" }, parts.join(", "))
}

/// The summary line for one tool-call row.
fn call_summary(kind: Kind, input: &Value, items: &[Item]) -> String {
    if !items.is_empty() {
        return items_summary(items);
    }
    // An item-wise update: which item, and what it became.
    if let Some(status) = str_at(input, "status").filter(|_| kind == Kind::Plan) {
        let which = ["taskId", "task_id", "id", "subject"].iter().find_map(|k| {
            input.get(*k).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
        });
        if let Some(which) = which {
            return crate::ir::truncate(&format!("{which} → {status}"), 120);
        }
    }
    const KEYS: [&str; 16] = [
        "description",
        "subject",
        "prompt",
        "command",
        "cron",
        "schedule",
        "skill",
        "name",
        "question",
        "goal",
        "objective",
        "instructions",
        "message",
        "reason",
        "id",
        "taskId",
    ];
    let mut parts = Vec::new();
    for k in KEYS {
        if let Some(s) = input.get(k).and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        }) {
            parts.push(s);
            if parts.len() == 2 || kind != Kind::Schedule {
                break;
            }
        }
    }
    if parts.is_empty() {
        if let Some(q) = input.get("questions").and_then(Value::as_array).and_then(|a| a.first()) {
            if let Some(s) = q.get("question").and_then(Value::as_str) {
                parts.push(s.to_string());
            }
        }
        if let Some(t) = input.get("tasks").and_then(Value::as_array).and_then(|a| a.first()) {
            if let Some(s) = item_of(t) {
                parts.push(s.content);
            }
        }
        if let Some(s) = str_at(input, "status") {
            parts.push(s.to_string());
        }
    }
    crate::ir::truncate(&parts.join(" · "), 120)
}

/// Record-shaped affordances: messages the harness wrote that no `MessageKind` names.
fn record(h: Harness, m: &Message, resolver: &crate::lazy::Resolver) -> Option<(Kind, &'static str, &'static str)> {
    use Harness as H;
    let bag = m.harness_extra(h);
    let bag_str = |k: &str| bag.and_then(|b| b.get(k)).and_then(Value::as_str);
    let head = match m.content.first() {
        Some(Block::Text { text }) => Some(text),
        _ => None,
    };
    let starts_with = |prefix| head.is_some_and(|head| text::starts_with(head, resolver, prefix));
    match h {
        H::Claude if m.kind == MessageKind::InjectedContext && starts_with("<task-notification>") => {
            Some((Kind::BackgroundJob, "complete", "task-notification"))
        }
        H::Kimi if starts_with("<notification") => Some((Kind::BackgroundJob, "complete", "notification")),
        H::KimiCode => match bag_str("origin_kind")? {
            "task" | "background_task" => Some((Kind::BackgroundJob, "complete", "task")),
            "skill_activation" => Some((Kind::Skill, "load", "skill_activation")),
            "hook_result" => Some((Kind::Hook, "output", "hook_result")),
            _ => None,
        },
        H::Hermes => match bag_str("display_kind")? {
            "process_complete" => Some((Kind::BackgroundJob, "complete", "process_complete")),
            "skill_invocation" => Some((Kind::Skill, "load", "skill_invocation")),
            _ => None,
        },
        // An MCP call Codex refused because the approval policy could not ask (`never`).
        H::Codex
            if bag_str("item_type") == Some("McpToolCall")
                && head.is_some_and(|head| text::contains(head, resolver, "requires approval")) =>
        {
            Some((Kind::Approval, "deny", "McpToolCall"))
        }
        H::OpenCode
            if bag
                .and_then(|b| b.get("metadata"))
                .and_then(|v| v.get("source"))
                .and_then(Value::as_str)
                == Some("shell") =>
        {
            Some((Kind::BackgroundJob, "complete", "synthetic"))
        }
        _ => None,
    }
}

/// A message's one-line summary from its complete first text or tool output. The reducer searches
/// for the first complete summary/reminder wrapper and keeps only enough text for the final row.
fn text_head(m: &Message, resolver: &crate::lazy::Resolver) -> String {
    m.content
        .iter()
        .find_map(|b| match b {
            Block::Text { text } => Some(text),
            Block::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .map(|body| text::facts(body, resolver).summary())
        .unwrap_or_default()
}

/// What one message contributes: a tool call the table names, or the message itself (a kind, a
/// record, an origin). The one classification behind both [`of_session`] (which builds the rows)
/// and [`Counter`] (which only counts them), so the catalog's counts and the view cannot disagree.
enum Hit<'a> {
    Call {
        kind: Kind,
        action: &'static str,
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    Message {
        kind: Kind,
        action: &'static str,
        tool: Option<&'static str>,
    },
    /// A fact one block or one bag entry carries, with its own summary: a denied tool result, a
    /// file snapshot riding a turn.
    Detail {
        kind: Kind,
        action: &'static str,
        tool: Option<&'a str>,
        tool_use_id: Option<&'a str>,
        summary: String,
    },
}

/// The first eight characters of a snapshot hash, for a summary.
fn short(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

/// The paths of a JSON list of strings, joined for a summary.
fn joined(v: Option<&Value>) -> String {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// The restorable points a message carries beside its content: OpenCode's `patch` parts (1.x) and
/// `snapshot{start, end, files}` (2.x) when the step changed files, Devin's `restorable` files on
/// an edit's result.
fn detail_hits<'a>(
    h: Harness,
    m: &'a Message,
    resolver: &crate::lazy::Resolver,
    summaries: bool,
    out: &mut Vec<Hit<'a>>,
) {
    for b in &m.content {
        let Block::ToolResult {
            tool_use_id,
            content,
            is_error,
            tool_name,
            details,
            ..
        } = b
        else {
            continue;
        };
        if *is_error && text::denied(content, resolver, h) {
            out.push(Hit::Detail {
                kind: Kind::Approval,
                action: "deny",
                tool: tool_name.as_deref(),
                tool_use_id: Some(tool_use_id),
                summary: if summaries {
                    text::plain_summary(content, resolver)
                } else {
                    String::new()
                },
            });
        }
        if let Some(files) = details
            .as_ref()
            .and_then(|d| d.get("restorable"))
            .filter(|f| !joined(Some(f)).is_empty())
        {
            out.push(Hit::Detail {
                kind: Kind::Checkpoint,
                action: "create",
                tool: tool_name.as_deref(),
                tool_use_id: Some(tool_use_id),
                summary: crate::ir::truncate(&joined(Some(files)), 120),
            });
        }
    }
    if h != Harness::OpenCode {
        return;
    }
    let Some(bag) = m.harness_extra(h) else {
        return;
    };
    for p in bag.get("patches").and_then(Value::as_array).into_iter().flatten() {
        let hash = p.get("hash").and_then(Value::as_str).unwrap_or("");
        out.push(Hit::Detail {
            kind: Kind::Checkpoint,
            action: "create",
            tool: Some("patch"),
            tool_use_id: None,
            summary: crate::ir::truncate(&format!("{} · {}", short(hash), joined(p.get("files"))), 120),
        });
    }
    if let Some(snap) = bag.get("snapshot").filter(|v| !joined(v.get("files")).is_empty()) {
        let hash = snap.get("start").and_then(Value::as_str).unwrap_or("");
        out.push(Hit::Detail {
            kind: Kind::Checkpoint,
            action: "create",
            tool: Some("snapshot"),
            tool_use_id: None,
            summary: crate::ir::truncate(&format!("{} · {}", short(hash), joined(snap.get("files"))), 120),
        });
    }
}

fn hits<'a>(h: Harness, m: &'a Message, resolver: &crate::lazy::Resolver, summaries: bool) -> Vec<Hit<'a>> {
    let mut out = Vec::new();
    let mut spawn_calls = 0;
    for b in &m.content {
        let Block::ToolUse { id, name, input, .. } = b else {
            continue;
        };
        let Some((kind, action)) = classify_tool(h, name, input) else {
            continue;
        };
        // A spawn is a spawn when the adapter said so (the message kind); a harness that marks
        // spawns with its own records (Codex, Kimi Code) is read from those.
        if kind == Kind::SubagentSpawn {
            if m.kind != MessageKind::SubagentSpawn {
                continue;
            }
            spawn_calls += 1;
        }
        out.push(Hit::Call {
            kind,
            action,
            id,
            name,
            input,
        });
    }
    let by_kind = match m.kind {
        MessageKind::SubagentSpawn if spawn_calls == 0 => Some((Kind::SubagentSpawn, "spawn")),
        MessageKind::SubagentReturn => Some((Kind::SubagentReturn, "return")),
        MessageKind::Branch => Some((Kind::Branch, "branch")),
        MessageKind::ModelChange => Some((Kind::ModelChange, "change")),
        _ => None,
    };
    let message = match by_kind {
        Some((kind, action)) => Some((kind, action, None)),
        None => record(h, m, resolver)
            .map(|(kind, action, tool)| (kind, action, Some(tool)))
            .or(match m.origin {
                Origin::Hook => Some((Kind::Hook, "output", None)),
                Origin::Scheduler => Some((Kind::Schedule, "fire", None)),
                _ => None,
            }),
    };
    if let Some((kind, action, tool)) = message {
        out.push(Hit::Message { kind, action, tool });
    }
    detail_hits(h, m, resolver, summaries, &mut out);
    out
}

/// A row the session carries once rather than at a message.
fn session_row(kind: Kind, action: &'static str, tool: &str, summary: String, items: Vec<Item>) -> Affordance {
    Affordance {
        kind,
        action,
        index: None,
        timestamp: None,
        tool: Some(tool.to_string()),
        tool_use_id: None,
        summary: crate::ir::truncate(&summary, 120),
        items,
        source: Source::Message,
    }
}

/// The facts a harness keeps once per session, as rows: the fork, the approval mode (or OpenCode's
/// permission ruleset), the restorable points an adapter tallied ([`CHECKPOINTS_KEY`]), and
/// OpenCode's staged revert. Read off the session's metadata, so a streamed pass that saw no
/// message of it still counts it ([`Counter::finish`]).
pub fn session_rows(s: &Session) -> Vec<Affordance> {
    let mut out = Vec::new();
    if let Some(from) = &s.lineage.forked_from {
        out.push(Affordance {
            timestamp: s.created_at,
            tool: None,
            ..session_row(Kind::Fork, "fork", "", format!("forked from {from}"), Vec::new())
        });
    }
    let Some(bag) = s.harness_extra(s.harness) else {
        return out;
    };
    let modes: Vec<&str> = match (s.harness, bag.get("agent_mode")) {
        // Devin's `agent_mode` is its permission mode (`bypass`, `normal`, `accept-edits`, `plan`).
        (Harness::Devin, Some(Value::String(m))) => vec![m.as_str()],
        _ => bag
            .get(MODES_KEY)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default(),
    };
    if !modes.is_empty() {
        let tool = if s.harness == Harness::Devin {
            "agent_mode"
        } else {
            MODES_KEY
        };
        let summary = format!("mode: {}", modes.join(" → "));
        out.push(session_row(Kind::Approval, "state", tool, summary, Vec::new()));
    }
    if s.harness == Harness::OpenCode {
        // The session's permission ruleset: `[{permission, pattern, action}]`.
        let rules: Vec<Item> = bag
            .get("permission")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let what = r.get("permission").and_then(Value::as_str)?;
                let pattern = r.get("pattern").and_then(Value::as_str).unwrap_or("*");
                Some(Item {
                    content: format!("{what} {pattern}"),
                    status: r.get("action").and_then(Value::as_str).map(str::to_string),
                })
            })
            .collect();
        if !rules.is_empty() {
            let summary = items_summary(&rules).replacen(" item", " rule", 1);
            out.push(session_row(Kind::Approval, "state", "permission", summary, rules));
        }
        if let Some(rev) = bag.get("revert").and_then(Value::as_object) {
            let to = rev.get("messageID").and_then(Value::as_str).unwrap_or("?");
            let snap = rev.get("snapshot").and_then(Value::as_str).map(short).unwrap_or("");
            let summary = format!("to {to} · snapshot {snap}");
            out.push(session_row(Kind::Checkpoint, "restore", "revert", summary, Vec::new()));
        }
    }
    if let Some(cp) = bag.get(CHECKPOINTS_KEY).and_then(Value::as_object) {
        let n = cp.get("count").and_then(Value::as_u64).unwrap_or(0);
        let files: Vec<Item> = cp
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|f| Item {
                content: f.to_string(),
                status: None,
            })
            .collect();
        let mut summary = format!("{n} checkpoint{}", if n == 1 { "" } else { "s" });
        if let Some(tag) = cp.get("tag").and_then(Value::as_str) {
            summary.push_str(&format!(" · tag {tag}"));
        }
        if !files.is_empty() {
            let k = files.len();
            summary.push_str(&format!(" · {k} file{} tracked", if k == 1 { "" } else { "s" }));
        }
        if n > 0 {
            out.push(session_row(Kind::Checkpoint, "state", CHECKPOINTS_KEY, summary, files));
        }
    }
    out
}

/// Every affordance the transcript shows: the session's own rows ([`session_rows`]) first, then
/// message order. Safe on a lazy parse: text is read through the session's resolver, a few KB at
/// most per message.
pub fn of_session(s: &Session) -> Vec<Affordance> {
    let resolver = s.resolver();
    let mut out = session_rows(s);
    for (i, m) in s.messages.iter().enumerate() {
        out.extend(message_rows(s.harness, m, i, &resolver));
    }
    out
}

/// Render one message's hits identically for whole-session and streamed views.
fn message_rows(harness: Harness, m: &Message, i: usize, resolver: &crate::lazy::Resolver) -> Vec<Affordance> {
    let mut out = Vec::new();
    for hit in hits(harness, m, resolver, true) {
        out.push(match hit {
            Hit::Call {
                kind,
                action,
                id,
                name,
                input,
            } => {
                let items = if matches!(kind, Kind::Plan | Kind::Goal) {
                    items_of(input)
                } else {
                    Vec::new()
                };
                Affordance {
                    kind,
                    action,
                    index: Some(i),
                    timestamp: m.timestamp,
                    tool: Some(name.to_string()),
                    tool_use_id: Some(id.to_string()),
                    summary: call_summary(kind, input, &items),
                    items,
                    source: Source::ToolCall,
                }
            }
            Hit::Message { kind, action, tool } => Affordance {
                kind,
                action,
                index: Some(i),
                timestamp: m.timestamp,
                tool: tool.map(str::to_string),
                tool_use_id: None,
                summary: match (kind, &m.model) {
                    (Kind::ModelChange, Some(model)) => model.clone(),
                    _ => text_head(m, resolver),
                },
                items: Vec::new(),
                source: Source::Message,
            },
            Hit::Detail {
                kind,
                action,
                tool,
                tool_use_id,
                summary,
            } => Affordance {
                kind,
                action,
                index: Some(i),
                timestamp: m.timestamp,
                tool: tool.map(str::to_string),
                tool_use_id: tool_use_id.map(str::to_string),
                summary,
                items: Vec::new(),
                source: Source::Message,
            },
        });
    }
    out
}

/// The view's version, stored beside the catalog's counts: bump it whenever a kind, a table row or
/// a reader changes what [`Counter`] would count, so rows an older view counted read as "not
/// counted" and are recounted, instead of answering `has:<new kind>` with a stale no.
pub const VIEW_VERSION: u32 = 3;

/// Affordance counts by kind: what the catalog stores per session so `has:<kind>` never parses.
pub type Counts = std::collections::BTreeMap<Kind, u32>;

/// Counts a session's affordances one message at a time, inside a pass that is reading it anyway
/// (`stream::stream_counted`), with the same classification as [`of_session`].
pub struct Counter {
    harness: Harness,
    resolver: crate::lazy::Resolver,
    counts: Counts,
}

impl Counter {
    pub fn new(r: &crate::ir::SessionRef) -> Counter {
        Counter {
            harness: r.harness,
            resolver: crate::lazy::Resolver::new(Some(r.path.clone())),
            counts: Counts::new(),
        }
    }

    pub fn note(&mut self, m: &Message) {
        for hit in hits(self.harness, m, &self.resolver, false) {
            let (Hit::Call { kind, .. } | Hit::Message { kind, .. } | Hit::Detail { kind, .. }) = hit;
            *self.counts.entry(kind).or_default() += 1;
        }
    }

    /// What the messages so far showed (a resumable pass's checkpoint keeps it).
    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    /// Continue from the counts a checkpoint kept.
    pub fn restore(&mut self, counts: Counts) {
        self.counts = counts;
    }

    /// The whole session's counts: what the messages showed, the session's own rows
    /// ([`session_rows`]), and `store` (the adapter's side-store rows).
    pub fn finish(mut self, s: &Session, store: &[Affordance]) -> Counts {
        for a in session_rows(s).iter().chain(store) {
            *self.counts.entry(a.kind).or_default() += 1;
        }
        self.counts
    }
}

/// The transcript rows plus the adapter's side-store rows, for one session ref.
pub fn read(adapter: &dyn crate::harness::Adapter, r: &crate::ir::SessionRef) -> anyhow::Result<Vec<Affordance>> {
    let resolver = crate::lazy::Resolver::new(Some(r.path.clone()));
    let mut messages = Vec::new();
    let mut index = 0;
    // Retain full metadata and tool details while reducing complete content spans into bounded
    // summaries. Only the resulting rows survive each streamed message.
    let s = crate::stream::stream_counted(
        adapter,
        r,
        &crate::stream::ParseOptions::lazy_extra(),
        &mut |m: Message| {
            messages.extend(message_rows(r.harness, &m, index, &resolver));
            index += 1;
            Flow::Continue
        },
    )?;
    // Session facts can arrive at EOF; they still precede message rows, exactly as of_session.
    let mut rows = session_rows(&s);
    rows.extend(messages);
    rows.extend(adapter.store_affordances(r));
    Ok(rows)
}

/// Parse a `--kind` list of affordance words (`affordances` is all of them). `None` when any word
/// is not one.
pub fn parse_kinds(words: &[&str]) -> Option<HashSet<Kind>> {
    let mut set = HashSet::new();
    for w in words {
        if *w == "affordances" {
            set.extend(Kind::ALL.iter().copied());
        } else {
            set.insert(Kind::parse(w)?);
        }
    }
    Some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The old whole-session oracle includes EOF session facts, inline summaries beyond the lazy
    /// prefix, result details and side-store rows. Streaming must preserve all of those in order.
    #[test]
    fn streamed_read_matches_full_session_and_side_store() {
        let _world = cv_testkit::World::in_process("affordance-stream");
        struct Fixture {
            text_path: std::path::PathBuf,
        }
        impl crate::harness::Adapter for Fixture {
            fn harness(&self) -> Harness {
                Harness::Claude
            }
            fn roots(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn discover(&self) -> anyhow::Result<Vec<crate::SessionRef>> {
                Ok(Vec::new())
            }
            fn stream(
                &self,
                r: &crate::SessionRef,
                opts: &crate::ParseOptions,
                sink: &mut dyn MessageSink,
            ) -> anyhow::Result<Session> {
                assert!(opts.extra, "the view must preserve full metadata");
                let mut s = Session::new(r.id.clone(), r.harness);
                sink.meta(&s);
                // Many large irrelevant outputs must be dropped; only affordance rows survive.
                for i in 0..16 {
                    if sink.message(result(&format!("large-{i}"), &"x".repeat(256 * 1024), false, None)) == Flow::Stop {
                        return Ok(s);
                    }
                }
                let mut call = Message::new(Role::Assistant);
                call.content.push(Block::ToolUse {
                    id: "plan".into(),
                    name: "TodoWrite".into(),
                    namespace: None,
                    input: json!({"todos":[{"content":"preserve fidelity", "status":"pending"}]}),
                });
                sink.message(call);
                let mut hook = Message::new(Role::System);
                hook.origin = Origin::Hook;
                let text = if opts.spans {
                    crate::Text::Span(crate::Span {
                        source: Some(self.text_path.clone()),
                        offset: 0,
                        len: std::fs::metadata(&self.text_path)?.len(),
                        escaped: false,
                    })
                } else {
                    std::fs::read_to_string(&self.text_path)?.into()
                };
                hook.content.push(Block::Text { text });
                sink.message(hook);
                sink.message(result("denied", "Permission to use Bash has been denied", true, None));
                sink.message(result(
                    "checkpoint",
                    "output",
                    false,
                    Some(json!({"restorable":["/tmp/result.txt"]})),
                ));
                s.lineage.forked_from = Some("parent-late".into());
                note_mode(s.harness_extra_mut(Harness::Claude), "plan");
                sink.meta(&s);
                Ok(s)
            }
            fn store_affordances(&self, _: &crate::SessionRef) -> Vec<Affordance> {
                vec![Affordance::store(Kind::Plan, "todo.json", "side store", Vec::new())]
            }
        }
        let r = crate::SessionRef::new(
            "affordance-stream",
            Harness::Claude,
            "/nonexistent/affordance-stream.jsonl",
        );
        let text_path = std::env::temp_dir().join(format!("cv-affordance-stream-{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &text_path,
            format!("<summary>{}late closing tag</summary>", " ".repeat(8192)),
        )
        .unwrap();
        let fixture = Fixture {
            text_path: text_path.clone(),
        };
        let adapter: &dyn crate::harness::Adapter = &fixture;
        let full = adapter.parse(&r).unwrap();
        let mut expected = of_session(&full);
        expected.extend(adapter.store_affordances(&r));
        let actual = read(adapter, &r).unwrap();
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(actual.first().unwrap().kind, Kind::Fork);
        assert_eq!(actual.last().unwrap().source, Source::Store);
        assert!(actual.iter().any(|a| a.summary == "late closing tag"));
        assert!(actual.iter().any(|a| a.kind == Kind::Checkpoint));
        std::fs::remove_file(text_path).unwrap();
    }

    #[test]
    fn every_todo_shape_normalizes_to_items() {
        let todo = json!({"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "pending"}]});
        assert_eq!(items_of(&todo).len(), 2);
        let plan = json!({"plan": [{"step": "a", "status": "in_progress"}]});
        assert_eq!(items_of(&plan)[0].content, "a");
        let md = json!({"todos": "[x] one\n[ ] two\n[-] three"});
        let it = items_of(&md);
        assert_eq!(
            it.iter().map(|i| i.status.as_deref()).collect::<Vec<_>>(),
            [Some("completed"), Some("pending"), Some("in_progress")]
        );
        let goose = json!({"content": "1. Read a.txt\n2. Read b.txt"});
        assert_eq!(items_of(&goose).len(), 2);
        assert_eq!(items_summary(&items_of(&todo)), "2 items: 1 completed, 1 pending");
    }

    /// Each harness's `tool_call` spelling unwraps to the inner call and wraps back unchanged.
    #[test]
    fn deferred_calls_unwrap_and_rewrap_per_harness() {
        for (h, wrapped) in [
            (
                Harness::Hermes,
                json!({"name": "mcp__corpus__add", "arguments": {"a": 2}}),
            ),
            (
                Harness::OpenClaw,
                json!({"id": "sessions_spawn", "args": {"task": "ok"}}),
            ),
        ] {
            let orig = Block::ToolUse {
                id: "c".into(),
                name: DEFERRED.into(),
                input: wrapped.clone(),
                namespace: None,
            };
            let mut b = orig.clone();
            unwrap_deferred(h, &mut b);
            let Block::ToolUse { name, namespace, .. } = &b else {
                unreachable!()
            };
            assert_ne!(name, DEFERRED);
            assert_eq!(namespace.as_deref(), Some(DEFERRED));
            let Some(Block::ToolUse { name, input, .. }) = rewrap_deferred(h, &b) else {
                panic!("rewraps")
            };
            assert_eq!((name.as_str(), input), (DEFERRED, wrapped));
        }
        // Grok `plan.json` keys its todos by id.
        let grok = json!({"todos": {"t1": {"content": "a", "status": "pending"}}});
        assert_eq!(items_of(&grok).len(), 1);
        assert!(plan_state(Kind::Plan, "plan.json", items_of(&json!({"todos": {}}))).is_none());
    }

    #[test]
    fn names_are_scoped_where_they_collide() {
        let none = json!({});
        assert_eq!(
            classify_tool(Harness::OpenCode, "task", &none),
            Some((Kind::SubagentSpawn, "spawn"))
        );
        assert_eq!(classify_tool(Harness::Claude, "task", &none), None);
        assert_eq!(
            classify_tool(Harness::Qwen, "task_create", &none),
            Some((Kind::Plan, "create"))
        );
        assert_eq!(classify_tool(Harness::Claude, "Bash", &json!({"command": "ls"})), None);
        assert_eq!(
            classify_tool(Harness::Claude, "Bash", &json!({"run_in_background": true})),
            Some((Kind::BackgroundJob, "start"))
        );
        assert_eq!(
            classify_tool(Harness::Hermes, "cronjob", &json!({"action": "create"})),
            Some((Kind::Schedule, "create"))
        );
        assert_eq!(
            classify_tool(Harness::KimiCode, "TaskList", &none),
            Some((Kind::BackgroundJob, "read"))
        );
        assert_eq!(
            classify_tool(Harness::Claude, "TaskList", &none),
            Some((Kind::Plan, "read"))
        );
    }

    fn result(id: &str, text: &str, is_error: bool, details: Option<Value>) -> Message {
        let mut m = Message::new(Role::Tool);
        m.content.push(Block::ToolResult {
            tool_use_id: id.into(),
            content: text.into(),
            is_error,
            tool_name: Some("Bash".into()),
            status: None,
            details,
        });
        m
    }

    fn kinds_actions(rows: &[Affordance]) -> Vec<(Kind, &'static str)> {
        rows.iter().map(|a| (a.kind, a.action)).collect()
    }

    /// A refusal is an approval row only as the error result its harness writes; the same words
    /// in a successful result (a file that quotes them) or from another harness are not.
    #[test]
    fn a_permission_refusal_is_a_denied_approval() {
        let mut s = Session::new("s", Harness::Claude);
        s.messages = vec![
            result("a", "Permission to use Bash has been denied because …", true, None),
            result("b", "Permission to use Bash has been denied because …", false, None),
            result(
                "c",
                "The user doesn't want to proceed with this tool use. The tool use was rejected",
                true,
                None,
            ),
        ];
        let rows = of_session(&s);
        assert_eq!(
            kinds_actions(&rows),
            [(Kind::Approval, "deny"), (Kind::Approval, "deny")]
        );
        assert_eq!(rows[0].tool_use_id.as_deref(), Some("a"));
        s.harness = Harness::Codex;
        assert!(of_session(&s).is_empty());
        let mut oc = Session::new("o", Harness::OpenCode);
        oc.messages = vec![result(
            "d",
            "The user has specified a rule which prevents you from using this specific tool call.",
            true,
            None,
        )];
        assert_eq!(kinds_actions(&of_session(&oc)), [(Kind::Approval, "deny")]);
    }

    /// What a harness keeps once per session (modes, a checkpoint tally, OpenCode's ruleset and
    /// revert) are `session` rows, and the catalog counter counts them the same.
    #[test]
    fn session_facts_are_rows_the_counter_counts() {
        let mut s = Session::new("s", Harness::Claude);
        let bag = s.harness_extra_mut(Harness::Claude);
        note_mode(bag, "default");
        note_mode(bag, "plan");
        note_mode(bag, "default");
        note_checkpoints(bag, 1, ["a.txt"]);
        note_checkpoints(bag, 1, ["a.txt", "b.txt"]);
        s.messages
            .push(result("x", "ok", false, Some(json!({"restorable": ["/w/a.txt"]}))));
        let rows = of_session(&s);
        assert_eq!(
            kinds_actions(&rows),
            [
                (Kind::Approval, "state"),
                (Kind::Checkpoint, "state"),
                (Kind::Checkpoint, "create")
            ]
        );
        assert_eq!(rows[0].summary, "mode: default → plan");
        assert_eq!(rows[1].summary, "2 checkpoints · 2 files tracked");
        assert_eq!((rows[2].index, rows[2].summary.as_str()), (Some(0), "/w/a.txt"));

        let mut counter = Counter::new(&crate::ir::SessionRef::new(
            "s",
            Harness::Claude,
            std::path::PathBuf::from("/nonexistent"),
        ));
        s.messages.iter().for_each(|m| counter.note(m));
        let counts = counter.finish(&s, &[]);
        assert_eq!(counts.get(&Kind::Approval), Some(&1));
        assert_eq!(counts.get(&Kind::Checkpoint), Some(&2));

        let mut oc = Session::new("o", Harness::OpenCode);
        let bag = oc.harness_extra_mut(Harness::OpenCode);
        bag.insert(
            "permission".into(),
            json!([{"permission": "bash", "pattern": "rm *", "action": "deny"}]),
        );
        bag.insert(
            "revert".into(),
            json!({"messageID": "msg_1", "snapshot": "912f5c898f9e"}),
        );
        let rows = of_session(&oc);
        assert_eq!(
            kinds_actions(&rows),
            [(Kind::Approval, "state"), (Kind::Checkpoint, "restore")]
        );
        assert_eq!(rows[0].summary, "1 rule: 1 deny");
        assert_eq!(rows[1].summary, "to msg_1 · snapshot 912f5c89");
    }

    #[test]
    fn spawn_sink_marks_the_call_and_a_synchronous_return() {
        let mut got = Vec::new();
        let mut inner = |m: Message| {
            got.push((m.kind, m.origin));
            Flow::Continue
        };
        let mut sink = AffordanceSink::new(Harness::Hermes, &mut inner);
        for (call, result) in [("c1", "{\"answer\": \"ok\"}"), ("c2", "{\"status\": \"dispatched\"}")] {
            let mut reply = Message::new(Role::Assistant);
            reply.content.push(Block::ToolUse {
                id: call.into(),
                name: "delegate_task".into(),
                input: json!({}),
                namespace: None,
            });
            sink.message(reply);
            let mut res = Message::new(Role::Tool);
            res.content.push(Block::ToolResult {
                tool_use_id: call.into(),
                content: result.into(),
                is_error: false,
                tool_name: None,
                status: None,
                details: None,
            });
            sink.message(res);
        }
        assert_eq!(
            got,
            [
                (MessageKind::SubagentSpawn, Origin::Model),
                (MessageKind::SubagentReturn, Origin::Subagent),
                (MessageKind::SubagentSpawn, Origin::Model),
                (MessageKind::ToolResult, Origin::Harness),
            ]
        );
    }
}
