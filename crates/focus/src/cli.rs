//! The command-line verb surface: every TUI action, scriptable and
//! agent-legible (DESIGN.md §9). Verbs that §8 names for the agent return
//! path (`ask`, `done`) keep those names and flags so Milestone B extends
//! rather than renames. Parsing is hand-rolled: positionals plus `--flag
//! value` pairs is all the grammar this needs.

use std::collections::HashMap;
use std::fmt::Write as _;

use focus_core::{
    Action, NewTask, Priority, Project, Store, Task, TaskEdit, TaskState, Triage, scheduler,
};

const HELP: &str = "\
focus — prioritised attention across projects

usage: focus [--db PATH]                 launch the TUI
       focus [--db PATH] <verb> [args]

projects
  project add <name> <path>       create a project (weight 3)
  project list                    list projects with weights
  weight <project> <0-5>          set a project's weight (0 parks it)

tasks
  add <project> <title> [--body TEXT | --body-file PATH] [--priority 0-3]
      [--state proposed|backlog|ready] [--agent NAME] [--blocks IDS]
  set <task-id> [--title T] [--priority 0-3] [--agent NAME | --no-agent]
      [--body TEXT | --body-file PATH] [--blocks IDS]
  show <task-id>                  full task: body, deps, events
  list [--state STATE] [--project P]
  inbox                           needs-input + review, sorted by score
  next                            the single highest-scoring ready task
  explain <task-id>               score decomposition

transitions
  triage <task-id> <backlog|ready|reject>
  start <task-id>                 ready → running
  ask <task-id> --question TEXT   running → needs-input
  answer <task-id> TEXT           needs-input → running
  done <task-id>                  running → review
  accept <task-id>                review → done
  reject <task-id> TEXT           review → running (TEXT is the feedback)
  abort <task-id>                 running → ready
  park <task-id>                  ready → backlog
  unpark <task-id>                backlog → ready
  abandon <task-id>               backlog|ready|needs-input|review → rejected
";

pub fn run(store: &mut Store, args: Vec<String>) -> Result<String, String> {
    let (pos, flags) = split_args(args)?;
    let verb = pos.first().map(String::as_str).unwrap_or("help");
    match verb {
        "help" | "--help" | "-h" => Ok(HELP.to_string()),
        "project" => project_verb(store, &pos, &flags),
        "weight" => weight_verb(store, &pos),
        "add" => add_verb(store, &pos, &flags),
        "set" => set_verb(store, &pos, &flags),
        "show" => show_verb(store, &pos),
        "list" => list_verb(store, &flags),
        "inbox" => inbox_verb(store),
        "next" => next_verb(store),
        "explain" => explain_verb(store, &pos),
        "triage" | "start" | "ask" | "answer" | "done" | "accept" | "reject" | "abort" | "park"
        | "unpark" | "abandon" => transition_verb(store, verb, &pos, &flags),
        other => Err(format!("unknown verb '{other}' — try 'focus help'")),
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
        .ok_or_else(|| format!("missing {what} — try 'focus help'"))
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
        return Err(format!("missing {what} — try 'focus help'"));
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
                TaskState::Proposed | TaskState::Backlog | TaskState::Ready
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
        writeln!(out, "{}", task_line(&task, name)).unwrap();
    }
    Ok(out)
}

fn inbox_verb(store: &mut Store) -> Result<String, String> {
    let candidates = store.candidates().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for c in scheduler::inbox(&candidates) {
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
        writeln!(out).unwrap();
    }
    let proposed = store.proposed_count().map_err(|e| e.to_string())?;
    if proposed > 0 {
        writeln!(out, "triage {proposed} proposed task(s)").unwrap();
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
                "{:5.1}  {}",
                c.score.total,
                task_line(&c.task, &c.project_name)
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
    writeln!(out, "base w × p      {:>6.1}", b.base).unwrap();
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

fn transition_verb(
    store: &mut Store,
    verb: &str,
    pos: &[String],
    flags: &HashMap<String, String>,
) -> Result<String, String> {
    let id = task_id(pos, 1)?;
    let action = match verb {
        "triage" => match need(pos, 2, "triage target (backlog|ready|reject)")? {
            "backlog" => Action::Triage(Triage::Backlog),
            "ready" => Action::Triage(Triage::Ready),
            "reject" => Action::Triage(Triage::Reject),
            other => {
                return Err(format!(
                    "triage target must be backlog|ready|reject, got '{other}'"
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
        "answer" => Action::Answer(rest_text(pos, 2, "answer text")?),
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

    fn ok(store: &mut Store, args: &[&str]) -> String {
        run(store, args.iter().map(|s| s.to_string()).collect())
            .unwrap_or_else(|e| panic!("{args:?} failed: {e}"))
    }

    fn err(store: &mut Store, args: &[&str]) -> String {
        run(store, args.iter().map(|s| s.to_string()).collect())
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
        assert!(ok(&mut s, &["inbox"]).contains("triage 1 proposed"));
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
        assert!(out.contains("(backlog)"), "{out}");

        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["accept", "1"]);
        assert!(ok(&mut s, &["list", "--state", "ready"]).contains("Dependent"));
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
        assert!(out.contains("base w × p        16.0"), "{out}");
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
}
