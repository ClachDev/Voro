//! The command-line verb surface: every TUI action, scriptable and
//! agent-legible (DESIGN.md §9). Parsing is clap derive, so a flag a verb does
//! not declare is rejected by name rather than silently swallowed. Help is the
//! one clap default overridden: every help request short-circuits to the
//! hand-written `HELP` overview.

use std::fmt::Write as _;

use clap::{Args, Parser, Subcommand, ValueEnum};

use voro_core::{
    Action, AgentsConfig, DepKind, NewTask, PrRef, Priority, Project, ReviewAction, ReviewMedium,
    Store, Task, TaskEdit, TaskState, Triage, scheduler,
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
  project archive <project>       retire a project: hide it and all its tasks
                                  from the cockpit (queue, stats, running
                                  strip); tasks freeze in their states, and
                                  `project list` keeps it, tagged [archived]
  project unarchive <project>     restore an archived project and its tasks
                                  exactly as they were
  project delete <project>        delete a project with no tasks — park it
                                  (weight 0) or archive it instead to retire
                                  one that has any
  project action <project> <auto|pr|viewer[:NAME]>
                                  set how `pr` shows the project's review
                                  diffs: auto (GitHub when the checkout is a
                                  GitHub repo, else the viewer), pr always,
                                  or a viewer from voro.toml (viewer:NAME
                                  picks a [viewers.NAME] entry)
  weight <project> <0-5>          set a project's weight (0 parks it)

tasks
  add <project> <title> [--body TEXT | --body-file PATH] [--priority 0-3]
      [--state proposed|parked|ready] [--agent NAME] [--blocked-by IDS]
      [--blocks IDS] [--human]
                                  --blocked-by lists the tasks this one waits
                                  on; --blocks makes the listed tasks wait on
                                  this one
                                  --human marks a task no agent can execute:
                                  never dispatched, worked by hand, and its
                                  completion goes straight to done
  propose <project> <title> [--body TEXT | --body-file PATH] [--from TASK-ID]
                                  create a proposed task; --from (default
                                  $VORO_TASK_ID) links it discovered-from
  set <task-id> [--title T] [--priority 0-3] [--agent NAME | --no-agent]
      [--body TEXT | --body-file PATH] [--blocked-by IDS] [--blocks IDS]
      [--pr URL | --no-pr] [--branch NAME | --no-branch] [--human | --no-human]
      [--summary TEXT | --summary-file PATH]
                                  --blocked-by replaces this task's own
                                  blocker list; --blocks adds this task as a
                                  blocker of each listed task
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
                                  review, waiting, stalled, done; excludes
                                  parked projects
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
  viewer list                     list the viewers voro.toml defines; * marks
                                  the default used when nothing names one
  viewer add <name> <cmd>         define a [viewers.NAME] entry in voro.toml
                                  (comment-preserving); cmd may carry {path},
                                  {branch}, {base} (e.g. 'zed {path}')
  viewer remove <name>            delete a viewer; refused while a project's
                                  review action still names it
  open <task-id>                  open a review/running task's checkout in a
                                  voro.toml viewer to see its diff — the
                                  explicit spelling of pr's viewer medium;
                                  reports what to configure if none is set
  pr <task-id> [--yes]            show the task's diff via the project's
                                  review action (`project action`). GitHub:
                                  jump to the tracked PR in a browser, or push
                                  the review task's branch and open a ready PR
                                  from its summary, recording the URL (--yes
                                  skips the confirm; track an existing PR with
                                  `set --pr`). Viewer: open the checkout in
                                  the configured viewer, like `open`

transitions
  triage <task-id> <parked|ready|reject>
  start <task-id>                 ready → running
  ask <task-id> --question TEXT   running → needs-input
  resume <task-id>                needs-input → running, once you have answered
                                  the question in the agent's own session
  done <task-id> [--summary TEXT | --summary-file PATH] [--branch NAME]
                                  running | stalled → review (from stalled:
                                  reporting a dead session's finished work on
                                  its behalf); the summary is the agent's
                                  completion note (kept as a summary event, and
                                  the PR body `pr` opens from) — write it as a
                                  PR description; --branch records the git branch
                                  the work landed on (agent return path). Warns
                                  but succeeds when branch or summary is absent
  accept <task-id> [--yes]        review | waiting → done; then offers to
                                  remove the task's dispatch worktree (--yes
                                  skips the confirmation)
  reject <task-id> [TEXT] [--from-pr]
                                  review | waiting → running; TEXT is the
                                  feedback, or --from-pr pulls the tracked PR's
                                  review comments as the feedback (TEXT appended)
  wait <task-id>                  review → waiting; hand the work off to an
                                  external party (a PR awaiting someone else's
                                  review or merge) — out of the queue until it
                                  is your move again
  reclaim <task-id>               waiting → review; pull handed-off work back
                                  when it is your move again
  abort <task-id>                 running → ready
  park <task-id>                  ready → parked
  unpark <task-id>                parked → ready
  abandon <task-id> [--yes]       parked|ready|needs-input|review|waiting →
                                  rejected; then offers to remove the task's
                                  worktree
";

#[derive(Parser)]
#[command(
    name = "voro",
    disable_help_flag = true,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    Weight {
        project: String,
        weight: i64,
    },
    Add(AddArgs),
    Propose(ProposeArgs),
    Set(SetArgs),
    Show {
        task_id: i64,
    },
    List(ListArgs),
    Inbox,
    Next,
    Stats,
    Explain {
        task_id: i64,
    },
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    Dispatch {
        task_id: i64,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Run a configured viewer on a review/running task's checkout so its diff
    /// can be seen (DESIGN.md §8/§11a) — the explicit spelling of `pr`'s viewer
    /// medium, reaching the local diff even on a GitHub project.
    Open {
        task_id: i64,
    },
    Viewer {
        #[command(subcommand)]
        cmd: ViewerCmd,
    },
    Pr {
        task_id: i64,
        #[arg(long)]
        yes: bool,
    },
    Reject(RejectArgs),
    Done(DoneArgs),
    Import(ImportArgs),
    Triage {
        task_id: i64,
        target: TriageTarget,
    },
    Start {
        task_id: i64,
    },
    Ask(AskArgs),
    /// needs-input → running: the operator answered the question in the
    /// agent's own session (DESIGN.md §6/§8), so this only moves the state.
    Resume {
        task_id: i64,
    },
    Accept {
        task_id: i64,
        #[arg(long)]
        yes: bool,
    },
    Wait {
        task_id: i64,
    },
    Reclaim {
        task_id: i64,
    },
    Abort {
        task_id: i64,
    },
    Park {
        task_id: i64,
    },
    Unpark {
        task_id: i64,
    },
    Abandon {
        task_id: i64,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ProjectCmd {
    Add { name: String, path: String },
    List,
    Rename { project: String, name: String },
    Path { project: String, path: String },
    Archive { project: String },
    Unarchive { project: String },
    Delete { project: String },
    Action { project: String, action: String },
}

#[derive(Subcommand)]
enum AgentCmd {
    Init,
    List,
    Path,
}

#[derive(Subcommand)]
enum ViewerCmd {
    List,
    Add { name: String, cmd: String },
    Remove { name: String },
}

#[derive(Args)]
struct AddArgs {
    project: String,
    #[arg(value_name = "TITLE")]
    title: Vec<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long, conflicts_with = "body")]
    body_file: Option<String>,
    #[arg(long, value_parser = parse_priority)]
    priority: Option<Priority>,
    #[arg(long)]
    state: Option<String>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    blocked_by: Option<String>,
    #[arg(long)]
    blocks: Option<String>,
    #[arg(long)]
    human: bool,
}

#[derive(Args)]
struct ProposeArgs {
    project: String,
    #[arg(value_name = "TITLE")]
    title: Vec<String>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long, conflicts_with = "body")]
    body_file: Option<String>,
    #[arg(long)]
    from: Option<i64>,
    /// Hidden: accepted only so the handler can refuse it with a pointer to
    /// `add --state` instead of a generic unknown-argument error.
    #[arg(long, hide = true)]
    state: Option<String>,
}

#[derive(Args)]
struct SetArgs {
    task_id: i64,
    #[arg(long)]
    title: Option<String>,
    #[arg(long, value_parser = parse_priority)]
    priority: Option<Priority>,
    #[arg(long)]
    agent: Option<String>,
    #[arg(long)]
    no_agent: bool,
    #[arg(long)]
    body: Option<String>,
    #[arg(long, conflicts_with = "body")]
    body_file: Option<String>,
    #[arg(long)]
    blocked_by: Option<String>,
    #[arg(long)]
    blocks: Option<String>,
    #[arg(long)]
    pr: Option<String>,
    #[arg(long, conflicts_with = "pr")]
    no_pr: bool,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long, conflicts_with = "branch")]
    no_branch: bool,
    #[arg(long)]
    human: bool,
    #[arg(long, conflicts_with = "human")]
    no_human: bool,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long, conflicts_with = "summary")]
    summary_file: Option<String>,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    state: Option<String>,
    #[arg(long)]
    project: Option<String>,
}

#[derive(Args)]
struct RejectArgs {
    task_id: i64,
    #[arg(value_name = "TEXT")]
    text: Vec<String>,
    #[arg(long)]
    from_pr: bool,
}

#[derive(Args)]
struct DoneArgs {
    task_id: i64,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long, conflicts_with = "summary")]
    summary_file: Option<String>,
    #[arg(long)]
    branch: Option<String>,
}

#[derive(Args)]
struct ImportArgs {
    project: String,
    #[arg(long)]
    repo: Option<String>,
}

#[derive(Args)]
struct AskArgs {
    task_id: i64,
    #[arg(value_name = "TEXT")]
    text: Vec<String>,
    #[arg(long)]
    question: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum TriageTarget {
    Parked,
    Ready,
    Reject,
}

impl From<TriageTarget> for Triage {
    fn from(target: TriageTarget) -> Triage {
        match target {
            TriageTarget::Parked => Triage::Parked,
            TriageTarget::Ready => Triage::Ready,
            TriageTarget::Reject => Triage::Reject,
        }
    }
}

pub fn run(store: &mut Store, args: Vec<String>, ctx: &DispatchCtx) -> Result<String, String> {
    // Reconcile-on-read (DESIGN.md §8): before any verb consults session or
    // task state, close out sessions whose process has already exited.
    crate::reconcile::reconcile_live_sessions(store, &ctx.agents_path)
        .map_err(|e| e.to_string())?;

    // Every help request gets the hand-written overview page; clap's own help
    // machinery is disabled so it can never shadow this.
    if args.is_empty() || args[0] == "help" || args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(HELP.to_string());
    }
    let cli = Cli::try_parse_from(std::iter::once("voro".to_string()).chain(args))
        .map_err(|e| e.to_string().trim_end().to_string())?;
    match cli.verb {
        Verb::Project { cmd } => project_verb(store, cmd),
        Verb::Weight { project, weight } => weight_verb(store, &project, weight),
        Verb::Add(args) => add_verb(store, args),
        Verb::Propose(args) => propose_verb(store, args, ctx.session_task_id.clone()),
        Verb::Set(args) => set_verb(store, args),
        Verb::Show { task_id } => show_verb(store, task_id),
        Verb::List(args) => list_verb(store, &args),
        Verb::Inbox => inbox_verb(store),
        Verb::Next => next_verb(store),
        Verb::Stats => stats_verb(store),
        Verb::Explain { task_id } => explain_verb(store, task_id),
        Verb::Agent { cmd } => agent_verb(cmd, ctx),
        Verb::Dispatch { task_id, agent } => {
            dispatch::dispatch(store, ctx, task_id, agent.as_deref())
        }
        Verb::Open { task_id } => dispatch::open(store, ctx, task_id, None),
        Verb::Viewer { cmd } => viewer_verb(store, cmd, ctx),
        Verb::Pr { task_id, yes } => pr_verb(store, task_id, yes, ctx),
        Verb::Reject(args) => reject_verb(store, args),
        Verb::Done(args) => done_verb(store, args),
        Verb::Import(args) => import_verb(store, args),
        Verb::Triage { task_id, target } => {
            apply_action(store, task_id, Action::Triage(target.into()), false)
        }
        Verb::Start { task_id } => apply_action(store, task_id, Action::Start, false),
        Verb::Ask(args) => ask_verb(store, args),
        Verb::Resume { task_id } => apply_action(store, task_id, Action::Resume, false),
        Verb::Accept { task_id, yes } => apply_action(store, task_id, Action::Accept, yes),
        Verb::Wait { task_id } => apply_action(store, task_id, Action::HandOff, false),
        Verb::Reclaim { task_id } => apply_action(store, task_id, Action::Reclaim, false),
        Verb::Abort { task_id } => apply_action(store, task_id, Action::Abort, false),
        Verb::Park { task_id } => apply_action(store, task_id, Action::Park, false),
        Verb::Unpark { task_id } => apply_action(store, task_id, Action::Unpark, false),
        Verb::Abandon { task_id, yes } => apply_action(store, task_id, Action::Abandon, yes),
    }
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

fn parse_ids(flag: &str, raw: &str) -> Result<Vec<i64>, String> {
    raw.split([',', ' '])
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.trim()
                .parse()
                .map_err(|_| format!("{flag} must be task ids, got '{s}'"))
        })
        .collect()
}

/// Apply `--blocks IDS`: make `blocker_id` a blocker of each listed task, and
/// echo every edge loudly — `task 104 blocks #43 — #43 demoted to parked` —
/// so authoring the wrong direction by muscle memory is visible immediately.
fn apply_blocks_flag(store: &mut Store, blocker_id: i64, raw: &str) -> Result<String, String> {
    let affected = store
        .block_tasks(blocker_id, &parse_ids("blocks", raw)?)
        .map_err(|e| e.to_string())?;
    let mut out = String::new();
    for (dep, before) in affected {
        write!(out, "\ntask {} blocks #{}", blocker_id, dep.id).unwrap();
        if before == TaskState::Ready && dep.state == TaskState::Parked {
            write!(out, " — #{} demoted to parked", dep.id).unwrap();
        }
    }
    Ok(out)
}

/// A value given inline (`--body TEXT`) or read from a file (`--body-file
/// PATH`), the latter for multi-line PR-ready text (DESIGN.md §8). The pairs
/// are mutually exclusive in the parser, so at most one arrives here; `None`
/// when neither is given stays valid.
fn text_or_file(text: Option<String>, path: Option<String>) -> Result<Option<String>, String> {
    match (text, path) {
        (Some(text), _) => Ok(Some(text)),
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| format!("cannot read {path}: {e}")),
        (None, None) => Ok(None),
    }
}

/// Free-text positionals (a title, an answer, rejection feedback) arrive as
/// the words the shell split them into; join them back and refuse emptiness.
fn joined(words: &[String], what: &str) -> Result<String, String> {
    let text = words.join(" ");
    if text.trim().is_empty() {
        return Err(format!("missing {what} — try 'voro help'"));
    }
    Ok(text)
}

// --- verbs ---

fn project_verb(store: &mut Store, cmd: ProjectCmd) -> Result<String, String> {
    match cmd {
        ProjectCmd::Add { name, path } => {
            let p = store
                .create_project(&name, &path)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} '{}' created (weight {})",
                p.id, p.name, p.weight
            ))
        }
        ProjectCmd::List => {
            let mut out = String::new();
            for p in store.projects().map_err(|e| e.to_string())? {
                let action = match &p.review_action {
                    ReviewAction::Auto => String::new(),
                    other => format!("  [{other}]"),
                };
                let archived = if p.archived { "  [archived]" } else { "" };
                writeln!(
                    out,
                    "{:3}  w{}  {}  {}{action}{archived}",
                    p.id, p.weight, p.name, p.path
                )
                .unwrap();
            }
            Ok(out)
        }
        ProjectCmd::Rename { project, name } => {
            let project = resolve_project(store, &project)?;
            let p = store
                .rename_project(project.id, &name)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} renamed '{}' -> '{}'",
                p.id, project.name, p.name
            ))
        }
        ProjectCmd::Path { project, path } => {
            let project = resolve_project(store, &project)?;
            let p = store
                .set_path(project.id, &path)
                .map_err(|e| e.to_string())?;
            Ok(format!("project {} path -> {}", p.id, p.path))
        }
        ProjectCmd::Archive { project } => {
            let project = resolve_project(store, &project)?;
            let p = store
                .set_archived(project.id, true)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} '{}' archived — hidden from the cockpit with all its tasks; \
                 `voro project unarchive {}` restores them",
                p.id, p.name, p.name
            ))
        }
        ProjectCmd::Unarchive { project } => {
            let project = resolve_project(store, &project)?;
            let p = store
                .set_archived(project.id, false)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} '{}' unarchived — its tasks are back in their prior states",
                p.id, p.name
            ))
        }
        ProjectCmd::Delete { project } => {
            let project = resolve_project(store, &project)?;
            store
                .delete_project(project.id)
                .map_err(|e| e.to_string())?;
            Ok(format!("project {} '{}' deleted", project.id, project.name))
        }
        ProjectCmd::Action { project, action } => {
            let project = resolve_project(store, &project)?;
            let action = ReviewAction::parse(&action).map_err(|e| e.to_string())?;
            let p = store
                .set_review_action(project.id, &action)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{} review action {} -> {}",
                p.name, project.review_action, p.review_action
            ))
        }
    }
}

/// Manage the `voro.toml` that dispatch resolves against — config that lives
/// outside the database (DESIGN.md §8), so this verb takes no `store`.
fn agent_verb(cmd: AgentCmd, ctx: &DispatchCtx) -> Result<String, String> {
    let path = &ctx.agents_path;
    match cmd {
        AgentCmd::Init => {
            AgentsConfig::write_starter(path).map_err(|e| e.to_string())?;
            Ok(format!(
                "wrote a config skeleton to {} — optional, since the built-in claude/codex \
                 agents already dispatch; edit it to add or override agents, or set options \
                 like `default_agent` and `[viewer]`",
                path.display()
            ))
        }
        AgentCmd::List => {
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
                    ("plan", template.plan()),
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
        AgentCmd::Path => Ok(path.display().to_string()),
    }
}

fn weight_verb(store: &mut Store, project: &str, weight: i64) -> Result<String, String> {
    let project = resolve_project(store, project)?;
    store
        .set_weight(project.id, weight)
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{} weight {} -> {}",
        project.name, project.weight, weight
    ))
}

fn add_verb(store: &mut Store, args: AddArgs) -> Result<String, String> {
    let project = resolve_project(store, &args.project)?;
    let title = joined(&args.title, "title")?;
    let state = match args.state.as_deref() {
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
    let task = store
        .create_task(NewTask {
            project_id: project.id,
            title,
            body: text_or_file(args.body, args.body_file)?.unwrap_or_default(),
            priority: args.priority.unwrap_or(Priority::P2),
            state,
            agent: args.agent,
            human: args.human,
        })
        .map_err(|e| e.to_string())?;
    let task = match &args.blocked_by {
        Some(raw) => store
            .set_blocks_deps(task.id, &parse_ids("blocked-by", raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    let mut out = format!("task {} '{}' created ({})", task.id, task.title, task.state);
    if let Some(raw) = &args.blocks {
        out.push_str(&apply_blocks_flag(store, task.id, raw)?);
    }
    Ok(out)
}

/// The agent return-path form of `add` (DESIGN.md §8): always lands in
/// `proposed`, and links the new task discovered-from its source — `--from`
/// explicitly, or the `VORO_TASK_ID` a dispatched session runs under.
fn propose_verb(
    store: &mut Store,
    args: ProposeArgs,
    env_source: Option<String>,
) -> Result<String, String> {
    if args.state.is_some() {
        return Err("propose always creates 'proposed' tasks — use 'add --state' instead".into());
    }
    let project = resolve_project(store, &args.project)?;
    let title = joined(&args.title, "title")?;
    let source_id = match (args.from, env_source) {
        (Some(id), _) => Some(id),
        (None, Some(raw)) => Some(
            raw.parse()
                .map_err(|_| format!("'{raw}' is not a task id"))?,
        ),
        (None, None) => None,
    };
    let source = match source_id {
        Some(id) => Some(store.task(id).map_err(|e| e.to_string())?),
        None => None,
    };
    let task = store
        .create_task(NewTask {
            project_id: project.id,
            title,
            body: text_or_file(args.body, args.body_file)?.unwrap_or_default(),
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

fn set_verb(store: &mut Store, args: SetArgs) -> Result<String, String> {
    let id = args.task_id;
    let current = store.task(id).map_err(|e| e.to_string())?;
    let agent = if args.no_agent {
        None
    } else {
        args.agent.or(current.agent)
    };
    let human = match (args.human, args.no_human) {
        (true, _) => true,
        (_, true) => false,
        (false, false) => current.human,
    };
    let edit = TaskEdit {
        title: args.title.unwrap_or(current.title),
        body: text_or_file(args.body, args.body_file)?.unwrap_or(current.body),
        priority: args.priority.unwrap_or(current.priority),
        agent,
        human,
    };
    let task = store.update_task(id, edit).map_err(|e| e.to_string())?;
    let task = match &args.blocked_by {
        Some(raw) => store
            .set_blocks_deps(id, &parse_ids("blocked-by", raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    let blocks_echo = match &args.blocks {
        Some(raw) => apply_blocks_flag(store, id, raw)?,
        None => String::new(),
    };
    let task = if args.no_pr {
        store.set_pr(id, None).map_err(|e| e.to_string())?
    } else if let Some(raw) = &args.pr {
        // Validate and canonicalise the reference before storing, so a
        // tracked PR is always addressable and the stored form is stable.
        let pr = PrRef::parse(raw).map_err(|e| e.to_string())?;
        store.set_pr(id, Some(&pr.url)).map_err(|e| e.to_string())?
    } else {
        task
    };
    let task = if args.no_branch {
        store.set_branch(id, None).map_err(|e| e.to_string())?
    } else if let Some(name) = &args.branch {
        store
            .set_branch(id, Some(name.trim()))
            .map_err(|e| e.to_string())?
    } else {
        task
    };
    let task = match text_or_file(args.summary, args.summary_file)? {
        Some(text) => store.set_summary(id, &text).map_err(|e| e.to_string())?,
        None => task,
    };
    Ok(format!(
        "task {} updated ({}){blocks_echo}",
        task.id, task.state
    ))
}

/// `pr <task-id> [--yes]` (DESIGN.md §8/§11c): the per-project "show me this
/// task's diff" action. With a tracked PR, open it in a browser. Without one,
/// resolve the project's review medium: GitHub creates the PR from the review
/// task's done-time state (asserting PR-readiness, confirming unless `--yes`),
/// a viewer project opens the checkout, as `open` does.
fn pr_verb(store: &mut Store, id: i64, yes: bool, ctx: &DispatchCtx) -> Result<String, String> {
    let task = store.task(id).map_err(|e| e.to_string())?;
    if task.pr_url.is_some() {
        return crate::pr::open(store, id);
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    if let ReviewMedium::Viewer(viewer) = crate::pr::resolve_medium(&project) {
        return dispatch::open(store, ctx, id, viewer.as_deref());
    }
    // Assert PR-ready and learn the branch before prompting, so a task missing
    // state, branch, or summary fails naming the gap rather than at the prompt.
    let plan = crate::pr::plan(store, id)?;
    if !yes && !confirm(&format!("push `{}` and open a PR for #{id}?", plan.branch))? {
        return Ok(format!("cancelled — no PR opened for #{id}"));
    }
    crate::pr::create(store, id)
}

/// Ask a yes/no question on the terminal, defaulting to no (DESIGN.md §8). A
/// non-interactive stdin (a pipe at EOF) reads as "no", so a scripted run
/// without `--yes` declines rather than blocking.
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

/// `reject <task-id> [TEXT] [--from-pr]` (DESIGN.md §6/§8/§11c): review |
/// waiting → running with feedback appended to the body. `--from-pr` pulls the
/// tracked PR's review comments as that feedback, with any extra TEXT appended;
/// otherwise TEXT is the feedback. Because `review`/`waiting` keep the session
/// open (§8), the task returns to `running` with its agent session still live —
/// the operator addresses the feedback in that same session (§6/§8).
fn reject_verb(store: &mut Store, args: RejectArgs) -> Result<String, String> {
    let id = args.task_id;
    let feedback = if args.from_pr {
        let pulled = crate::pr::pull_review_feedback(store, id)?;
        let extra = args.text.join(" ");
        if extra.trim().is_empty() {
            pulled
        } else {
            format!("{pulled}\n{extra}")
        }
    } else {
        joined(&args.text, "rejection feedback")?
    };

    let task = store
        .apply(id, Action::RejectWork(feedback))
        .map_err(|e| e.to_string())?;
    Ok(format!("task {} -> {}", task.id, task.state))
}

/// `done <task-id> [--summary TEXT | --summary-file PATH] [--branch NAME]`
/// (DESIGN.md §6/§8): running | stalled → review — from `stalled` it reports a
/// dead session's finished work on its behalf. The summary is the completion
/// note, read back as the PR body when `pr` opens a pull request. `--branch` is
/// the branch the agent reports its work landed on (task #81), overwriting any
/// intended name dispatch injected. The transition applies first, so a task
/// that is neither `running` nor `stalled` is refused before any branch is
/// recorded. A `done` that leaves the task without a branch or summary *warns*
/// rather than failing, so both stay optional through the lifecycle.
fn done_verb(store: &mut Store, args: DoneArgs) -> Result<String, String> {
    let id = args.task_id;
    let summary = text_or_file(args.summary, args.summary_file)?;
    let task = store
        .apply(id, Action::Complete(summary))
        .map_err(|e| e.to_string())?;
    let mut out = format!("task {} -> {}", task.id, task.state);
    if let Some(name) = &args.branch {
        store
            .set_branch(id, Some(name.trim()))
            .map_err(|e| e.to_string())?;
        write!(out, " (branch {})", name.trim()).unwrap();
    }
    // A complete report carries both a branch and a summary whatever the review
    // medium, so warn (never fail) about whichever is absent. A human task lands
    // straight in `done` with no report to read, so it earns no warning.
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
                "\nnote: no {} recorded — complete the report with `voro set {id}` if this task produced code",
                missing.join(" or ")
            )
            .unwrap();
        }
    }
    Ok(out)
}

fn show_verb(store: &mut Store, id: i64) -> Result<String, String> {
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
    if let Some(verb) = task.next_action() {
        writeln!(out, "next: {verb}").unwrap();
    }
    if store
        .incomplete_report_flag(id)
        .map_err(|e| e.to_string())?
    {
        writeln!(
            out,
            "incomplete report: only one of branch and summary recorded — complete it with `voro set {id}`"
        )
        .unwrap();
    }
    let deps = store.deps_of(id).map_err(|e| e.to_string())?;
    for dep in &deps {
        // `deps(task_id, depends_on, 'blocks')` means this task is blocked
        // *by* `depends_on` — say so, rather than reading the kind backwards.
        match dep.kind {
            DepKind::Blocks => writeln!(out, "dep: blocked by #{}", dep.depends_on).unwrap(),
            _ => writeln!(out, "dep: {} {}", dep.kind, dep.depends_on).unwrap(),
        }
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

fn list_verb(store: &mut Store, args: &ListArgs) -> Result<String, String> {
    let state_filter = match &args.state {
        Some(raw) => Some(TaskState::parse(raw).map_err(|e| e.to_string())?),
        None => None,
    };
    let project_filter = match &args.project {
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
        let incomplete = incomplete_report_suffix(store, task.id);
        let suffix = if incomplete.is_empty() {
            review_next_suffix(&task)
        } else {
            incomplete.to_string()
        };
        writeln!(out, "{}{}", task_line(&task, name), suffix).unwrap();
    }
    Ok(out)
}

/// A review row's next action as a browser suffix (DESIGN.md §3). The list
/// shows state in its own column, so only `review` — whose verb reads the
/// tracked PR, not the state alone — earns the suffix.
fn review_next_suffix(task: &Task) -> String {
    if task.state != TaskState::Review {
        return String::new();
    }
    task.next_action()
        .map_or_else(String::new, |verb| format!("  next: {verb}"))
}

fn inbox_verb(store: &mut Store) -> Result<String, String> {
    let candidates = store.candidates().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for c in scheduler::queue(&candidates) {
        // The queue row carries the verb instead of the state: every inbox row
        // is a next action (DESIGN.md §3), like the TUI queue.
        write!(
            out,
            "{:5.1}  #{} {:10} {} {}: {}",
            c.score.total,
            c.task.id,
            c.task.next_action().map_or("", |a| a.as_str()),
            c.task.priority,
            c.project_name,
            c.task.title
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

/// Task counts by state (DESIGN.md §12) as a scriptable readout, excluding
/// parked projects so the numbers match the queue and header.
fn stats_verb(store: &mut Store) -> Result<String, String> {
    let c = store.state_counts().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for (label, n) in [
        ("triage", c.proposed),
        ("ready", c.ready),
        ("running", c.running),
        ("needs-input", c.needs_input),
        ("review", c.review),
        ("waiting", c.waiting),
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
/// and a summary (DESIGN.md §8), else empty. Deliberately does not resolve the
/// review medium, since `auto` resolution probes `gh` and this renders per line.
fn incomplete_report_suffix(store: &Store, task_id: i64) -> &'static str {
    if store.incomplete_report_flag(task_id).unwrap_or(false) {
        "  [incomplete report]"
    } else {
        ""
    }
}

fn explain_verb(store: &mut Store, id: i64) -> Result<String, String> {
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

/// `viewer list/add/remove` (DESIGN.md §8/§11a): read and edit the viewers
/// `voro.toml` defines. `add`/`remove` route through the same comment-preserving
/// write helper the TUI Config screen uses; `remove` needs the store to refuse
/// deleting a viewer a project's review action still names.
fn viewer_verb(store: &mut Store, cmd: ViewerCmd, ctx: &DispatchCtx) -> Result<String, String> {
    let path = &ctx.agents_path;
    match cmd {
        ViewerCmd::Add { name, cmd } => {
            voro_core::config_edit::add_viewer(path, &name, &cmd).map_err(|e| e.to_string())?;
            let mut out = format!("viewer '{name}' added: {cmd}");
            if voro_core::config_edit::missing_path_placeholder(&cmd) {
                out.push_str(
                    "\nnote: the command has no {path} placeholder, so it will run in the \
                     checkout directory itself",
                );
            }
            Ok(out)
        }
        ViewerCmd::Remove { name } => {
            let projects = store.projects().map_err(|e| e.to_string())?;
            let referencing = voro_core::config_edit::projects_referencing_viewer(&projects, &name);
            if !referencing.is_empty() {
                let names: Vec<&str> = referencing.iter().map(|p| p.name.as_str()).collect();
                return Err(format!(
                    "viewer '{name}' is the review action of {} — repoint {} with `voro project \
                     action <project> <auto|pr|viewer:NAME>` before removing it",
                    names.join(", "),
                    if referencing.len() == 1 { "it" } else { "them" }
                ));
            }
            let cleared =
                voro_core::config_edit::delete_viewer(path, &name).map_err(|e| e.to_string())?;
            let mut out = format!("viewer '{name}' removed");
            if cleared {
                out.push_str(" — it was the default, so default_viewer is now unset");
            }
            Ok(out)
        }
        ViewerCmd::List => {
            let config = AgentsConfig::load(path).map_err(|e| e.to_string())?;
            let names = config.viewer_names();
            let default = config.default_viewer_name();
            let mut out = String::new();
            for name in &names {
                let marker = if Some(name.as_str()) == default.as_deref() {
                    "* "
                } else {
                    "  "
                };
                let cmd = config.viewer_cmd(Some(name)).map_err(|e| e.to_string())?;
                writeln!(out, "{marker}{name}  {cmd}").unwrap();
            }
            // The anonymous [viewer] table has no name but still resolves as
            // the default; show it so `list` reflects what `open` will run.
            if default.is_none()
                && let Ok(cmd) = config.viewer_cmd(None)
            {
                writeln!(out, "* [viewer]  {cmd}").unwrap();
            }
            if out.is_empty() {
                writeln!(
                    out,
                    "no viewers configured — add a [viewers.<name>] table to {} with a cmd \
                     such as 'zed {{path}}' or 'git difftool -d'",
                    path.display()
                )
                .unwrap();
            } else {
                writeln!(out, "\n({} — * is the default)", path.display()).unwrap();
            }
            Ok(out)
        }
    }
}

/// Milestone C's one-way GitHub import (DESIGN.md §10): shells out to `gh
/// issue list` in the project's path (or `--repo owner/name` if the checkout
/// itself doesn't name the repo to import from) and captures each issue as a
/// `proposed` task, skipping ones already imported.
fn import_verb(store: &mut Store, args: ImportArgs) -> Result<String, String> {
    let project = resolve_project(store, &args.project)?;
    let json = import::fetch_issues(&project.path, args.repo.as_deref())?;
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

fn ask_verb(store: &mut Store, args: AskArgs) -> Result<String, String> {
    let question = match args.question {
        Some(q) => q,
        None => joined(&args.text, "question (--question TEXT)")?,
    };
    apply_action(store, args.task_id, Action::Ask(question), false)
}

/// Apply a plain state-machine action. `accept`/`abandon` are the terminal
/// transitions that own the dispatch worktree's teardown (§8; `yes` skips its
/// confirmation). The transition applies first and stands regardless of cleanup.
fn apply_action(store: &mut Store, id: i64, action: Action, yes: bool) -> Result<String, String> {
    let closes = matches!(action, Action::Accept | Action::Abandon);
    let task = store.apply(id, action).map_err(|e| e.to_string())?;
    let mut out = format!("task {} -> {}", task.id, task.state);
    if closes && let Some(line) = clean_up_worktree(store, &task, yes)? {
        out.push('\n');
        out.push_str(&line);
    }
    Ok(out)
}

/// Remove the worktree of a just-closed task after showing the operator what
/// will go and confirming (`--yes` skips the prompt). Declining still returns
/// `Ok`, so the transition it followed stands; `None` means nothing to clean
/// (no branch, or no matching worktree).
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
            session_task_id: None,
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
    fn viewer_add_remove_round_trip_through_the_cli() {
        let dir = std::env::temp_dir().join(format!(
            "voro-cli-viewers-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agents_path = dir.join("voro/voro.toml");
        let ctx = DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            session_task_id: None,
        };
        let mut s = store();
        let call = |s: &mut Store, args: &[&str]| {
            run(s, args.iter().map(|x| x.to_string()).collect(), &ctx)
        };

        // add then list shows it — `viewer list` gains its inverse
        let out = call(&mut s, &["viewer", "add", "zed", "zed {path}"]).unwrap();
        assert!(out.contains("zed"), "{out}");
        let listed = call(&mut s, &["viewer", "list"]).unwrap();
        assert!(
            listed.contains("zed") && listed.contains("zed {path}"),
            "{listed}"
        );

        // a duplicate name is refused
        let e = call(&mut s, &["viewer", "add", "zed", "zed ."]).unwrap_err();
        assert!(e.contains("already exists"), "{e}");

        // an empty command is refused
        let e = call(&mut s, &["viewer", "add", "emacs", "   "]).unwrap_err();
        assert!(e.contains("command is required"), "{e}");

        // a command with no {path} succeeds but warns
        let out = call(&mut s, &["viewer", "add", "difftool", "git difftool -d"]).unwrap();
        assert!(out.contains("{path}"), "{out}");

        // a project pinned to viewer:zed blocks its removal, naming the project
        call(&mut s, &["project", "add", "demo", "/tmp/demo"]).unwrap();
        call(&mut s, &["project", "action", "demo", "viewer:zed"]).unwrap();
        let e = call(&mut s, &["viewer", "remove", "zed"]).unwrap_err();
        assert!(e.contains("demo") && e.contains("review action"), "{e}");

        // repoint the project, then removal succeeds and list loses it
        call(&mut s, &["project", "action", "demo", "auto"]).unwrap();
        let out = call(&mut s, &["viewer", "remove", "zed"]).unwrap();
        assert!(out.contains("removed"), "{out}");
        let listed = call(&mut s, &["viewer", "list"]).unwrap();
        assert!(!listed.contains("zed"), "{listed}");
        assert!(listed.contains("difftool"), "{listed}");

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
    fn project_archive_hides_the_cockpit_views_and_unarchive_restores_them() {
        // The acceptance walk (task #136): a project with open and closed
        // tasks leaves inbox/next/stats wholesale on archive, stays tagged on
        // `project list`, and comes back exactly on unarchive.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp/demo"]);
        ok(&mut s, &["add", "demo", "Open task", "--state", "ready"]);
        ok(&mut s, &["add", "demo", "Closed task", "--state", "ready"]);
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["done", "2"]);
        ok(&mut s, &["accept", "2", "--yes"]);

        let inbox_before = ok(&mut s, &["inbox"]);
        assert!(inbox_before.contains("Open task"), "{inbox_before}");

        let out = ok(&mut s, &["project", "archive", "demo"]);
        assert!(out.contains("archived"), "{out}");
        assert!(ok(&mut s, &["inbox"]).contains("nothing needs you"));
        assert!(ok(&mut s, &["next"]).contains("no ready tasks"));
        assert!(ok(&mut s, &["stats"]).contains("ready       0"));
        let listed = ok(&mut s, &["project", "list"]);
        assert!(
            listed.contains("demo") && listed.contains("[archived]"),
            "{listed}"
        );

        // side doors: no new work lands on an archived project
        let e = err(&mut s, &["add", "demo", "Too late"]);
        assert!(e.contains("archived"), "{e}");
        let e = err(&mut s, &["propose", "demo", "Too late"]);
        assert!(e.contains("archived"), "{e}");
        // a second archive is heard, not absorbed
        let e = err(&mut s, &["project", "archive", "demo"]);
        assert!(e.contains("already archived"), "{e}");

        let out = ok(&mut s, &["project", "unarchive", "demo"]);
        assert!(out.contains("unarchived"), "{out}");
        assert_eq!(ok(&mut s, &["inbox"]), inbox_before);
        assert!(ok(&mut s, &["stats"]).contains("done        1"));
        assert!(!ok(&mut s, &["project", "list"]).contains("[archived]"));
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

        let out = ok(&mut s, &["resume", "1"]);
        assert!(out.contains("-> running"), "{out}");
        // the question is cleared once the task resumes
        assert!(!ok(&mut s, &["show", "1"]).contains("Schema A or B?"));

        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["reject", "1", "tests missing"]);
        ok(&mut s, &["done", "1"]);
        let out = ok(&mut s, &["accept", "1"]);
        assert!(out.contains("-> done"), "{out}");
    }

    /// `wait` hands a review task off to an external party and `reclaim` pulls
    /// it back; `accept` then closes it (DESIGN.md §6).
    #[test]
    fn wait_reclaim_and_accept_from_waiting_through_the_cli() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "A task", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);

        let out = ok(&mut s, &["wait", "1"]);
        assert!(out.contains("-> waiting"), "{out}");
        // wait is refused once the task is no longer in review
        assert!(err(&mut s, &["wait", "1"]).contains("hand off"));

        let out = ok(&mut s, &["reclaim", "1"]);
        assert!(out.contains("-> review"), "{out}");

        ok(&mut s, &["wait", "1"]);
        let out = ok(&mut s, &["accept", "1"]);
        assert!(out.contains("-> done"), "{out}");
    }

    /// `reject` works from `waiting` as well as `review`, requeuing the task
    /// with the feedback in hand (DESIGN.md §6/§8).
    #[test]
    fn reject_from_waiting_requeues_with_feedback() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "A task", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["wait", "1"]);

        let out = ok(&mut s, &["reject", "1", "reviewer wants tests"]);
        assert!(out.contains("-> running"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("reviewer wants tests"));
    }

    #[test]
    fn default_state_is_proposed_and_triage_works() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        assert!(ok(&mut s, &["inbox"]).contains("P2 demo: An idea"));
        ok(&mut s, &["triage", "1", "ready"]);
        assert!(ok(&mut s, &["next"]).contains("An idea"));
    }

    /// The inbox renders each row's next-action verb in place of the state,
    /// mirroring the TUI queue — both from `Task::next_action()` (DESIGN.md §3).
    #[test]
    fn inbox_shows_the_next_action_verb_on_each_row() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        ok(&mut s, &["add", "demo", "Startable", "--state", "ready"]);
        ok(
            &mut s,
            &["add", "demo", "By hand", "--state", "ready", "--human"],
        );
        ok(&mut s, &["add", "demo", "Blocked", "--state", "ready"]);
        ok(&mut s, &["start", "4"]);
        ok(&mut s, &["ask", "4", "--question", "Schema A or B?"]);

        let out = ok(&mut s, &["inbox"]);
        assert!(out.contains("#1 triage"), "{out}");
        assert!(out.contains("#2 dispatch"), "{out}");
        assert!(out.contains("#3 do"), "{out}");
        assert!(out.contains("#4 answer"), "{out}");
    }

    /// A review row's verb reads the tracked PR: `pr` without one, `review PR`
    /// with — the same sub-state rendering as the TUI (DESIGN.md §6).
    #[test]
    fn inbox_review_verb_follows_the_tracked_pr() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let id = review_task(&mut s, Some("feat/thing"), Some("did it"));
        let out = ok(&mut s, &["inbox"]);
        assert!(out.contains(&format!("#{id} pr ")), "{out}");

        ok(&mut s, &["set", &id.to_string(), "--pr", "acme/widget#42"]);
        let out = ok(&mut s, &["inbox"]);
        assert!(out.contains("review PR"), "{out}");
    }

    /// `show` names the next action in its header whenever the task derives
    /// one, and drops the line on states that ask nothing of the human.
    #[test]
    fn show_names_the_next_action() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: triage"));

        ok(&mut s, &["triage", "1", "ready"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: dispatch"));

        ok(&mut s, &["start", "1"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("next:"));

        ok(&mut s, &["done", "1", "--summary", "did it"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: pr"));

        ok(&mut s, &["set", "1", "--pr", "acme/widget#42"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: review PR"));

        ok(&mut s, &["accept", "1"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("next:"));
    }

    /// `list` shows state in its own column, so only `review` earns a next
    /// suffix — and the incomplete-report marker takes its place when the
    /// report is half-finished, as in the TUI browser.
    #[test]
    fn list_suffixes_review_rows_with_the_next_action() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let complete = review_task(&mut s, Some("feat/thing"), Some("did it"));
        let half = review_task(&mut s, Some("feat/other"), None);
        ok(&mut s, &["add", "demo", "Startable", "--state", "ready"]);

        let out = ok(&mut s, &["list"]);
        let line = |id: i64| {
            out.lines()
                .find(|l| l.starts_with(&format!("#{id} ")))
                .unwrap_or_else(|| panic!("no row for #{id}: {out}"))
        };
        assert!(line(complete).ends_with("next: pr"), "{out}");
        assert!(line(half).ends_with("[incomplete report]"), "{out}");
        assert!(!line(3).contains("next:"), "{out}");

        ok(&mut s, &["set", &complete.to_string(), "--pr", "acme/w#1"]);
        let out = ok(&mut s, &["list"]);
        assert!(out.contains("next: review PR"), "{out}");
    }

    #[test]
    fn stats_reports_counts_by_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Idea one"]); // proposed by default
        ok(&mut s, &["add", "demo", "Idea two"]);
        ok(&mut s, &["add", "demo", "Ready one", "--state", "ready"]);
        // A handed-off task is counted under waiting, not review.
        ok(&mut s, &["add", "demo", "Handed off", "--state", "ready"]);
        ok(&mut s, &["start", "4"]);
        ok(&mut s, &["done", "4"]);
        ok(&mut s, &["wait", "4"]);
        // A parked project's tasks stay out of the tally.
        ok(&mut s, &["project", "add", "snoozed", "/tmp"]);
        ok(&mut s, &["weight", "snoozed", "0"]);
        ok(&mut s, &["add", "snoozed", "Hidden idea"]);

        let out = ok(&mut s, &["stats"]);
        assert!(out.contains(&format!("{:<12}{}", "triage", 2)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "ready", 1)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "review", 0)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "waiting", 1)), "{out}");
        assert!(out.contains(&format!("{:<12}{}", "done", 0)), "{out}");
    }

    #[test]
    fn blocked_by_flag_demotes_and_promotion_flows() {
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
                "--blocked-by",
                "1",
            ],
        );
        assert!(out.contains("(parked)"), "{out}");

        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["accept", "1"]);
        assert!(ok(&mut s, &["list", "--state", "ready"]).contains("Dependent"));
    }

    #[test]
    fn add_blocks_authors_the_reverse_edge_and_echoes_the_demotion() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Dependent", "--state", "ready"]);
        let out = ok(
            &mut s,
            &[
                "add",
                "demo",
                "Prerequisite",
                "--state",
                "ready",
                "--blocks",
                "1",
            ],
        );
        assert!(out.contains("task 2 blocks #1"), "{out}");
        assert!(out.contains("#1 demoted to parked"), "{out}");

        // the dependent carries the edge, and show names the blocker
        let shown = ok(&mut s, &["show", "1"]);
        assert!(shown.contains("blocked by #2"), "{shown}");
        assert!(!shown.contains("blocks 2"), "{shown}");
        assert!(
            ok(&mut s, &["show", "2"])
                .lines()
                .all(|l| !l.starts_with("dep:"))
        );

        // closing the prerequisite promotes the dependent
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["done", "2"]);
        ok(&mut s, &["accept", "2"]);
        assert!(ok(&mut s, &["list", "--state", "ready"]).contains("Dependent"));
    }

    #[test]
    fn add_blocks_echo_stays_quiet_without_a_demotion() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Deferred", "--state", "parked"]);
        let out = ok(
            &mut s,
            &[
                "add",
                "demo",
                "Prerequisite",
                "--state",
                "ready",
                "--blocks",
                "1",
            ],
        );
        assert!(out.contains("task 2 blocks #1"), "{out}");
        assert!(!out.contains("demoted"), "{out}");
    }

    #[test]
    fn set_blocks_adds_without_detaching_other_blockers() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(
            &mut s,
            &["add", "demo", "First blocker", "--state", "ready"],
        );
        ok(
            &mut s,
            &["add", "demo", "Second blocker", "--state", "ready"],
        );
        ok(
            &mut s,
            &[
                "add",
                "demo",
                "Dependent",
                "--state",
                "ready",
                "--blocked-by",
                "1",
            ],
        );

        let out = ok(&mut s, &["set", "2", "--blocks", "3"]);
        assert!(out.contains("task 2 blocks #3"), "{out}");
        let shown = ok(&mut s, &["show", "3"]);
        assert!(shown.contains("blocked by #1"), "{shown}");
        assert!(shown.contains("blocked by #2"), "{shown}");

        // re-adding the same edge is idempotent
        ok(&mut s, &["set", "2", "--blocks", "3"]);
        assert_eq!(s.deps_of(3).unwrap().len(), 2);
    }

    #[test]
    fn set_blocked_by_replaces_the_blocker_list() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(
            &mut s,
            &["add", "demo", "First blocker", "--state", "ready"],
        );
        ok(
            &mut s,
            &["add", "demo", "Second blocker", "--state", "ready"],
        );
        ok(
            &mut s,
            &[
                "add",
                "demo",
                "Dependent",
                "--state",
                "ready",
                "--blocked-by",
                "1",
            ],
        );

        ok(&mut s, &["set", "3", "--blocked-by", "2"]);
        let shown = ok(&mut s, &["show", "3"]);
        assert!(!shown.contains("blocked by #1"), "{shown}");
        assert!(shown.contains("blocked by #2"), "{shown}");
    }

    #[test]
    fn set_blocks_demotes_a_ready_dependent_loudly() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Dependent", "--state", "ready"]);
        ok(&mut s, &["add", "demo", "Prerequisite", "--state", "ready"]);

        let out = ok(&mut s, &["set", "2", "--blocks", "1"]);
        assert!(
            out.contains("task 2 blocks #1 — #1 demoted to parked"),
            "{out}"
        );
        assert!(ok(&mut s, &["show", "1"]).contains("#1 parked"));
    }

    #[test]
    fn both_blocks_directions_are_cycle_checked() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "A", "--state", "ready"]);
        ok(
            &mut s,
            &["add", "demo", "B", "--state", "ready", "--blocked-by", "1"],
        );

        // 2 waits on 1; making 2 block 1 would close the loop, either way round
        let e = err(&mut s, &["set", "2", "--blocks", "1"]);
        assert!(e.contains("cycle"), "{e}");
        let e = err(&mut s, &["set", "1", "--blocked-by", "2"]);
        assert!(e.contains("cycle"), "{e}");
        let e = err(&mut s, &["add", "demo", "C", "--blocks", "3"]);
        assert!(e.contains("cycle"), "{e}");
    }

    fn propose(store: &mut Store, args: &[&str], env: Option<&str>) -> Result<String, String> {
        let cli = Cli::try_parse_from(std::iter::once("voro").chain(args.iter().copied()))
            .map_err(|e| e.to_string())?;
        let Verb::Propose(parsed) = cli.verb else {
            panic!("{args:?} is not a propose invocation");
        };
        propose_verb(store, parsed, env.map(str::to_string))
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
    fn run_sources_propose_default_from_ctx_not_ambient_env() {
        // `run` must take propose's discovered-from default from the dispatch
        // context, never by reading VORO_TASK_ID itself — otherwise the test
        // suite becomes non-deterministic when run inside a dispatched session.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);

        let mut session_ctx = ctx();
        session_ctx.session_task_id = Some("1".into());
        run(
            &mut s,
            ["propose", "demo", "Follow-up"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            &session_ctx,
        )
        .unwrap();
        assert!(ok(&mut s, &["show", "2"]).contains("dep: discovered-from 1"));

        // With no session task in the context, propose links nothing — proving
        // the plumbing ignores whatever VORO_TASK_ID the environment carries.
        let mut none_ctx = ctx();
        none_ctx.session_task_id = None;
        run(
            &mut s,
            ["propose", "demo", "Orphan"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            &none_ctx,
        )
        .unwrap();
        assert!(s.deps_of(3).unwrap().is_empty());
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
        assert!(e.contains("cannot be used with"), "{e}");

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
        assert!(e.contains("cannot be used with"), "{e}");
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
        assert!(e.contains("cannot be used with"), "{e}");
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

    /// `done` on a stalled task reports the dead session's finished work on
    /// its behalf (DESIGN.md §6/§8): stalled → review, no manual `start`
    /// detour and no session reopened.
    #[test]
    fn done_on_a_stalled_task_reaches_review() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let (_, session) = s.record_dispatch(1, "claude", Some(1), None).unwrap();
        s.reconcile_session(session.id, false, false).unwrap();
        assert_eq!(s.task(1).unwrap().state, TaskState::Stalled);

        let out = ok(
            &mut s,
            &[
                "done",
                "1",
                "--summary",
                "finished; the report never landed",
            ],
        );
        assert!(out.contains("-> review"), "{out}");
        assert!(s.session(session.id).unwrap().ended_at.is_some());
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
    fn help_documents_both_blocks_directions() {
        let mut s = store();
        let out = ok(&mut s, &["help"]);
        assert!(out.contains("--blocked-by IDS"), "{out}");
        assert!(out.contains("--blocks IDS"), "{out}");
        assert!(out.contains("wait on"), "{out}");
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

    /// Pin the demo project's review action to `pr`, so these tests exercise
    /// the GitHub create path deterministically — under `auto` a non-GitHub
    /// temp path would resolve to the viewer medium instead (DESIGN.md §8).
    fn pin_pr_action(s: &mut Store) {
        ok(s, &["project", "action", "demo", "pr"]);
    }

    /// `pr` on a task that is not in `review` fails naming the state gap, before
    /// touching git or `gh` — the validation runs first.
    #[test]
    fn pr_create_requires_the_review_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        pin_pr_action(&mut s);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        let e = err(&mut s, &["pr", "1", "--yes"]);
        assert!(e.contains("review"), "{e}");
    }

    /// `pr` on a review task with no branch fails naming the branch gap.
    #[test]
    fn pr_create_requires_a_branch() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        pin_pr_action(&mut s);
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
        pin_pr_action(&mut s);
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

    // --- the folded review action (DESIGN.md §8/§11a) ---

    /// A DispatchCtx whose voro.toml is the given text, isolated under a temp
    /// root — the CLI-test face of the dispatch fixtures, for verbs that read
    /// viewers. The default `ctx()` points at the developer's real config,
    /// which these tests must not depend on (or launch viewers from).
    fn ctx_with_toml(toml: &str) -> DispatchCtx {
        let root = std::env::temp_dir().join(format!(
            "voro-cli-viewer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let agents_path = root.join("voro.toml");
        std::fs::write(&agents_path, toml).unwrap();
        DispatchCtx {
            db_path: root.join("voro.db"),
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            session_task_id: None,
        }
    }

    fn run_with(store: &mut Store, args: &[&str], ctx: &DispatchCtx) -> Result<String, String> {
        run(store, args.iter().map(|s| s.to_string()).collect(), ctx)
    }

    #[test]
    fn project_action_sets_shows_and_rejects() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let out = ok(&mut s, &["project", "action", "demo", "viewer:zed"]);
        assert!(out.contains("auto -> viewer:zed"), "{out}");
        assert!(
            ok(&mut s, &["project", "list"]).contains("[viewer:zed]"),
            "list must show a pinned action"
        );

        ok(&mut s, &["project", "action", "demo", "auto"]);
        assert!(
            !ok(&mut s, &["project", "list"]).contains("[viewer"),
            "auto is the default and earns no marker"
        );

        let e = err(&mut s, &["project", "action", "demo", "github"]);
        assert!(e.contains("auto, pr, viewer"), "{e}");
        assert!(ok(&mut s, &["help"]).contains("project action"), "help");
    }

    #[test]
    fn viewer_list_shows_viewers_flagging_the_default() {
        let mut s = store();
        let ctx = ctx_with_toml(
            "default_viewer = \"zed\"\n\n[viewers.zed]\ncmd = \"zed {path}\"\n\n\
             [viewers.difftool]\ncmd = \"git difftool -d\"\n",
        );
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(out.contains("* zed  zed {path}"), "{out}");
        assert!(out.contains("  difftool  git difftool -d"), "{out}");

        // the anonymous [viewer] table shows as the default it resolves to
        let ctx = ctx_with_toml("[viewer]\ncmd = \"zed {path}\"\n");
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(out.contains("* [viewer]  zed {path}"), "{out}");

        // nothing configured: say what to add rather than printing nothing
        let ctx = ctx_with_toml("");
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(out.contains("no viewers configured"), "{out}");
        assert!(ok(&mut s, &["help"]).contains("viewer list"), "help");
    }

    /// The fold itself: `pr` on a project whose review action names a viewer
    /// runs that viewer on the checkout — no branch, summary, or GitHub repo
    /// required — instead of erroring at the forge seam (DESIGN.md §8).
    #[test]
    fn pr_on_a_viewer_project_opens_the_viewer() {
        let mut s = store();
        let ctx = ctx_with_toml("[viewers.marker]\ncmd = \"touch {path}/opened.marker\"\n");
        let project_dir = ctx.db_path.parent().unwrap().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        ok(
            &mut s,
            &["project", "add", "demo", project_dir.to_str().unwrap()],
        );
        ok(&mut s, &["project", "action", "demo", "viewer:marker"]);
        // a review task with neither branch nor summary — the viewer medium
        // must not demand PR-readiness
        let id = review_task(&mut s, None, None);

        let out = run_with(&mut s, &["pr", &id.to_string()], &ctx).unwrap();
        assert!(out.contains("opened task"), "{out}");
        let marker = project_dir.join("opened.marker");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "pr must have run the project's viewer");
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
        assert!(e.contains("cannot be used with"), "{e}");
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
        // The note names the report, not a `pr` failure — on a viewer-medium
        // project `pr` opens the diff without either half.
        assert!(!out.contains("open a PR"), "{out}");
    }

    #[test]
    fn done_warning_promises_no_pr_failure_on_a_viewer_project() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["project", "action", "demo", "viewer:zed"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let out = ok(&mut s, &["done", "1", "--branch", "feat/x"]);
        assert!(out.contains("note: no summary recorded"), "{out}");
        assert!(!out.contains("open a PR"), "{out}");
        assert!(!out.contains("needs a branch and summary"), "{out}");
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
            ok(&mut s, &["show", "1"])
                .contains("incomplete report: only one of branch and summary recorded")
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
        assert!(e.contains("cannot be used with"), "{e}");
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
        assert!(err(&mut s, &["frobnicate"]).contains("unrecognized subcommand 'frobnicate'"));
        assert!(err(&mut s, &["weight", "nope", "3"]).contains("no project"));
        assert!(err(&mut s, &["start"]).contains("<TASK_ID>"));
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

    // --- unknown-flag rejection (task #108) ---

    /// A typo'd flag on a mutating verb is refused by name, before the verb
    /// runs: no transition, no summary, no event.
    #[test]
    fn an_unknown_flag_on_a_mutating_verb_writes_nothing() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        let events_before = s.events_for(1).unwrap().len();

        let e = err(&mut s, &["done", "1", "--sumary-file", "/tmp/x"]);
        assert!(e.contains("unexpected argument '--sumary-file'"), "{e}");
        assert!(
            e.contains("a similar argument exists: '--summary-file'"),
            "{e}"
        );

        assert!(ok(&mut s, &["show", "1"]).contains("#1 running"));
        assert_eq!(s.latest_summary(1).unwrap(), None);
        assert_eq!(s.events_for(1).unwrap().len(), events_before);
    }

    #[test]
    fn an_unknown_flag_on_a_read_verb_is_refused() {
        let mut s = store();
        let e = err(&mut s, &["list", "--stat", "ready"]);
        assert!(e.contains("unexpected argument '--stat'"), "{e}");
        assert!(e.contains("a similar argument exists: '--state'"), "{e}");

        // a flag valid on other verbs is still unknown on one without it
        let e = err(&mut s, &["inbox", "--state", "ready"]);
        assert!(e.contains("unexpected argument '--state'"), "{e}");
    }

    /// Boolean flags are covered too: a known boolean on the wrong verb, and a
    /// misspelled one, both fail as unknown — and the check runs before the
    /// handler, so the misplaced `--yes` typo never reaches the transition.
    #[test]
    fn boolean_flag_typos_are_refused() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "T", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);

        let e = err(&mut s, &["done", "1", "--from-pr"]);
        assert!(e.contains("unexpected argument '--from-pr'"), "{e}");

        let e = err(&mut s, &["accept", "1", "--ys", "now"]);
        assert!(e.contains("unexpected argument '--ys'"), "{e}");
        assert!(ok(&mut s, &["show", "1"]).contains("#1 running"));
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
            session_task_id: None,
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

    // --- resume: the operator answers in-session, Voro only signposts
    // (DESIGN.md §6/§8) ---

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
            session_task_id: None,
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

    /// `voro resume` on a dispatched task moves needs-input → running without
    /// spawning anything: the operator answered in the agent's own session, so
    /// the original session stays open and no new prompt is written.
    #[test]
    fn resuming_a_dispatched_task_moves_to_running_without_a_new_session() {
        let (mut store, ctx, project) = scratch_env("cat {prompt_file}");
        let id = dispatched_and_asked(&mut store, &ctx, &project);
        assert_eq!(store.sessions_for(id).unwrap().len(), 1);

        let out = run(&mut store, vec!["resume".into(), id.to_string()], &ctx).unwrap();
        assert_eq!(out, format!("task {id} -> running"));
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);

        // no continuation is spawned: still the single dispatch session and its
        // one prompt file, the exchange having happened in the live session.
        assert_eq!(store.sessions_for(id).unwrap().len(), 1, "no new session");
        assert_eq!(prompt_files(&ctx).len(), 1, "no new prompt written");

        let _ = std::fs::remove_dir_all(project.parent().unwrap());
    }

    /// `voro resume` on a task only ever started by hand — no dispatch, no
    /// session — is a plain transition.
    #[test]
    fn resuming_a_never_dispatched_task_is_a_plain_transition() {
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

        let out = run(&mut store, vec!["resume".into(), "1".into()], &ctx).unwrap();
        assert_eq!(out, "task 1 -> running");
        assert_eq!(store.task(1).unwrap().state, TaskState::Running);
        assert!(store.sessions_for(1).unwrap().is_empty());
    }
}
