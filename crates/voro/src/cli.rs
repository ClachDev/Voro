//! The command-line verb surface: every TUI action, scriptable and
//! agent-legible (DESIGN.md §9). Verbs that §8 names for the agent return
//! path (`ask`, `done`, `propose`) keep those names and flags so Milestone B
//! extends rather than renames. Parsing is hand-rolled: positionals plus
//! `--flag value` pairs is all the grammar this needs.

use std::collections::HashMap;
use std::fmt::Write as _;

use voro_core::{
    Action, AgentsConfig, DepKind, NewTask, PrRef, Priority, Project, Store, Task, TaskEdit,
    TaskState, Triage, scheduler,
};

use crate::dispatch::{self, DispatchCtx};
use crate::import;

const HELP: &str = "\
voro — prioritised attention across projects

usage: voro [--db PATH]                 launch the TUI
       voro [--db PATH] <verb> [args]

projects
  project add <name> <path>       create a project (weight 3)
  project list                    list projects with weights
  project rename <project> <name> rename a project (tasks reference it by id)
  project path <project> <path>   change a project's checkout path
  project delete <project>        delete a project with no tasks — park it
                                  (weight 0) instead to snooze one that has any
  weight <project> <0-5>          set a project's weight (0 parks it)

tasks
  add <project> <title> [--body TEXT | --body-file PATH] [--priority 0-3]
      [--state proposed|parked|ready] [--agent NAME] [--blocks IDS] [--human]
                                  --human marks a task no agent can execute:
                                  never dispatched, worked by hand, and its
                                  completion goes straight to done
  propose <project> <title> [--body TEXT | --body-file PATH] [--from TASK-ID]
                                  create a proposed task; --from (default
                                  $VORO_TASK_ID) links it discovered-from
  set <task-id> [--title T] [--priority 0-3] [--agent NAME | --no-agent]
      [--body TEXT | --body-file PATH] [--blocks IDS] [--pr URL | --no-pr]
      [--branch NAME | --no-branch] [--human | --no-human]
      [--summary TEXT | --summary-file PATH]
                                  --pr tracks a GitHub PR (URL or owner/repo#N)
                                  for review; --no-pr clears it. --branch sets
                                  the git branch dispatch injects into the
                                  prompt; --no-branch clears it. --summary
                                  sets or replaces a running/review task's
                                  completion summary (the PR body `pr` opens
                                  from) without a reject/done round trip
  show <task-id>                  full task: body, deps, events
  list [--state STATE] [--project P]
  inbox                           the next-action queue: questions, reviews,
                                  proposals, top ready tasks — by score
  next                            the single highest-scoring ready task
  stats                           task counts by state — the triage backlog
                                  (§12) plus ready, running, needs-input,
                                  review, done; excludes parked projects
  explain <task-id>               score decomposition
  import <project> [--repo owner/name]
                                  import open GitHub issues as proposed
                                  tasks via `gh issue list`; idempotent,
                                  --repo overrides the checkout's own remote

dispatch
  agent init                      write an optional voro.toml skeleton for
                                  extending/overriding the built-ins (won't
                                  overwrite an existing one)
  agent list                      list effective agents (built-in + user)
                                  with provenance; * marks the default
  agent path                      print where dispatch looks for voro.toml
  dispatch <task-id> [--agent NAME]
                                  spawn a headless agent session on a ready
                                  task; --agent overrides the resolved agent
  continue <task-id> [--agent NAME]
                                  continue an already-running task without
                                  changing its state — via the agent's
                                  `continue` verb when its session was
                                  captured, else a fresh session; what
                                  `answer` does automatically for a
                                  previously-dispatched task, exposed to retry
                                  a continuation that failed
  open <task-id>                  open a review/running task's checkout in the
                                  configured [viewer] (voro.toml) to see its
                                  diff — reports what to configure if none is set
  pr <task-id> [--yes]            with a tracked PR, open it in a browser
                                  (jump-to-PR); with none, push the review
                                  task's branch and open a ready PR from its
                                  summary, recording the URL (--yes skips the
                                  confirm). Track an existing one with `set --pr`

transitions
  triage <task-id> <parked|ready|reject>
  start <task-id>                 ready → running
  ask <task-id> --question TEXT   running → needs-input
  answer <task-id> TEXT [--no-dispatch]
                                  needs-input → running; if the task has ever
                                  been dispatched, also starts a continuation
                                  session with the answer in the prompt body
                                  (--no-dispatch skips that)
  done <task-id> [--summary TEXT | --summary-file PATH] [--branch NAME]
                                  running → review; the summary is the agent's
                                  completion note (kept as a summary event, and
                                  the PR body `pr` opens from) — write it as a
                                  PR description; --branch records the git branch
                                  the work landed on (agent return path). Warns
                                  but succeeds when branch or summary is absent
  accept <task-id> [--yes]        review → done; then offers to remove the
                                  task's dispatch worktree (--yes skips the
                                  confirmation)
  reject <task-id> [TEXT] [--from-pr]
                                  review → running; TEXT is the feedback, or
                                  --from-pr pulls the tracked PR's review
                                  comments as the feedback (TEXT appended)
  abort <task-id>                 running → ready
  park <task-id>                  ready → parked
  unpark <task-id>                parked → ready
  abandon <task-id> [--yes]       parked|ready|needs-input|review → rejected;
                                  then offers to remove the task's worktree
";

pub fn run(store: &mut Store, args: Vec<String>, ctx: &DispatchCtx) -> Result<String, String> {
    // Reconcile-on-read (DESIGN.md §8): before any verb consults session or
    // task state, close out sessions whose process has already exited.
    crate::reconcile::reconcile_live_sessions(store, &ctx.agents_path)
        .map_err(|e| e.to_string())?;

    let (pos, flags) = split_args(args)?;
    let verb = pos.first().map(String::as_str).unwrap_or("help");
    match verb {
        "help" | "--help" | "-h" => Ok(HELP.to_string()),
        "project" => project_verb(store, &pos, &flags),
        "weight" => weight_verb(store, &pos),
        "add" => add_verb(store, &pos, &flags),
        "propose" => propose_verb(store, &pos, &flags, std::env::var("VORO_TASK_ID").ok()),
        "set" => set_verb(store, &pos, &flags),
        "show" => show_verb(store, &pos),
        "list" => list_verb(store, &flags),
        "inbox" => inbox_verb(store),
        "next" => next_verb(store),
        "stats" => stats_verb(store),
        "explain" => explain_verb(store, &pos),
        "agent" => agent_verb(&pos, ctx),
        "dispatch" => dispatch_verb(store, &pos, &flags, ctx),
        "continue" => continue_verb(store, &pos, &flags, ctx),
        "open" => open_verb(store, &pos, ctx),
        "pr" => pr_verb(store, &pos, &flags),
        "reject" => reject_verb(store, &pos, &flags, ctx),
        "done" => done_verb(store, &pos, &flags),
        "answer" => answer_verb(store, &pos, &flags, ctx),
        "import" => import_verb(store, &pos, &flags),
        "triage" | "start" | "ask" | "accept" | "abort" | "park" | "unpark" | "abandon" => {
            transition_verb(store, verb, &pos, &flags)
        }
        other => Err(format!("unknown verb '{other}' — try 'voro help'")),
    }
}

fn split_args(args: Vec<String>) -> Result<(Vec<String>, HashMap<String, String>), String> {
    let mut pos = Vec::new();
    let mut flags = HashMap::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.strip_prefix("--") {
            Some("no-agent") => {
                flags.insert("no-agent".to_string(), String::new());
            }
            Some("no-dispatch") => {
                flags.insert("no-dispatch".to_string(), String::new());
            }
            Some("no-pr") => {
                flags.insert("no-pr".to_string(), String::new());
            }
            Some("no-branch") => {
                flags.insert("no-branch".to_string(), String::new());
            }
            Some("human") => {
                flags.insert("human".to_string(), String::new());
            }
            Some("no-human") => {
                flags.insert("no-human".to_string(), String::new());
            }
            Some("from-pr") => {
                flags.insert("from-pr".to_string(), String::new());
            }
            Some("yes") => {
                flags.insert("yes".to_string(), String::new());
            }
            Some("help") => pos.insert(0, "help".to_string()),
            Some(key) => {
                let value = it.next().ok_or_else(|| format!("--{key} needs a value"))?;
                flags.insert(key.to_string(), value);
            }
            None => pos.push(arg),
        }
    }
    Ok((pos, flags))
}

fn need<'a>(pos: &'a [String], index: usize, what: &str) -> Result<&'a str, String> {
    pos.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {what} — try 'voro help'"))
}

fn task_id(pos: &[String], index: usize) -> Result<i64, String> {
    let raw = need(pos, index, "task id")?;
    raw.parse().map_err(|_| format!("'{raw}' is not a task id"))
}

fn resolve_project(store: &Store, key: &str) -> Result<Project, String> {
    let projects = store.projects().map_err(|e| e.to_string())?;
    if let Ok(id) = key.parse::<i64>()
        && let Some(p) = projects.iter().find(|p| p.id == id)
    {
        return Ok(p.clone());
    }
    projects
        .into_iter()
        .find(|p| p.name == key)
        .ok_or_else(|| format!("no project named '{key}'"))
}

fn parse_priority(raw: &str) -> Result<Priority, String> {
    let n: i64 = raw
        .trim_start_matches(['p', 'P'])
        .parse()
        .map_err(|_| format!("priority must be 0-3, got '{raw}'"))?;
    Priority::from_int(n).map_err(|e| e.to_string())
}

fn parse_blocks(raw: &str) -> Result<Vec<i64>, String> {
    raw.split([',', ' '])
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse()
                .map_err(|_| format!("blocks must be task ids, got '{s}'"))
        })
        .collect()
}

fn body_from(flags: &HashMap<String, String>) -> Result<Option<String>, String> {
    match (flags.get("body"), flags.get("body-file")) {
        (Some(_), Some(_)) => Err("--body and --body-file are mutually exclusive".into()),
        (Some(text), None) => Ok(Some(text.clone())),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("cannot read {path}: {e}")),
        (None, None) => Ok(None),
    }
}

/// The agent's completion summary from `--summary TEXT` or `--summary-file
/// PATH` (DESIGN.md §8), mutually exclusive — the latter lets an agent leave a
/// PR-ready description without cramming it onto one command line. `None` when
/// neither is given, which stays valid: not every task produces one.
fn summary_from(flags: &HashMap<String, String>) -> Result<Option<String>, String> {
    match (flags.get("summary"), flags.get("summary-file")) {
        (Some(_), Some(_)) => Err("--summary and --summary-file are mutually exclusive".into()),
        (Some(text), None) => Ok(Some(text.clone())),
        (None, Some(path)) => std::fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("cannot read {path}: {e}")),
        (None, None) => Ok(None),
    }
}

fn rest_text(pos: &[String], from: usize, what: &str) -> Result<String, String> {
    let text = pos[from.min(pos.len())..].join(" ");
    if text.trim().is_empty() {
        return Err(format!("missing {what} — try 'voro help'"));
    }
    Ok(text)
}

// --- verbs ---

fn project_verb(
    store: &mut Store,
    pos: &[String],
    _flags: &HashMap<String, String>,
) -> Result<String, String> {
    match need(pos, 1, "project subcommand (add|list|rename|path|delete)")? {
        "add" => {
            let name = need(pos, 2, "project name")?;
            let path = need(pos, 3, "project path")?;
            let p = store
                .create_project(name, path)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} '{}' created (weight {})",
                p.id, p.name, p.weight
            ))
        }
        "list" => {
            let mut out = String::new();
            for p in store.projects().map_err(|e| e.to_string())? {
                writeln!(out, "{:3}  w{}  {}  {}", p.id, p.weight, p.name, p.path).unwrap();
            }
            Ok(out)
        }
        "rename" => {
            let project = resolve_project(store, need(pos, 2, "project")?)?;
            let name = need(pos, 3, "new name")?;
            let p = store
                .rename_project(project.id, name)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} renamed '{}' -> '{}'",
                p.id, project.name, p.name
            ))
        }
        "path" => {
            let project = resolve_project(store, need(pos, 2, "project")?)?;
            let path = need(pos, 3, "new path")?;
            let p = store
                .set_path(project.id, path)
                .map_err(|e| e.to_string())?;
            Ok(format!("project {} path -> {}", p.id, p.path))
        }
        "delete" => {
            let project = resolve_project(store, need(pos, 2, "project")?)?;
            store
                .delete_project(project.id)
                .map_err(|e| e.to_string())?;
            Ok(format!("project {} '{}' deleted", project.id, project.name))
        }
        other => Err(format!("unknown project subcommand '{other}'")),
    }
}

/// Manage the `voro.toml` that dispatch resolves against — the config that
/// lives outside the database (DESIGN.md §8), so this verb takes no `store`.
/// `init` scaffolds a starter file, `list` shows the effective agents, and
/// `path` prints where dispatch looks for it.
fn agent_verb(pos: &[String], ctx: &DispatchCtx) -> Result<String, String> {
    let path = &ctx.agents_path;
    match need(pos, 1, "agent subcommand (init|list|path)")? {
        "init" => {
            AgentsConfig::write_starter(path).map_err(|e| e.to_string())?;
            Ok(format!(
                "wrote a config skeleton to {} — optional, since the built-in claude/codex \
                 agents already dispatch; edit it to add or override agents, or set options \
                 like `default_agent` and `[viewer]`",
                path.display()
            ))
        }
        "list" => {
            let config = AgentsConfig::load(path).map_err(|e| e.to_string())?;
            let default = config.default_name();
            let mut out = String::new();
            for (name, template, provenance) in config.entries() {
                let marker = if Some(name) == default.as_deref() {
                    "* "
                } else {
                    "  "
                };
                let verbs: Vec<&str> = [
                    ("sessions", template.sessions()),
                    ("attach", template.attach()),
                    ("resume", template.resume()),
                    ("continue", template.continue_cmd()),
                ]
                .into_iter()
                .filter_map(|(verb, defined)| defined.map(|_| verb))
                .collect();
                let suffix = if verbs.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", verbs.join(" "))
                };
                writeln!(
                    out,
                    "{marker}{name}  {}{suffix}  ({})",
                    template.dispatch(),
                    provenance.label()
                )
                .unwrap();
                let missing = config.override_missing_verbs(name);
                if !missing.is_empty() {
                    writeln!(
                        out,
                        "    ! overrides the built-in {name} but drops: {} — those verbs no \
                         longer work; copy them from the built-in if you want them",
                        missing.join(", ")
                    )
                    .unwrap();
                }
            }
            match default {
                Some(_) => writeln!(out, "\n({} — * is the default)", path.display()).unwrap(),
                None => writeln!(
                    out,
                    "\n({} — no default agent resolved; install claude/codex or set \
                     `default_agent`)",
                    path.display()
                )
                .unwrap(),
            }
            Ok(out)
        }
        "path" => Ok(path.display().to_string()),
        other => Err(format!("unknown agent subcommand '{other}'")),
    }
}

fn weight_verb(store: &mut Store, pos: &[String]) -> Result<String, String> {
    let project = resolve_project(store, need(pos, 1, "project")?)?;
    let weight: i64 = need(pos, 2, "weight (0-5)")?
        .parse()
        .map_err(|_| "weight must be 0-5".to_string())?;
    store
        .set_weight(project.id, weight)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{} weight {} -> {}",
        project.name, project.weight, weight
    ))
}

fn add_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let project = resolve_project(store, need(pos, 1, "project")?)?;
    let title = rest_text(pos, 2, "title")?;
    let state = match flags.get("state").map(String::as_str) {
        None => TaskState::Proposed,
        Some(raw) => {
            let state = TaskState::parse(raw).map_err(|e| e.to_string())?;
            if !matches!(
                state,
                TaskState::Proposed | TaskState::Parked | TaskState::Ready
            ) {
                return Err(format!("a task cannot be created in state '{state}'"));
            }
            state
        }
    };
    let priority = match flags.get("priority") {
        Some(raw) => parse_priority(raw)?,
        None => Priority::P2,
    };
    let task = store
        .create_task(NewTask {
            project_id: project.id,
            title,
            body: body_from(flags)?.unwrap_or_default(),
            priority,
            state,
            agent: flags.get("agent").cloned(),
            human: flags.contains_key("human"),
        })
        .map_err(|e| e.to_string())?;
    let task = match flags.get("blocks") {
        Some(raw) => store
            .set_blocks_deps(task.id, &parse_blocks(raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    Ok(format!(
        "task {} '{}' created ({})",
        task.id, task.title, task.state
    ))
}

/// The agent return-path form of `add` (DESIGN.md §8): always lands in
/// `proposed`, and links the new task discovered-from its source — `--from`
/// explicitly, or the `VORO_TASK_ID` a dispatched session runs under.
fn propose_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
    env_source: Option<String>,
) -> Result<String, String> {
    if flags.contains_key("state") {
        return Err("propose always creates 'proposed' tasks — use 'add --state' instead".into());
    }
    let project = resolve_project(store, need(pos, 1, "project")?)?;
    let title = rest_text(pos, 2, "title")?;
    let source = match flags.get("from").cloned().or(env_source) {
        Some(raw) => {
            let id: i64 = raw
                .parse()
                .map_err(|_| format!("'{raw}' is not a task id"))?;
            Some(store.task(id).map_err(|e| e.to_string())?)
        }
        None => None,
    };
    let task = store
        .create_task(NewTask {
            project_id: project.id,
            title,
            body: body_from(flags)?.unwrap_or_default(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
        })
        .map_err(|e| e.to_string())?;
    let mut out = format!("task {} '{}' proposed", task.id, task.title);
    if let Some(source) = source {
        store
            .add_dep(task.id, source.id, DepKind::DiscoveredFrom)
            .map_err(|e| e.to_string())?;
        write!(out, " (discovered from #{})", source.id).unwrap();
    }
    Ok(out)
}

fn set_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let current = store.task(id).map_err(|e| e.to_string())?;
    let agent = if flags.contains_key("no-agent") {
        None
    } else {
        flags.get("agent").cloned().or(current.agent)
    };
    let human = match (flags.contains_key("human"), flags.contains_key("no-human")) {
        (true, true) => return Err("--human and --no-human are mutually exclusive".into()),
        (true, false) => true,
        (false, true) => false,
        (false, false) => current.human,
    };
    let edit = TaskEdit {
        title: flags.get("title").cloned().unwrap_or(current.title),
        body: body_from(flags)?.unwrap_or(current.body),
        priority: match flags.get("priority") {
            Some(raw) => parse_priority(raw)?,
            None => current.priority,
        },
        agent,
        human,
    };
    let task = store.update_task(id, edit).map_err(|e| e.to_string())?;
    let task = match flags.get("blocks") {
        Some(raw) => store
            .set_blocks_deps(id, &parse_blocks(raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    let task = match (flags.contains_key("no-pr"), flags.get("pr")) {
        (true, Some(_)) => return Err("--pr and --no-pr are mutually exclusive".into()),
        (true, None) => store.set_pr(id, None).map_err(|e| e.to_string())?,
        (false, Some(raw)) => {
            // Validate and canonicalise the reference before storing, so a
            // tracked PR is always addressable and the stored form is stable.
            let pr = PrRef::parse(raw).map_err(|e| e.to_string())?;
            store.set_pr(id, Some(&pr.url)).map_err(|e| e.to_string())?
        }
        (false, None) => task,
    };
    let task = match (flags.contains_key("no-branch"), flags.get("branch")) {
        (true, Some(_)) => return Err("--branch and --no-branch are mutually exclusive".into()),
        (true, None) => store.set_branch(id, None).map_err(|e| e.to_string())?,
        (false, Some(name)) => store
            .set_branch(id, Some(name.trim()))
            .map_err(|e| e.to_string())?,
        (false, None) => task,
    };
    let task = match summary_from(flags)? {
        Some(text) => store.set_summary(id, &text).map_err(|e| e.to_string())?,
        None => task,
    };
    Ok(format!("task {} updated ({})", task.id, task.state))
}

/// `pr <task-id> [--yes]` (DESIGN.md §8/§11c): with a tracked PR, open it in a
/// browser — the jump-to-PR that lands on the diff and its comments. Without
/// one, *create* it from the review task's done-time state: assert it is
/// PR-ready (naming any missing state, branch, or summary), confirm with the
/// operator unless `--yes`, then push the branch and open a non-draft PR whose
/// body is the completion summary, recording its URL. The `git`/`gh` shell-outs
/// live in the `pr` module beside the other process work.
fn pr_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let task = store.task(id).map_err(|e| e.to_string())?;
    if task.pr_url.is_some() {
        return crate::pr::open(store, id);
    }
    // Assert PR-ready and learn the branch before prompting, so a task missing
    // state, branch, or summary fails naming the gap rather than at the prompt.
    let plan = crate::pr::plan(store, id)?;
    if !flags.contains_key("yes")
        && !confirm(&format!("push `{}` and open a PR for #{id}?", plan.branch))?
    {
        return Ok(format!("cancelled — no PR opened for #{id}"));
    }
    crate::pr::create(store, id)
}

/// Ask a yes/no question on the terminal, defaulting to no (DESIGN.md §8): the
/// interactive gate before `pr` pushes a branch and opens a PR. Mirrors the
/// TUI's confirmation modal; `--yes` skips it. A non-interactive stdin (a pipe
/// at EOF) reads as "no", so a scripted run without `--yes` declines rather
/// than blocking.
fn confirm(question: &str) -> Result<bool, String> {
    use std::io::Write as _;
    print!("{question} [y/N] ");
    std::io::stdout()
        .flush()
        .map_err(|e| format!("cannot write to stdout: {e}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("cannot read confirmation: {e}"))?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// `reject <task-id> [TEXT] [--from-pr] [--no-dispatch]` (DESIGN.md §6/§8/§11c):
/// review → running with feedback. `--from-pr` pulls the tracked PR's review
/// comments as that feedback so a GitHub review reaches the agent without
/// retyping; any extra TEXT is appended below them. Without the flag, TEXT is
/// the feedback. Like `answer`, a task with prior session history then continues
/// the work with the feedback in hand — and because review keeps the session
/// open, that reuses the *same* agent session; `--no-dispatch` forces the plain
/// transition, and a task only ever started by hand has nothing to continue.
fn reject_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
    ctx: &DispatchCtx,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let feedback = if flags.contains_key("from-pr") {
        let pulled = crate::pr::pull_review_feedback(store, id)?;
        match pos.get(2..).map(|rest| rest.join(" ")) {
            Some(extra) if !extra.trim().is_empty() => format!("{pulled}\n{extra}"),
            _ => pulled,
        }
    } else {
        rest_text(pos, 2, "rejection feedback")?
    };
    let has_history = !store
        .sessions_for(id)
        .map_err(|e| e.to_string())?
        .is_empty();

    let task = store
        .apply(id, Action::RejectWork(feedback.clone()))
        .map_err(|e| e.to_string())?;
    let out = format!("task {} -> {}", task.id, task.state);

    if !has_history || flags.contains_key("no-dispatch") {
        return Ok(out);
    }
    match dispatch::continue_dispatch(store, ctx, id, None, Some(&feedback)) {
        Ok(summary) => Ok(format!("{out}; {summary}")),
        Err(e) => Err(format!(
            "{out}, but continuation dispatch failed: {e} — retry with 'voro continue {id}'"
        )),
    }
}

/// `done <task-id> [--summary TEXT | --summary-file PATH] [--branch NAME]`
/// (DESIGN.md §6/§8): running → review. The summary is the agent's completion
/// note, kept as a summary event by the transition and read back as the PR body
/// when `pr` opens a pull request — so it should read as a PR description
/// (`--summary-file` lets the agent write a multi-line one without cramming a
/// command line). `--branch` is the branch the agent reports its work landed on
/// (task #81), recorded on the task (overwriting any intended name dispatch
/// injected) so the task correlates with its branch and PR. The completion
/// transition is applied first, so a task that is not `running` is refused
/// before any branch is recorded. Voro never runs git; it only stores the
/// reported name. A `done` that leaves the task without a branch or summary
/// *warns* rather than failing — planning and task-generation work produces
/// neither — so both stay optional through the whole lifecycle.
fn done_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let summary = summary_from(flags)?;
    let task = store
        .apply(id, Action::Complete(summary))
        .map_err(|e| e.to_string())?;
    let mut out = format!("task {} -> {}", task.id, task.state);
    if let Some(name) = flags.get("branch") {
        store
            .set_branch(id, Some(name.trim()))
            .map_err(|e| e.to_string())?;
        write!(out, " (branch {})", name.trim()).unwrap();
    }
    // A PR needs both a branch and a summary; warn (never fail) about whichever
    // is absent so the operator knows `pr` will not yet open one. A human task
    // lands straight in `done` with no PR to open, so it earns no warning.
    if task.state == TaskState::Review {
        let has_branch = store.task(id).map_err(|e| e.to_string())?.branch.is_some();
        let has_summary = store
            .latest_summary(id)
            .map_err(|e| e.to_string())?
            .is_some();
        let missing: Vec<&str> = [("branch", has_branch), ("summary", has_summary)]
            .into_iter()
            .filter_map(|(what, present)| (!present).then_some(what))
            .collect();
        if !missing.is_empty() {
            write!(
                out,
                "\nnote: no {} recorded — `voro pr {id}` needs a branch and summary to open a PR",
                missing.join(" or ")
            )
            .unwrap();
        }
    }
    Ok(out)
}

fn show_verb(store: &mut Store, pos: &[String]) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let task = store.task(id).map_err(|e| e.to_string())?;
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    let mut out = String::new();
    writeln!(out, "{}", task_line(&task, &project.name)).unwrap();
    writeln!(
        out,
        "created {}   in state since {}",
        task.created_at, task.state_since
    )
    .unwrap();
    if task.human {
        writeln!(
            out,
            "human-only: never dispatched; completion goes straight to done"
        )
        .unwrap();
    }
    if let Some(agent) = &task.agent {
        writeln!(out, "agent override: {agent}").unwrap();
    }
    if let Some(q) = &task.question {
        writeln!(out, "question: {q}").unwrap();
    }
    if let Some(pr) = &task.pr_url {
        writeln!(out, "pr: {pr}").unwrap();
    }
    if let Some(branch) = &task.branch {
        writeln!(out, "branch: {branch}").unwrap();
    }
    if store
        .incomplete_report_flag(id)
        .map_err(|e| e.to_string())?
    {
        writeln!(
            out,
            "incomplete report: needs a branch and summary before `voro pr {id}`"
        )
        .unwrap();
    }
    let deps = store.deps_of(id).map_err(|e| e.to_string())?;
    for dep in &deps {
        writeln!(out, "dep: {} {}", dep.kind, dep.depends_on).unwrap();
    }
    if !task.body.is_empty() {
        writeln!(out, "\n{}", task.body).unwrap();
    }
    writeln!(out, "\nevents:").unwrap();
    for e in store.events_for(id).map_err(|e| e.to_string())? {
        writeln!(
            out,
            "  {}  {}  {}",
            e.at,
            e.kind,
            e.detail.unwrap_or_default()
        )
        .unwrap();
    }
    Ok(out)
}

fn list_verb(store: &mut Store, flags: &HashMap<String, String>) -> Result<String, String> {
    let state_filter = match flags.get("state") {
        Some(raw) => Some(TaskState::parse(raw).map_err(|e| e.to_string())?),
        None => None,
    };
    let project_filter = match flags.get("project") {
        Some(key) => Some(resolve_project(store, key)?.id),
        None => None,
    };
    let projects = store.projects().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for task in store.tasks().map_err(|e| e.to_string())? {
        if state_filter.is_some_and(|s| task.state != s)
            || project_filter.is_some_and(|p| task.project_id != p)
        {
            continue;
        }
        let name = projects
            .iter()
            .find(|p| p.id == task.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        writeln!(
            out,
            "{}{}",
            task_line(&task, name),
            incomplete_report_suffix(store, task.id)
        )
        .unwrap();
    }
    Ok(out)
}

fn inbox_verb(store: &mut Store) -> Result<String, String> {
    let candidates = store.candidates().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for c in scheduler::queue(&candidates) {
        write!(
            out,
            "{:5.1}  {}",
            c.score.total,
            task_line(&c.task, &c.project_name)
        )
        .unwrap();
        if let Some(q) = &c.task.question {
            write!(out, "  — {q}").unwrap();
        }
        write!(out, "{}", incomplete_report_suffix(store, c.task.id)).unwrap();
        writeln!(out).unwrap();
    }
    if out.is_empty() {
        out = "nothing needs you\n".to_string();
    }
    Ok(out)
}

/// The always-visible untriaged count (DESIGN.md §12) and its companions, as a
/// scriptable readout: task counts by state, excluding parked projects so the
/// numbers match the queue and header. `triage` is the untriaged-proposal
/// guard rail; the rest give the other queues a running tally.
fn stats_verb(store: &mut Store) -> Result<String, String> {
    let c = store.state_counts().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for (label, n) in [
        ("triage", c.proposed),
        ("ready", c.ready),
        ("running", c.running),
        ("needs-input", c.needs_input),
        ("review", c.review),
        ("stalled", c.stalled),
        ("done", c.done),
    ] {
        writeln!(out, "{label:<12}{n}").unwrap();
    }
    Ok(out)
}

fn next_verb(store: &mut Store) -> Result<String, String> {
    let candidates = store.candidates().map_err(|e| e.to_string())?;
    match scheduler::focus(&candidates) {
        Some(c) => {
            let mut out = String::new();
            writeln!(
                out,
                "{:5.1}  {}{}",
                c.score.total,
                task_line(&c.task, &c.project_name),
                incomplete_report_suffix(store, c.task.id)
            )
            .unwrap();
            if !c.task.body.is_empty() {
                writeln!(out, "\n{}", c.task.body).unwrap();
            }
            Ok(out)
        }
        None => Ok("no ready tasks\n".to_string()),
    }
}

/// `  [incomplete report]` when a `review` task carries only one of a branch
/// and a summary (DESIGN.md §8), else empty — the half-finished done report a
/// dispatched session left behind, surfaced so the operator sees it rather than
/// hitting the gap at `pr` time. Re-derived per line, never stored.
fn incomplete_report_suffix(store: &Store, task_id: i64) -> &'static str {
    if store.incomplete_report_flag(task_id).unwrap_or(false) {
        "  [incomplete report]"
    } else {
        ""
    }
}

fn explain_verb(store: &mut Store, pos: &[String]) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let task = store.task(id).map_err(|e| e.to_string())?;
    let b = store.explain(id).map_err(|e| e.to_string())?;
    let mut out = String::new();
    writeln!(out, "task {}  '{}'  ({})", task.id, task.title, task.state).unwrap();
    writeln!(out, "weight          {:>6}", b.weight).unwrap();
    writeln!(
        out,
        "priority        {:>6}  (value {})",
        b.priority.to_string(),
        b.priority_value
    )
    .unwrap();
    writeln!(
        out,
        "state           {:>6}  (bonus +{})",
        b.state.to_string(),
        b.state_bonus
    )
    .unwrap();
    writeln!(out, "base w×(p+s)    {:>6.1}", b.base).unwrap();
    writeln!(out, "age             {:>6.1} days", b.age_days).unwrap();
    writeln!(
        out,
        "age bonus       {:>6.2}  (0.1/day, cap 2)",
        b.age_bonus
    )
    .unwrap();
    writeln!(out, "total           {:>6.2}", b.total).unwrap();
    if !matches!(
        task.state,
        TaskState::Ready | TaskState::NeedsInput | TaskState::Review | TaskState::Stalled
    ) {
        writeln!(out, "({} tasks are not scheduled)", task.state).unwrap();
    }
    Ok(out)
}

fn dispatch_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
    ctx: &DispatchCtx,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    dispatch::dispatch(store, ctx, id, flags.get("agent").map(String::as_str))
}

/// `continue <task-id> [--agent NAME]`: spawn a fresh session on a task that
/// is already `running`, without touching its state — the mechanics `answer`
/// uses automatically for a previously-dispatched task, exposed directly so a
/// continuation that failed (a dirty tree, a misconfigured agent) can be
/// retried once fixed.
fn continue_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
    ctx: &DispatchCtx,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    dispatch::continue_dispatch(store, ctx, id, flags.get("agent").map(String::as_str), None)
}

/// `answer <task-id> TEXT [--no-dispatch]` (DESIGN.md §6): needs-input →
/// running, answer appended to the body and logged — the transition alone
/// always applies first. "fed to the session" then means dispatching a fresh
/// continuation whose prompt carries the `## Answers` section, not writing to
/// a live pipe: by the time a human answers, the asking session has typically
/// already exited. That continuation only makes sense for a task with prior
/// session history — one only ever started by hand has nothing to continue,
/// and stays a plain transition, which is also what `--no-dispatch` forces
/// even when history exists.
fn answer_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
    ctx: &DispatchCtx,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let text = rest_text(pos, 2, "answer text")?;
    let has_history = !store
        .sessions_for(id)
        .map_err(|e| e.to_string())?
        .is_empty();

    let task = store
        .apply(id, Action::Answer(text.clone()))
        .map_err(|e| e.to_string())?;
    let out = format!("task {} -> {}", task.id, task.state);

    if !has_history || flags.contains_key("no-dispatch") {
        return Ok(out);
    }
    match dispatch::continue_dispatch(store, ctx, id, None, Some(&text)) {
        Ok(summary) => Ok(format!("{out}; {summary}")),
        Err(e) => Err(format!(
            "{out}, but continuation dispatch failed: {e} — retry with 'voro continue {id}'"
        )),
    }
}

/// `open <task-id>` (DESIGN.md §11a): run the configured `[viewer]` command on
/// a review/running task's checkout so its diff can be seen. Spawning lives in
/// the dispatch module beside the other process work; `voro-core` stays
/// process-free.
fn open_verb(store: &mut Store, pos: &[String], ctx: &DispatchCtx) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    dispatch::open(store, ctx, id)
}

/// Milestone C's one-way GitHub import (DESIGN.md §10): shells out to `gh
/// issue list` in the project's path (or `--repo owner/name` if the checkout
/// itself doesn't name the repo to import from) and captures each issue as a
/// `proposed` task, skipping ones already imported.
fn import_verb(
    store: &mut Store,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let project = resolve_project(store, need(pos, 1, "project")?)?;
    let repo = flags.get("repo").map(String::as_str);
    let json = import::fetch_issues(&project.path, repo)?;
    let summary = import::import_issues(store, &project, &json)?;
    let mut out = String::new();
    for task in &summary.imported {
        writeln!(out, "{}", task_line(task, &project.name)).unwrap();
    }
    writeln!(
        out,
        "{} imported, {} already present",
        summary.imported.len(),
        summary.skipped
    )
    .unwrap();
    Ok(out)
}

fn transition_verb(
    store: &mut Store,
    verb: &str,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let action = match verb {
        "triage" => match need(pos, 2, "triage target (parked|ready|reject)")? {
            "parked" => Action::Triage(Triage::Parked),
            "ready" => Action::Triage(Triage::Ready),
            "reject" => Action::Triage(Triage::Reject),
            other => {
                return Err(format!(
                    "triage target must be parked|ready|reject, got '{other}'"
                ));
            }
        },
        "start" => Action::Start,
        "ask" => {
            let question = match flags.get("question") {
                Some(q) => q.clone(),
                None => rest_text(pos, 2, "question (--question TEXT)")?,
            };
            Action::Ask(question)
        }
        "accept" => Action::Accept,
        "abort" => Action::Abort,
        "park" => Action::Park,
        "unpark" => Action::Unpark,
        "abandon" => Action::Abandon,
        _ => unreachable!("guarded by run()"),
    };
    // The task closes worktree in tow (§8): `accept`/`abandon` are the terminal
    // transitions that own the dispatch worktree's teardown. The transition is
    // applied first and stands regardless of what the cleanup does.
    let closes = matches!(action, Action::Accept | Action::Abandon);
    let task = store.apply(id, action).map_err(|e| e.to_string())?;
    let mut out = format!("task {} -> {}", task.id, task.state);
    if closes && let Some(line) = clean_up_worktree(store, &task, flags.contains_key("yes"))? {
        out.push('\n');
        out.push_str(&line);
    }
    Ok(out)
}

/// Remove the worktree of a just-closed task after showing the operator exactly
/// what will go (worktree path, branch, why the branch is judged safe) and
/// confirming — `--yes` skips the prompt for scripting. Declining still returns
/// `Ok`, so the transition it followed stands; `None` means there was nothing to
/// clean (no branch, or no matching worktree). The git/`gh` work lives in the
/// `worktree` module beside dispatch.
fn clean_up_worktree(store: &Store, task: &Task, yes: bool) -> Result<Option<String>, String> {
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    let Some(plan) = crate::worktree::Cleanup::plan(task, &project.path)? else {
        return Ok(None);
    };
    if !yes && !confirm(&format!("{} — proceed?", plan.describe()))? {
        return Ok(Some(format!(
            "worktree {} left in place — cleanup declined",
            plan.worktree().display()
        )));
    }
    Ok(Some(plan.execute()))
}

fn task_line(task: &Task, project: &str) -> String {
    format!(
        "#{} {} {} {}: {}",
        task.id, task.state, task.priority, project, task.title
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn ctx() -> DispatchCtx {
        DispatchCtx::from_db_path(std::path::Path::new("/nonexistent/voro.db"))
    }

    fn ok(store: &mut Store, args: &[&str]) -> String {
        run(store, args.iter().map(|s| s.to_string()).collect(), &ctx())
            .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
    }

    fn err(store: &mut Store, args: &[&str]) -> String {
        run(store, args.iter().map(|s| s.to_string()).collect(), &ctx())
            .expect_err(&format!("{args:?} should fail"))
    }

    #[test]
    fn project_rename_path_and_delete_through_the_cli() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp/demo"]);

        let out = ok(&mut s, &["project", "rename", "demo", "renamed"]);
        assert!(out.contains("'demo' -> 'renamed'"), "{out}");
        assert!(ok(&mut s, &["project", "list"]).contains("renamed"));

        let out = ok(&mut s, &["project", "path", "renamed", "/tmp/moved"]);
        assert!(out.contains("/tmp/moved"), "{out}");
        assert!(ok(&mut s, &["project", "list"]).contains("/tmp/moved"));

        let out = ok(&mut s, &["project", "delete", "renamed"]);
        assert!(out.contains("deleted"), "{out}");
        assert!(!ok(&mut s, &["project", "list"]).contains("renamed"));
    }

    #[test]
    fn agent_init_then_list_through_the_cli() {
        let dir = std::env::temp_dir().join(format!(
            "voro-cli-agents-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agents_path = dir.join("voro/voro.toml");
        let ctx = DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path: agents_path.clone(),
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
        };
        let mut s = store();
        let call = |s: &mut Store, args: &[&str]| {
            run(s, args.iter().map(|x| x.to_string()).collect(), &ctx)
        };

        // no config yet: the built-in agents already list, with provenance
        let listed = call(&mut s, &["agent", "list"]).unwrap();
        assert!(listed.contains("claude"), "{listed}");
        assert!(listed.contains("codex"), "{listed}");
        assert!(listed.contains("built-in"), "{listed}");

        // init writes an optional skeleton
        let out = call(&mut s, &["agent", "init"]).unwrap();
        assert!(out.contains(&agents_path.display().to_string()), "{out}");
        assert!(agents_path.exists());

        // the skeleton adds nothing, so the built-ins still list
        let listed = call(&mut s, &["agent", "list"]).unwrap();
        assert!(listed.contains("claude"), "{listed}");
        assert!(listed.contains("built-in"), "{listed}");

        // a second init refuses rather than clobbering
        let e = call(&mut s, &["agent", "init"]).unwrap_err();
        assert!(e.contains("already exists"), "{e}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_delete_refuses_when_it_has_a_task() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T"]);
        let e = err(&mut s, &["project", "delete", "demo"]);
        assert!(e.contains("park") && e.contains("weight to 0"), "{e}");
        // the refusal must not have deleted anything
        assert!(ok(&mut s, &["project", "list"]).contains("demo"));
    }

    #[test]
    fn full_lifecycle_through_the_cli() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp/demo"]);
        ok(&mut s, &["weight", "demo", "5"]);
        assert!(ok(&mut s, &["project", "list"]).contains("w5  demo"));

        let out = ok(
            &mut s,
            &[
                "add",
                "demo",
                "Fix the parser",
                "--priority",
                "1",
                "--state",
                "ready",
            ],
        );
        assert!(out.contains("task 1"), "{out}");

        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["ask", "1", "--question", "Schema A or B?"]);
        assert!(ok(&mut s, &["inbox"]).contains("Schema A or B?"));

        ok(&mut s, &["answer", "1", "B, with a covering index"]);
        assert!(ok(&mut s, &["show", "1"]).contains("covering index"));

        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["reject", "1", "tests missing"]);
        ok(&mut s, &["done", "1"]);
        let out = ok(&mut s, &["accept", "1"]);
        assert!(out.contains("-> done"), "{out}");
    }

    #[test]
    fn default_state_is_proposed_and_triage_works() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        assert!(ok(&mut s, &["inbox"]).contains("proposed P2 demo: An idea"));
        ok(&mut s, &["triage", "1", "ready"]);
        assert!(ok(&mut s, &["next"]).contains("An idea"));
    }

    #[test]
    fn stats_reports_counts_by_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Idea one"]); // proposed by default
        ok(&mut s, &["add", "demo", "Idea two"]);
        ok(&mut s, &["add", "demo", "Ready one", "--state", "ready"]);
        // A parked project's tasks stay out of the tally.
        ok(&mut s, &["project", "add", "snoozed", "/tmp"]);
        ok(&mut s, &["weight", "snoozed", "0"]);
        ok(&mut s, &["add", "snoozed", "Hidden idea"]);

        let out = ok(&mut s, &["stats"]);
        assert!(out.contains(&format!("{:<12}{}", "triage", 2)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "ready", 1)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "done", 0)), "{out}");
    }

    #[test]
    fn blocks_flag_demotes_and_promotion_flows() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Blocker", "--state", "ready"]);
        let out = ok(
            &mut s,
            &[
                "add",
                "demo",
                "Dependent",
                "--state",
                "ready",
                "--blocks",
                "1",
            ],
        );
        assert!(out.contains("(parked)"), "{out}");

        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["accept", "1"]);
        assert!(ok(&mut s, &["list", "--state", "ready"]).contains("Dependent"));
    }

    fn propose(store: &mut Store, args: &[&str], env: Option<&str>) -> Result<String, String> {
        let (pos, flags) = split_args(args.iter().map(|s| s.to_string()).collect())?;
        propose_verb(store, &pos, &flags, env.map(str::to_string))
    }

    #[test]
    fn propose_lands_proposed() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let out = propose(&mut s, &["propose", "demo", "An idea"], None).unwrap();
        assert!(out.contains("task 1 'An idea' proposed"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("#1 proposed"));
        assert!(s.deps_of(1).unwrap().is_empty());
    }

    #[test]
    fn propose_cannot_specify_a_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let e = propose(
            &mut s,
            &["propose", "demo", "An idea", "--state", "ready"],
            None,
        )
        .unwrap_err();
        assert!(e.contains("proposed"), "{e}");
        assert!(ok(&mut s, &["list"]).is_empty());
    }

    #[test]
    fn propose_from_records_the_discovered_from_dep() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(
            &mut s,
            &["propose", "demo", "Follow-up", "--from", "1"],
            None,
        )
        .unwrap();
        assert!(ok(&mut s, &["show", "2"]).contains("dep: discovered-from 1"));
        assert!(ok(&mut s, &["show", "2"]).contains("#2 proposed"));
    }

    #[test]
    fn propose_falls_back_to_the_dispatch_env_task() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up"], Some("1")).unwrap();
        assert!(ok(&mut s, &["show", "2"]).contains("dep: discovered-from 1"));
    }

    #[test]
    fn propose_from_flag_wins_over_env() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "A", "--state", "ready"]);
        ok(&mut s, &["add", "demo", "B", "--state", "ready"]);
        propose(
            &mut s,
            &["propose", "demo", "Follow-up", "--from", "2"],
            Some("1"),
        )
        .unwrap();
        assert!(ok(&mut s, &["show", "3"]).contains("dep: discovered-from 2"));
    }

    #[test]
    fn propose_rejects_an_unknown_source_without_creating() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        propose(&mut s, &["propose", "demo", "Orphan", "--from", "99"], None).unwrap_err();
        propose(&mut s, &["propose", "demo", "Orphan"], Some("nonsense")).unwrap_err();
        assert!(ok(&mut s, &["list"]).is_empty());
    }

    #[test]
    fn set_updates_fields_without_touching_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Old title", "--state", "ready"]);
        ok(
            &mut s,
            &[
                "set",
                "1",
                "--title",
                "New title",
                "--priority",
                "0",
                "--agent",
                "codex",
            ],
        );
        let shown = ok(&mut s, &["show", "1"]);
        assert!(shown.contains("New title"));
        assert!(shown.contains("P0"));
        assert!(shown.contains("codex"));
        assert!(shown.contains("ready"));
        ok(&mut s, &["set", "1", "--no-agent"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("codex"));
    }

    // --- human-only tasks (DESIGN.md §3/§6) ---

    #[test]
    fn a_human_task_lives_start_to_done_and_refuses_dispatch() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(
            &mut s,
            &[
                "add",
                "demo",
                "Capture a bag",
                "--state",
                "ready",
                "--human",
            ],
        );
        assert!(ok(&mut s, &["show", "1"]).contains("human-only"));

        // dispatch refuses with a clear error, before touching any config
        let e = err(&mut s, &["dispatch", "1"]);
        assert!(e.contains("human-only"), "{e}");

        // by hand it starts like any task, cannot ask, and completes straight
        // to done with no review and no PR nag
        ok(&mut s, &["start", "1"]);
        let e = err(&mut s, &["ask", "1", "--question", "which bag?"]);
        assert!(e.contains("human-only"), "{e}");
        let out = ok(&mut s, &["done", "1"]);
        assert!(out.contains("-> done"), "{out}");
        assert!(!out.contains("note:"), "{out}");
    }

    #[test]
    fn set_toggles_human_and_guards_the_agent_override() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);

        ok(&mut s, &["set", "1", "--human"]);
        assert!(ok(&mut s, &["show", "1"]).contains("human-only"));
        ok(&mut s, &["set", "1", "--no-human"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("human-only"));

        let e = err(&mut s, &["set", "1", "--human", "--no-human"]);
        assert!(e.contains("mutually exclusive"), "{e}");

        // an existing override blocks --human until cleared in the same edit
        ok(&mut s, &["set", "1", "--agent", "codex"]);
        let e = err(&mut s, &["set", "1", "--human"]);
        assert!(e.contains("agent override"), "{e}");
        ok(&mut s, &["set", "1", "--human", "--no-agent"]);
        assert!(ok(&mut s, &["show", "1"]).contains("human-only"));

        // ...and the human flag blocks a new override symmetrically
        let e = err(&mut s, &["set", "1", "--agent", "codex"]);
        assert!(e.contains("agent override"), "{e}");
    }

    #[test]
    fn add_refuses_human_with_an_agent_override() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let e = err(&mut s, &["add", "demo", "T", "--human", "--agent", "codex"]);
        assert!(e.contains("agent override"), "{e}");
        assert!(ok(&mut s, &["list"]).is_empty());
    }

    #[test]
    fn open_refuses_a_non_review_task_and_help_documents_it() {
        // The state guard fires before any config is loaded, so a `ready` task
        // is refused without touching the real user voro.toml `ctx()` names.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["open", "1"]);
        assert!(e.contains("review or running"), "{e}");
        assert!(ok(&mut s, &["help"]).contains("open <task-id>"), "help");
    }

    // --- PR tracking (task, DESIGN.md §11c) ---

    #[test]
    fn set_tracks_canonicalises_and_clears_a_pr() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);

        // the owner/repo#n shorthand is accepted and stored as a canonical URL
        ok(&mut s, &["set", "1", "--pr", "acme/widget#42"]);
        let shown = ok(&mut s, &["show", "1"]);
        assert!(
            shown.contains("pr: https://github.com/acme/widget/pull/42"),
            "{shown}"
        );

        // --no-pr clears it
        ok(&mut s, &["set", "1", "--no-pr"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("pr:"));
    }

    #[test]
    fn set_rejects_a_non_pr_reference_without_tracking() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(
            &mut s,
            &[
                "set",
                "1",
                "--pr",
                "https://github.com/acme/widget/issues/9",
            ],
        );
        assert!(e.contains("not a GitHub PR"), "{e}");
        assert!(!ok(&mut s, &["show", "1"]).contains("pr:"));
    }

    #[test]
    fn set_pr_and_no_pr_are_mutually_exclusive() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["set", "1", "--pr", "acme/w#1", "--no-pr"]);
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    /// `reject --from-pr` on a task with no tracked PR reports the missing
    /// reference before shelling out to `gh` — a network-free failure — and
    /// leaves the task in `review`.
    #[test]
    fn reject_from_pr_without_a_tracked_pr_reports_and_does_not_transition() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        let e = err(&mut s, &["reject", "1", "--from-pr"]);
        assert!(e.contains("no tracked PR"), "{e}");
        assert!(ok(&mut s, &["show", "1"]).contains("#1 review"));
    }

    /// Plain `reject <id> TEXT` is unchanged by the `--from-pr` addition.
    #[test]
    fn reject_with_text_still_feeds_back_and_requeues() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        let out = ok(&mut s, &["reject", "1", "tests missing"]);
        assert!(out.contains("-> running"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("tests missing"));
    }

    // --- branch tracking (task #81, DESIGN.md §5/§8) ---

    #[test]
    fn set_tracks_and_clears_a_branch() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);

        ok(&mut s, &["set", "1", "--branch", "feat/parser"]);
        assert!(ok(&mut s, &["show", "1"]).contains("branch: feat/parser"));

        ok(&mut s, &["set", "1", "--no-branch"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("branch:"));
    }

    #[test]
    fn set_branch_and_no_branch_are_mutually_exclusive() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["set", "1", "--branch", "x", "--no-branch"]);
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn done_records_the_reported_branch_and_reaches_review() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);

        let out = ok(&mut s, &["done", "1", "--branch", "feat/parser"]);
        assert!(out.contains("-> review"), "{out}");
        assert!(out.contains("branch feat/parser"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("branch: feat/parser"));
    }

    #[test]
    fn done_without_a_branch_still_reviews() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let out = ok(&mut s, &["done", "1"]);
        assert!(out.contains("-> review"), "{out}");
        assert!(!ok(&mut s, &["show", "1"]).contains("branch:"));
    }

    /// `done --branch` on a non-running task is refused by the transition
    /// before any branch is recorded.
    #[test]
    fn done_branch_on_a_non_running_task_records_nothing() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["done", "1", "--branch", "feat/parser"]);
        assert!(e.contains("cannot"), "{e}");
        assert!(!ok(&mut s, &["show", "1"]).contains("branch:"));
    }

    #[test]
    fn help_documents_branch_tracking() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(out.contains("--branch NAME"), "{out}");
        assert!(out.contains("--no-branch"), "{out}");
    }

    #[test]
    fn done_summary_surfaces_in_show() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let out = ok(
            &mut s,
            &["done", "1", "--summary", "Implemented X, tests pass"],
        );
        assert!(out.contains("-> review"), "{out}");
        let shown = ok(&mut s, &["show", "1"]);
        assert!(shown.contains("summary"), "{shown}");
        assert!(shown.contains("Implemented X, tests pass"), "{shown}");
    }

    #[test]
    fn help_documents_pr_tracking() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(out.contains("pr <task-id>"), "{out}");
        assert!(out.contains("--pr URL"), "{out}");
        assert!(out.contains("--from-pr"), "{out}");
    }

    // --- pr create from a review task's summary (DESIGN.md §8) ---

    /// Walk a fresh ready task into `review` with the given branch/summary set,
    /// through the transition machine and the branch/summary write paths.
    fn review_task(s: &mut Store, branch: Option<&str>, summary: Option<&str>) -> i64 {
        ok(s, &["add", "demo", "Do the thing", "--state", "ready"]);
        let id = s.tasks().unwrap().last().unwrap().id;
        if let Some(b) = branch {
            ok(s, &["set", &id.to_string(), "--branch", b]);
        }
        ok(s, &["start", &id.to_string()]);
        let mut done = vec!["done".to_string(), id.to_string()];
        if let Some(text) = summary {
            done.push("--summary".into());
            done.push(text.into());
        }
        ok(s, &done.iter().map(String::as_str).collect::<Vec<_>>());
        id
    }

    /// `pr` on a task that is not in `review` fails naming the state gap, before
    /// touching git or `gh` — the validation runs first.
    #[test]
    fn pr_create_requires_the_review_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["pr", "1", "--yes"]);
        assert!(e.contains("review"), "{e}");
    }

    /// `pr` on a review task with no branch fails naming the branch gap.
    #[test]
    fn pr_create_requires_a_branch() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let id = review_task(&mut s, None, Some("did it"));
        let e = err(&mut s, &["pr", &id.to_string(), "--yes"]);
        assert!(e.contains("branch"), "{e}");
    }

    /// `pr` on a review task with a branch but no summary fails naming the
    /// summary gap.
    #[test]
    fn pr_create_requires_a_summary() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let id = review_task(&mut s, Some("feat/thing"), None);
        let e = err(&mut s, &["pr", &id.to_string(), "--yes"]);
        assert!(e.contains("summary"), "{e}");
    }

    #[test]
    fn help_documents_pr_create_and_summary_file() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(out.contains("pr <task-id> [--yes]"), "{out}");
        assert!(out.contains("--summary-file"), "{out}");
    }

    // --- done summary-file and PR-readiness warnings (DESIGN.md §8) ---

    #[test]
    fn done_summary_file_records_the_summary() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let path = std::env::temp_dir().join(format!("voro-summary-{}.md", std::process::id()));
        std::fs::write(&path, "## What\nDid the thing\n").unwrap();

        let out = ok(
            &mut s,
            &["done", "1", "--summary-file", path.to_str().unwrap()],
        );
        assert!(out.contains("-> review"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("Did the thing"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn done_summary_and_summary_file_are_mutually_exclusive() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let e = err(
            &mut s,
            &["done", "1", "--summary", "x", "--summary-file", "/tmp/nope"],
        );
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn done_warns_when_branch_and_summary_are_missing() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        // neither recorded: warns but succeeds
        let out = ok(&mut s, &["done", "1"]);
        assert!(out.contains("-> review"), "{out}");
        assert!(out.contains("note:"), "{out}");
        assert!(out.contains("branch or summary"), "{out}");
    }

    #[test]
    fn done_warns_only_about_the_missing_half() {
        // summary given, no branch: the note names branch only
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let out = ok(&mut s, &["done", "1", "--summary", "did it"]);
        assert!(out.contains("note: no branch recorded"), "{out}");

        // branch given, no summary: the note names summary only
        ok(&mut s, &["add", "demo", "T2", "--state", "ready"]);
        ok(&mut s, &["set", "2", "--branch", "feat/x"]);
        ok(&mut s, &["start", "2"]);
        let out = ok(&mut s, &["done", "2"]);
        assert!(out.contains("note: no summary recorded"), "{out}");
    }

    #[test]
    fn done_with_branch_and_summary_does_not_warn() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["set", "1", "--branch", "feat/x"]);
        ok(&mut s, &["start", "1"]);
        let out = ok(&mut s, &["done", "1", "--summary", "did it"]);
        assert!(out.contains("-> review"), "{out}");
        assert!(!out.contains("note:"), "{out}");
    }

    #[test]
    fn a_partial_report_is_flagged_incomplete_in_list_and_show() {
        // A review task with a branch but no summary — the shape a forgotten
        // --summary (or the SessionEnd fallback) leaves — is surfaced as an
        // anomaly the operator can act on, not left looking complete.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--branch", "feat/x"]);

        assert!(ok(&mut s, &["list"]).contains("[incomplete report]"));
        assert!(
            ok(&mut s, &["show", "1"]).contains("incomplete report: needs a branch and summary")
        );
    }

    #[test]
    fn a_complete_report_is_not_flagged_incomplete() {
        // Both halves present: a complete report, no anomaly. And a planning
        // task with neither is likewise not flagged.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Coding", "--state", "ready"]);
        ok(&mut s, &["set", "1", "--branch", "feat/x"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--summary", "did it"]);
        ok(&mut s, &["add", "demo", "Planning", "--state", "ready"]);
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["done", "2"]);

        assert!(!ok(&mut s, &["list"]).contains("[incomplete report]"));
        assert!(!ok(&mut s, &["show", "1"]).contains("incomplete report"));
        assert!(!ok(&mut s, &["show", "2"]).contains("incomplete report"));
    }

    // --- set --summary (task #99, DESIGN.md §8) ---

    #[test]
    fn set_summary_replaces_a_review_tasks_summary() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--summary", "thin first draft"]);

        ok(&mut s, &["set", "1", "--summary", "the PR-ready account"]);
        assert_eq!(
            s.latest_summary(1).unwrap().as_deref(),
            Some("the PR-ready account")
        );
        // still in review — set never transitions
        assert!(ok(&mut s, &["show", "1"]).contains("#1 review"));
    }

    #[test]
    fn set_summary_clears_the_incomplete_report_marker() {
        // The half-report shape the SessionEnd fallback leaves: branch, no
        // summary. Supplying the summary in place clears the marker without a
        // reject/done round trip.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--branch", "feat/x"]);
        assert!(ok(&mut s, &["list"]).contains("[incomplete report]"));

        ok(&mut s, &["set", "1", "--summary", "the missing half"]);
        assert!(!ok(&mut s, &["list"]).contains("[incomplete report]"));
        assert!(!ok(&mut s, &["show", "1"]).contains("incomplete report"));
    }

    #[test]
    fn set_summary_file_records_the_summary_on_a_running_task() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let path = std::env::temp_dir().join(format!("voro-set-summary-{}.md", std::process::id()));
        std::fs::write(&path, "## What\nAmended account\n").unwrap();

        ok(
            &mut s,
            &["set", "1", "--summary-file", path.to_str().unwrap()],
        );
        assert!(
            s.latest_summary(1)
                .unwrap()
                .unwrap()
                .contains("Amended account")
        );
        assert!(ok(&mut s, &["show", "1"]).contains("#1 running"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_summary_and_summary_file_are_mutually_exclusive() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let e = err(
            &mut s,
            &["set", "1", "--summary", "x", "--summary-file", "/tmp/nope"],
        );
        assert!(e.contains("mutually exclusive"), "{e}");
    }

    #[test]
    fn set_summary_is_refused_outside_running_and_review() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["set", "1", "--summary", "too early"]);
        assert!(e.contains("running or review"), "{e}");
        assert_eq!(s.latest_summary(1).unwrap(), None);
    }

    #[test]
    fn help_documents_set_summary() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(
            out.contains("[--summary TEXT | --summary-file PATH]"),
            "{out}"
        );
    }

    #[test]
    fn help_documents_import() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(out.contains("import <project>"), "{out}");
        assert!(out.contains("gh issue list"), "{out}");
    }

    #[test]
    fn import_rejects_an_unknown_project_before_touching_gh() {
        // Never gets as far as shelling out to `gh` — resolve_project fails
        // first — so this stays a safe, network-free test.
        let mut s = store();
        assert!(err(&mut s, &["import", "nope"]).contains("no project"));
    }

    #[test]
    fn explain_prints_the_decomposition() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["weight", "demo", "2"]);
        ok(
            &mut s,
            &["add", "demo", "T", "--priority", "0", "--state", "ready"],
        );
        let out = ok(&mut s, &["explain", "1"]);
        assert!(out.contains("base w×(p+s)      16.0"), "{out}");
        assert!(out.contains("state            ready  (bonus +0)"), "{out}");
    }

    #[test]
    fn errors_are_actionable() {
        let mut s = store();
        assert!(err(&mut s, &["frobnicate"]).contains("unknown verb"));
        assert!(err(&mut s, &["weight", "nope", "3"]).contains("no project"));
        assert!(err(&mut s, &["start"]).contains("task id"));
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        assert!(err(&mut s, &["accept", "1"]).contains("cannot accept"));
        assert!(err(&mut s, &["add", "demo", "T2", "--state", "running"]).contains("running"));
        assert!(err(&mut s, &["ask", "1"]).contains("question"));
    }

    #[test]
    fn illegal_transitions_do_not_mutate() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        err(&mut s, &["done", "1"]);
        assert!(ok(&mut s, &["show", "1"]).contains("#1 ready"));
    }

    /// End to end (DESIGN.md §8): a task really dispatched, whose agent
    /// process really exits without calling `voro done`/`ask`, is finalised
    /// purely by a later CLI verb reading state — no code here ever calls the
    /// reconciliation function directly. The task lands in `stalled`,
    /// recording a `reconcile` event that the history surfaces.
    #[test]
    fn a_dead_dispatched_session_is_finalised_and_stalled_on_read() {
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!(
            "voro-cli-reconcile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);

        let db_path = root.join("voro.db");
        let agents_path = root.join("voro.toml");
        // an agent command that exits immediately with failure, as if it crashed
        std::fs::write(
            &agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"false {prompt_file}\"\n",
        )
        .unwrap();

        let mut store = Store::open(&db_path).unwrap();
        let dispatch_ctx = crate::dispatch::DispatchCtx {
            db_path: db_path.clone(),
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
        };
        ok(
            &mut store,
            &["project", "add", "demo", project.to_str().unwrap()],
        );
        ok(
            &mut store,
            &["add", "demo", "Do the thing", "--state", "ready"],
        );

        let summary = crate::dispatch::dispatch(&mut store, &dispatch_ctx, 1, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(1).unwrap().state, TaskState::Running);

        // give the spawned shell a moment to actually exit
        std::thread::sleep(std::time::Duration::from_millis(200));

        // a plain read-only verb — not a direct call to the reconciler —
        // must notice the dead process, finalise the session, and land the
        // task in stalled (DESIGN.md §6/§8)
        let out = run(
            &mut store,
            vec!["show".to_string(), "1".to_string()],
            &dispatch_ctx,
        )
        .unwrap();
        assert_eq!(store.task(1).unwrap().state, TaskState::Stalled);
        assert!(out.contains("#1 stalled"), "{out}");
        // and the reconcile is recorded in the task's history
        assert!(out.contains("reconcile"), "{out}");
        assert!(out.contains("without reporting"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- answer → continuation (task #31, DESIGN.md §6/§8) ---

    /// A scratch database, a freshly-`git init`ed clean project, and an
    /// `voro.toml` whose one agent is a stub command — the same shape as
    /// `dispatch.rs`'s own fixture, duplicated here since that one is private
    /// to its module's tests.
    fn scratch_env(cmd: &str) -> (Store, DispatchCtx, std::path::PathBuf) {
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!(
            "voro-cli-answer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let db_path = root.join("voro.db");
        let agents_path = root.join("voro.toml");
        std::fs::write(
            &agents_path,
            format!("default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"{cmd}\"\n"),
        )
        .unwrap();

        let store = Store::open(&db_path).unwrap();
        let ctx = DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
        };
        (store, ctx, project)
    }

    fn prompt_files(ctx: &DispatchCtx) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(&ctx.runtime_dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.to_string_lossy().ends_with(".prompt.md"))
            .collect()
    }

    /// A ready task in `store`, dispatched and then asked a question, all
    /// through direct `voro-core` calls rather than `run()` — reaching
    /// `needs-input` this way sidesteps `run()`'s reconcile-on-read, which
    /// would otherwise race the stub agent's near-instant exit and finalise the
    /// dispatch session before `ask` lands, leaving the test unable to control
    /// its session history.
    fn dispatched_and_asked(
        store: &mut Store,
        ctx: &DispatchCtx,
        project_path: &std::path::Path,
    ) -> i64 {
        let p = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        crate::dispatch::dispatch(store, ctx, task.id, None).unwrap();
        store
            .apply(task.id, Action::Ask("Schema A or B?".into()))
            .unwrap();
        task.id
    }

    /// Acceptance case one: a task with prior session history gets a
    /// continuation dispatched automatically, and that continuation's prompt
    /// carries the `## Answers` section the answer just appended.
    #[test]
    fn answering_a_previously_dispatched_task_starts_a_continuation() {
        let (mut store, ctx, project) = scratch_env("cat {prompt_file}");
        let id = dispatched_and_asked(&mut store, &ctx, &project);

        let out = run(
            &mut store,
            vec![
                "answer".into(),
                id.to_string(),
                "Schema B, with a covering index".into(),
            ],
            &ctx,
        )
        .unwrap();
        assert!(out.contains("-> running"), "{out}");
        assert!(out.contains(&format!("continued task {id}")), "{out}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);

        // the original dispatch's session plus the continuation's
        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2, "{sessions:?}");

        // the continuation's prompt is the task body, now carrying the answer
        let prompts = prompt_files(&ctx);
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts.iter().any(|p| std::fs::read_to_string(p)
                .unwrap()
                .contains("Schema B, with a covering index")),
            "the continuation prompt must carry the answer"
        );

        let _ = std::fs::remove_dir_all(project.parent().unwrap());
    }

    /// Acceptance case two: a task that was only ever started by hand — no
    /// dispatch, no session — still answers as a plain transition; nothing
    /// tries to spawn a continuation for it.
    #[test]
    fn answering_a_never_dispatched_task_is_a_plain_transition() {
        let mut store = Store::open_in_memory().unwrap();
        let ctx = ctx();
        ok(&mut store, &["project", "add", "demo", "/tmp/demo"]);
        ok(
            &mut store,
            &["add", "demo", "Fix the parser", "--state", "ready"],
        );
        ok(&mut store, &["start", "1"]);
        ok(&mut store, &["ask", "1", "--question", "A or B?"]);
        assert!(store.sessions_for(1).unwrap().is_empty());

        let out = run(
            &mut store,
            vec!["answer".into(), "1".into(), "B".into()],
            &ctx,
        )
        .unwrap();
        assert_eq!(out, "task 1 -> running");
        assert_eq!(store.task(1).unwrap().state, TaskState::Running);
        assert!(store.sessions_for(1).unwrap().is_empty());
    }

    /// `--no-dispatch` opts out of continuation even when history exists.
    #[test]
    fn no_dispatch_flag_skips_continuation_despite_history() {
        let (mut store, ctx, project) = scratch_env("cat {prompt_file}");
        let id = dispatched_and_asked(&mut store, &ctx, &project);

        let out = run(
            &mut store,
            vec![
                "answer".into(),
                id.to_string(),
                "--no-dispatch".into(),
                "B".into(),
            ],
            &ctx,
        )
        .unwrap();
        assert_eq!(out, format!("task {id} -> running"));
        assert_eq!(store.sessions_for(id).unwrap().len(), 1, "no continuation");

        let _ = std::fs::remove_dir_all(project.parent().unwrap());
    }
}
