//! The command-line verb surface: every TUI action, scriptable and
//! agent-legible (DESIGN.md §9). Verbs that §8 names for the agent return
//! path (`ask`, `done`, `propose`) keep those names and flags so Milestone B
//! extends rather than renames. Parsing is hand-rolled: positionals plus
//! `--flag value` pairs is all the grammar this needs.

use std::collections::HashMap;
use std::fmt::Write as _;

use voro_core::{
    Action, DepKind, NewTask, Priority, Project, Store, Task, TaskEdit, TaskState, Triage,
    scheduler,
};

use crate::dispatch::{self, DispatchCtx};

const HELP: &str = "\
voro — prioritised attention across projects

usage: voro [--db PATH]                 launch the TUI
       voro [--db PATH] <verb> [args]

projects
  project add <name> <path>       create a project (weight 3)
  project list                    list projects with weights
  weight <project> <0-5>          set a project's weight (0 parks it)

tasks
  add <project> <title> [--body TEXT | --body-file PATH] [--priority 0-3]
      [--state proposed|parked|ready] [--agent NAME] [--blocks IDS]
  propose <project> <title> [--body TEXT | --body-file PATH] [--from TASK-ID]
                                  create a proposed task; --from (default
                                  $VORO_TASK_ID) links it discovered-from
  set <task-id> [--title T] [--priority 0-3] [--agent NAME | --no-agent]
      [--body TEXT | --body-file PATH] [--blocks IDS]
  show <task-id>                  full task: body, deps, events
  list [--state STATE] [--project P]
  inbox                           the next-action queue: questions, reviews,
                                  proposals, top ready tasks — by score
  next                            the single highest-scoring ready task
  explain <task-id>               score decomposition

dispatch
  dispatch <task-id> [--agent NAME]
                                  spawn a headless agent session on a ready
                                  task; --agent overrides the resolved agent
  continue <task-id> [--agent NAME]
                                  spawn a fresh session on an already-running
                                  task without changing its state; what
                                  `answer` does automatically for a
                                  previously-dispatched task, exposed to retry
                                  a continuation that failed

transitions
  triage <task-id> <parked|ready|reject>
  start <task-id>                 ready → running
  ask <task-id> --question TEXT   running → needs-input
  answer <task-id> TEXT [--no-dispatch]
                                  needs-input → running; if the task has ever
                                  been dispatched, also starts a continuation
                                  session with the answer in the prompt body
                                  (--no-dispatch skips that)
  done <task-id>                  running → review
  accept <task-id>                review → done
  reject <task-id> TEXT           review → running (TEXT is the feedback)
  abort <task-id>                 running → ready
  park <task-id>                  ready → parked
  unpark <task-id>                parked → ready
  abandon <task-id>               parked|ready|needs-input|review → rejected
";

pub fn run(store: &mut Store, args: Vec<String>, ctx: &DispatchCtx) -> Result<String, String> {
    // Reconcile-on-read (DESIGN.md §8): before any verb consults session or
    // task state, close out sessions whose process has already exited.
    crate::reconcile::reconcile_live_sessions(store).map_err(|e| e.to_string())?;

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
        "explain" => explain_verb(store, &pos),
        "dispatch" => dispatch_verb(store, &pos, &flags, ctx),
        "continue" => continue_verb(store, &pos, &flags, ctx),
        "answer" => answer_verb(store, &pos, &flags, ctx),
        "triage" | "start" | "ask" | "done" | "accept" | "reject" | "abort" | "park" | "unpark"
        | "abandon" => transition_verb(store, verb, &pos, &flags),
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
    match need(pos, 1, "project subcommand (add|list)")? {
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
        other => Err(format!("unknown project subcommand '{other}'")),
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
    let edit = TaskEdit {
        title: flags.get("title").cloned().unwrap_or(current.title),
        body: body_from(flags)?.unwrap_or(current.body),
        priority: match flags.get("priority") {
            Some(raw) => parse_priority(raw)?,
            None => current.priority,
        },
        agent,
    };
    let task = store.update_task(id, edit).map_err(|e| e.to_string())?;
    let task = match flags.get("blocks") {
        Some(raw) => store
            .set_blocks_deps(id, &parse_blocks(raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    Ok(format!("task {} updated ({})", task.id, task.state))
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
    if let Some(agent) = &task.agent {
        writeln!(out, "agent override: {agent}").unwrap();
    }
    if let Some(q) = &task.question {
        writeln!(out, "question: {q}").unwrap();
    }
    if store.redispatch_flag(id).map_err(|e| e.to_string())? {
        writeln!(out, "flagged for redispatch").unwrap();
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
            redispatch_suffix(store, task.id)
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
        write!(out, "{}", redispatch_suffix(store, c.task.id)).unwrap();
        writeln!(out).unwrap();
    }
    if out.is_empty() {
        out = "nothing needs you\n".to_string();
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
                redispatch_suffix(store, c.task.id)
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

/// `  [redispatch]` when the task's most recent session ended `failed` or
/// `capped` (DESIGN.md §8), else empty — the flag lives in session history,
/// not on the task, so every place that prints a task line re-derives it.
fn redispatch_suffix(store: &Store, task_id: i64) -> &'static str {
    if store.redispatch_flag(task_id).unwrap_or(false) {
        "  [redispatch]"
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
        TaskState::Ready | TaskState::NeedsInput | TaskState::Review
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
    dispatch::continue_dispatch(store, ctx, id, flags.get("agent").map(String::as_str))
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
        .apply(id, Action::Answer(text))
        .map_err(|e| e.to_string())?;
    let out = format!("task {} -> {}", task.id, task.state);

    if !has_history || flags.contains_key("no-dispatch") {
        return Ok(out);
    }
    match dispatch::continue_dispatch(store, ctx, id, None) {
        Ok(summary) => Ok(format!("{out}; {summary}")),
        Err(e) => Err(format!(
            "{out}, but continuation dispatch failed: {e} — retry with 'voro continue {id}'"
        )),
    }
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
        "done" => Action::Complete,
        "accept" => Action::Accept,
        "reject" => Action::RejectWork(rest_text(pos, 2, "rejection feedback")?),
        "abort" => Action::Abort,
        "park" => Action::Park,
        "unpark" => Action::Unpark,
        "abandon" => Action::Abandon,
        _ => unreachable!("guarded by run()"),
    };
    let task = store.apply(id, action).map_err(|e| e.to_string())?;
    Ok(format!("task {} -> {}", task.id, task.state))
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
    /// and flagged for redispatch purely by a later CLI verb reading state —
    /// no code here ever calls the reconciliation function directly.
    #[test]
    fn a_dead_dispatched_session_surfaces_the_redispatch_flag_on_read() {
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
        let agents_path = root.join("agents.toml");
        // an agent command that exits immediately with failure, as if it crashed
        std::fs::write(
            &agents_path,
            "default = \"stub\"\n\n[agents.stub]\ncmd = \"false {prompt_file}\"\n",
        )
        .unwrap();

        let mut store = Store::open(&db_path).unwrap();
        let dispatch_ctx = crate::dispatch::DispatchCtx {
            db_path: db_path.clone(),
            agents_path,
            runtime_dir: root.join("sessions"),
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
        // must notice the dead process and finalise it
        let out = run(&mut store, vec!["inbox".to_string()], &dispatch_ctx).unwrap();
        assert!(out.contains("[redispatch]"), "{out}");
        assert_eq!(store.task(1).unwrap().state, TaskState::Ready);
        assert!(store.redispatch_flag(1).unwrap());

        let shown = ok(&mut store, &["show", "1"]);
        assert!(shown.contains("flagged for redispatch"), "{shown}");

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- answer → continuation (task #31, DESIGN.md §6/§8) ---

    /// A scratch database, a freshly-`git init`ed clean project, and an
    /// `agents.toml` whose one agent is a stub command — the same shape as
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
        let agents_path = root.join("agents.toml");
        std::fs::write(
            &agents_path,
            format!("default = \"stub\"\n\n[agents.stub]\ncmd = \"{cmd}\"\n"),
        )
        .unwrap();

        let store = Store::open(&db_path).unwrap();
        let ctx = DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
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
    /// would otherwise race the stub agent's near-instant exit and flag the
    /// still-`running` task for redispatch before `ask` ever lands.
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
