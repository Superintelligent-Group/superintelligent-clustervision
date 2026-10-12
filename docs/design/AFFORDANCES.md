# Affordances: one vocabulary for what agents do besides talk (2026-10-11)

The ask (ember): "cv should have affordances for richly querying all those things in a way that
feels natural to agents when they are within those harnesses." Plans, schedules, background jobs,
sub-agent spawns, skills, hooks, branches, forks, model switches: every harness has them, each
spells them its own way (`TodoWrite`, `update_plan`, `write_todos`, `todo_write`, `todowrite`,
`todo__todo_write`, `SetTodoList`, `Checklist`, `update_todo_list`, `todo` are one thing), and
several keep the state in a side file cv never opened. The evidence is the per-harness
`tools/harness-corpus/<h>/AFFORDANCES.md` tables and the committed corpus.

## The vocabulary

Twelve words, snake_case, each a value of `cv_core::affordance::Kind`:

| kind | what it is | actions |
|---|---|---|
| `plan` | a todo / task list or plan-mode switch | `write` (whole list), `create`, `update`, `read`, `enter`, `exit`, `state` |
| `goal` | a standing objective the harness tracks | `create`, `update`, `read`, `state` |
| `schedule` | cron jobs, wakeups, loops, heartbeats | `create`, `delete`, `list`, `fire`, `state` |
| `background_job` | a shell or task left running | `start`, `read`, `stop`, `complete` |
| `subagent_spawn` | a sub-agent launched here | `spawn` |
| `subagent_return` | a sub-agent's result delivered | `return` |
| `skill` | a skill loaded or managed | `load`, `manage` |
| `hook` | a user hook's output reached the model | `output` |
| `question` | the agent asked the person | `ask` |
| `branch` | rewind / undo / reset / clear: what follows does not continue what precedes | `branch` |
| `fork` | this session was forked from another | `fork` |
| `model_change` | the model (or effort) changed | `change` |

Not in it: approvals (no store in the corpus records a per-call approval cv can read: Roo `ask`
events live in unread `ui_messages.json`, Devin `tool_call_state` duplicates the call, Kimi carries
`permission.*` records only under `complete`, Goose refuses headless), checkpoints (Claude
`file-history-snapshot`, Kimi `_checkpoint`, Roo `checkpoint_saved` are carried, not read). Both
become kinds when a reader exists; a kind with nothing behind it is a promise, not an affordance.

## Where it lives: a derived view over the IR, plus three IR corrections

**A view, not new IR.** `cv_core::affordance::of_session(&Session) -> Vec<Affordance>` derives the
rows from what the IR already says, and `Adapter::store_affordances(&SessionRef)` (default: none)
adds the rows that live only in a side store. An `Affordance` is `{kind, action, index?, timestamp?,
tool?, tool_use_id?, summary, items[], source}`; `items` is the normalized list (`{content,
status}`) for `plan`/`goal` state, `source` is `message`, `tool_call` or `store`.

Why not `MessageKind` additions: a plan write is one `ToolUse` inside a `Reply` that may also edit a
file; giving the message a kind would lie about the rest of it, and every emitter would have to
learn twelve kinds that no target store has a record for. Why not a `Block` facet or
`extra["cv"]`: either changes the wire shape of every `show --json` message for a fact cv can
recompute from the tool name. The attractor ("a `--kind`, a `--sort`, a selector, or a route on
the one API") is met by one module, one table, one trait method, no new command.

The mapping by tool name is **one shared table** (`affordance::classify_tool`, harness-scoped where a name
is ambiguous: OpenCode `task` is a sub-agent, Qwen `task_create` is a plan item). Record-shaped
affordances come from the IR's existing kinds and origins: `SubagentSpawn`, `SubagentReturn`,
`Branch`, `ModelChange`, `Origin::Hook`, `Origin::Scheduler` (a `schedule` `fire`), and a short
record table for background completions (Claude `<task-notification>` of a command, Kimi
`<notification>`, Kimi Code origin `task`, Hermes `process_complete`).

**IR corrections** (existing kinds, assigned where adapters left them out; CHANGELOG'd):

1. `SubagentSpawn` / `SubagentReturn` by tool name, through one sink wrapper
   (`affordance::AffordanceSink`, which also does correction 3) the adapter puts around its sink: a `Reply` that calls a spawn tool is
   a `SubagentSpawn`; the `ToolResult` of a synchronous spawn is a `SubagentReturn`/`Subagent`.
   Applied to Claude, OpenCode, Hermes, OpenClaw, Goose, Grok, Roo, Cline, Devin, Continue, Kimi.
   Codex and Kimi Code already mark spawns with their own records and are left alone. A spawn turn
   is still a turn and still model-visible: `Message::is_turn` and the new
   `Message::is_model_visible` count a non-System spawn/return (dataset, distill and lanes switch
   to it), so nothing that used to reach a training row or a lane count disappears.
2. Claude `user` rows the harness wrote are no longer `Prompt`: a `<task-notification>` for an
   agent or workflow is `SubagentReturn`/`Subagent`, for a command `InjectedContext`/`Harness`; an
   interrupt marker (`[Request interrupted by user…]`) and other `isMeta` rows are
   `InjectedContext`/`Harness`. A scheduler fire stays `Prompt`/`Scheduler`.
3. Deferred-tool wrappers are unwrapped the way Qwen already is: Hermes `tool_call{name,
   arguments}` and OpenClaw 10.1 `tool_call{id, args}` become `ToolUse{name: <inner>, namespace:
   "tool_call", input: <arguments>}`, so `tools`, `events`, `tool:` and this table see the real
   tool (the table strips OpenClaw's `openclaw:core:` prefix). Both emitters wrap them back.

## The mapping (from the evidence)

| kind | tools and records, by harness |
|---|---|
| plan | Claude `TodoWrite` `TaskCreate` `TaskUpdate` `TaskList` `TaskGet` `EnterPlanMode` `ExitPlanMode` + `tasks/<sid>/*.json` (store); Codex `update_plan`; Grok `todo_write` `enter_plan_mode` `exit_plan_mode` + `plan.json` (store); OpenCode `todowrite` `todoread` + `todo` table (store); Gemini `write_todos` `tracker_*` `enter_plan_mode` `exit_plan_mode` + `tracker/*.json`, `plans/*.md` (store); Qwen `todo_write` `task_create` `task_update` `task_list` `exit_plan_mode`; Goose `todo__todo_write` + `extension_data["todo.v0"]` (store); Hermes `todo` `todo_list`; Kimi `SetTodoList` `EnterPlanMode` `ExitPlanMode`; Kimi Code `TodoList` `EnterPlanMode` `ExitPlanMode`; Continue `Checklist`; Roo `update_todo_list`; Devin `todo_write` |
| goal | Kimi Code `CreateGoal` `UpdateGoal` `GetGoal` `SetGoalBudget`; Qwen `get_goal` `update_goal`; Hermes `state_meta` `goal:<sid>` (store) |
| schedule | Claude `CronCreate` `CronList` `CronDelete` `ScheduleWakeup`; Grok `scheduler_create` `scheduler_list` `scheduler_delete`; Kimi Code `CronCreate` `CronList` `CronDelete`; Qwen `cron_create` `cron_list` `cron_delete` `loop_wakeup`; Hermes `cronjob` `cronjob_manage` + `state_meta` `loop:` / `heartbeat:` (store); OpenClaw `cron` `automations`; fires: every `Origin::Scheduler` turn (Claude wakeups, OpenClaw cron runs, Goose scheduled sessions) |
| background_job | Claude `Bash{run_in_background}` `BashOutput` `KillShell` `Monitor` + command `<task-notification>`; Grok `run_terminal_command{background}` `get_command_or_subagent_output` `monitor` `kill_command_or_subagent`; Gemini/Qwen `run_shell_command{is_background}`; Hermes `terminal{background}` `process` `process_manage` + `process_complete`; OpenClaw `process`; Kimi `TaskOutput` `TaskStop` `TaskList` + `<notification>`; Kimi Code `TaskOutput` `TaskStop` `TaskList` `WaitFor` + origin `task`; Continue `CheckBackgroundJob` |
| subagent_spawn / _return | Claude `Agent` `Task` `Workflow` (+ agent `<task-notification>` returns); OpenCode `task` `subagent`; Gemini `invoke_agent`; Qwen `agent`; Hermes `delegate_task`; OpenClaw `sessions_spawn`; Goose `delegate`; Grok `spawn_subagent`; Kimi `Agent`; Kimi Code `Agent` `AgentSwarm` (records); Roo `new_task`; Cline `spawn_agent` `team_spawn_teammate`; Devin `run_subagent`; Continue `Subagent`; Codex `SubAgentActivity` records |
| skill | Claude `Skill`; OpenCode `skill`; Gemini `activate_skill`; Qwen `skill`; Goose `load_skill`; Hermes `skill_manage`; Kimi Code `Skill`; Roo `skill`; Continue `Skills` |
| hook | every `Origin::Hook` message (Claude attachments and stop summaries, Gemini `<hook_context>`, Qwen prompt-submit context) |
| question | Claude `AskUserQuestion`; Kimi `AskUserQuestion`; Gemini `ask_user`; Qwen `ask_user_question`; Hermes `clarify`; OpenCode `question`; Roo `ask_followup_question`; Cline `ask_question`; Continue `AskQuestion` |
| branch | every `MessageKind::Branch` (Kimi `/clear`, undo; Kimi Code `/undo`; OpenClaw `reset`; Gemini `$rewindTo`) |
| fork | `Lineage::forked_from` (Codex, Grok, Hermes `/branch`, Kimi Code, Qwen, OpenCode 2.x) |
| model_change | every `MessageKind::ModelChange` (OpenClaw, OpenCode 2.x, Qwen `/model`) |

## The query surface

- **`cv show <id> --kind plan,schedule,…`**: a third `--kind` view family beside `prompt,answer`
  and the compaction seams. One row per affordance in transcript order (store rows last, and only
  when the store holds something): index, timestamp, kind, action, tool, summary, a plan's items.
  A window narrows the transcript rows. `--kind affordances` is every kind.
  `--json` prints the `Affordance` rows. MCP gets it for free (`show` with `kind`), because the
  roster is the clap tree.
- **`has:<kind>`** in the query calculus: `cv ls -q has:plan`, `cv ls -q 'has:schedule harness:hermes'`.
  The fleet question ("which sessions left a cron job behind") is a selector, not a `lanes` flag;
  `ls`, `search`, MCP `ls` and `cv serve` all take `-q`.
- **Hook body**: the `compact` hook (Claude `SessionStart` after compaction) adds the one command
  that reads back the plan, schedules and background jobs the compacted context held.

## Gaps this closes, and what it leaves

Closes: no `SubagentSpawn` in Claude/OpenCode/Hermes/OpenClaw/Goose (and Grok, Roo, Cline, Devin,
Continue, Kimi); Claude task-notification and interrupt turns typed `Prompt`; `tool_call` hiding
the real tool in Hermes and OpenClaw 10.1; unread side state for Claude `tasks/`, Grok `plan.json`,
OpenCode's `todo` table, Gemini `tracker/` and `plans/`, Goose `extension_data` todo, Hermes
`state_meta` goals, loops and heartbeats.

Leaves (named, not done here): Qwen `scheduled_tasks.json` (the `corpus-tidy` lane owns
`harness/qwen*` this round); Cline `schedules` / `teams.db`; OpenClaw `cron/jobs.json` and the
sub-agent registries; Kimi `state.json` todos and Kimi Code `tools.update_store` (carried
records); Grok `hook_execution` and `rewind_points.jsonl`; Claude `--fork-session` and Goose
fork, which record no link; OpenClaw 1.29 completions that are plain user text; Roo's
`new_task` result, which rides a mixed user turn the adapter types `Prompt` (so no
`subagent_return`); Kimi Code 0.39.1 spawns, which have no record of their own; Claude skills run
by slash command, whose only trace is the command notice and an `isMeta` body.
