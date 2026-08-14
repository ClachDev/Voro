//! The command-line verb surface: every TUI action, scriptable and
//! agent-legible (DESIGN.md §9). Parsing is clap derive, so a flag a verb does
//! not declare is rejected by name rather than silently swallowed. Help is the
//! one clap default overridden: every help request short-circuits to the
//! hand-written `HELP` overview.

use std::fmt::Write as _;

use clap::{Args, Parser, Subcommand, ValueEnum};

use voro_core::{
    Action, AgentsConfig, DepKind, Doc, Event, NewTask, NextAction, PrRef, Priority, Project,
    QueueRow, RefineOutcome, Repo, Store, Task, TaskEdit, TaskState, Triage, WipGate, scheduler,
};

use crate::app::viewer_label;
use crate::dispatch::{self, DispatchCtx};
use crate::import;
use crate::pr::ForgeMemo;

const HELP: &str = "\
voro — prioritised attention across projects

usage: voro [--db PATH]                 launch the TUI
       voro [--db PATH] <verb> [args]

projects
  project add <name> <path>       create a project (weight 3)
  project list                    list projects with weights
  project rename <project> <name> rename a project (tasks reference it by id)
  project path <project> <path>   change a project's default repo's path
  project archive <project>       retire a project: hide it and all its tasks
                                  from the cockpit (queue, stats, running
                                  strip); tasks freeze in their states, and
                                  `project list` keeps it, tagged [archived]
  project unarchive <project>     restore an archived project and its tasks
                                  exactly as they were
  project delete <project>        delete a project with no tasks — park it
                                  (weight 0) or archive it instead to retire
                                  one that has any
  project viewer <project> [NAME] set which viewer `open` shows the project's
                                  local diffs in: NAME picks a [viewers.NAME]
                                  entry from voro.toml, and naming none leaves
                                  the default viewer (`pr` is always GitHub, so
                                  the medium is not a project setting)
  weight <project> <0-5>          set a project's weight (0 parks it)

repos                             a project allocates attention; its repos
                                  locate the checkouts its tasks run in
  repo add <project> <name> <path>
                                  add a checkout to a project
  repo list [project]             list repos; * marks each project's default
  repo path <project> <name> <path>
                                  re-point a repo's checkout
  repo default <project> <name>   make a repo the project's default — where
                                  tasks that name no repo run
  repo remove <project> <name>    remove a repo; refused for the last one, the
                                  default while others remain, and one any
                                  task still names

documents                         the plan or design doc a body of work derives
                                  from, linked to the tasks it spawned so the
                                  derivation is queryable and reaches the agent
  doc add <project> <path-or-url> [--title T] [--repo NAME]
                                  register a document against a project; an
                                  absolute path inside one of its checkouts is
                                  stored relative to that checkout, so the link
                                  survives the checkout moving. --repo picks
                                  which checkout a relative path is read from
  doc list [project]              list documents with how many tasks cite each
  doc show <doc>                  a document and every task linked to it, with
                                  states — the progress view for one plan
  doc link <doc> <task-ids>       link tasks to a document; a task in any
                                  project may cite it, since one plan routinely
                                  spawns work across several
  doc unlink <doc> <task-ids>     drop those links
  doc remove <doc>                unregister a document, unlinking its tasks
                                  <doc> is a document id or its exact location

tasks
  add <project> <title> [--body TEXT | --body-file PATH] [--priority 0-3]
      [--state proposed|parked|ready] [--agent NAME] [--blocked-by IDS]
      [--blocks IDS] [--human] [--deep] [--repo NAME] [--doc DOCS]
                                  --repo names which of the project's repos
                                  the task runs in; omitted, it runs in the
                                  project's default repo
                                  --doc links the task to registered documents
                                  (ids or locations, comma-separated), which
                                  dispatch then names in the agent's prompt
                                  --blocked-by lists the tasks this one waits
                                  on; --blocks makes the listed tasks wait on
                                  this one
                                  --human marks a task no agent can execute:
                                  never dispatched, worked by hand, and its
                                  completion goes straight to done
                                  --deep dispatches on the strongest model the
                                  agent offers rather than its workhorse;
                                  agents that name no models ignore it
  propose <project> <title> [--body TEXT | --body-file PATH] [--from TASK-ID]
                                  create a proposed task; --from links it
                                  discovered-from that task (dispatch renders
                                  the flag with the running task's id)
  set <task-id> [--title T] [--priority 0-3] [--agent NAME | --no-agent]
      [--body TEXT | --body-file PATH] [--append-body TEXT | --append-body-file PATH]
      [--allow-empty] [--blocked-by IDS] [--blocks IDS] [--unlink KIND:ID]
      [--pr URL | --no-pr] [--branch NAME | --no-branch]
      [--human | --no-human] [--deep | --no-deep]
      [--summary TEXT | --summary-file PATH]
      [--repo NAME | --no-repo] [--doc DOCS | --no-doc]
                                  --body replaces the whole body; --append-body
                                  adds to what is there, after a blank line.
                                  A replacement that would leave the body empty
                                  is refused unless --allow-empty; either way
                                  the replaced text is kept on the event log
                                  (`show` names the event to recover it from).
                                  On a refining task a replacement ends the
                                  round: the task returns to proposed, marked
                                  ↻ refined for a fresh verdict
                                  --blocked-by replaces this task's own
                                  blocker list; --blocks adds this task as a
                                  blocker of each listed task
                                  --unlink drops one dependency edge of this
                                  task, named as `show` lists it —
                                  blocks:9, discovered-from:4, parent:2,
                                  related:7 — leaving any other edge to the
                                  same task standing; repeat it for several
                                  --pr tracks a GitHub PR (URL or owner/repo#N)
                                  for review; --no-pr clears it. --branch sets
                                  the git branch dispatch injects into the
                                  prompt; --no-branch clears it. --summary
                                  sets or replaces a running/review task's
                                  completion summary — the PR description `pr`
                                  opens the pull request from, so write it as
                                  one — without a reject/done round trip.
                                  --repo re-points the task at another of its
                                  project's repos; --no-repo returns it to the
                                  project default
                                  --deep dispatches on the agent's strongest
                                  model; --no-deep returns it to the workhorse
                                  --doc replaces the task's whole document list
                                  (`voro doc link` adds one without listing the
                                  rest); --no-doc clears it
  show <task-id> [--event EVENT-ID]
                                  full task: body, docs, deps, events. --event
                                  prints one event's recorded detail and nothing
                                  else, which is how a replaced body comes back:
                                  `voro show 62 --event 512 > body.md`
  list [--state STATE] [--project P] [--doc DOC]
                                  --doc answers 'which tasks derive from this
                                  plan?'
  inbox                           the next-action queue: questions, reviews,
                                  proposals, top ready tasks — ranked by score
                                  divided by what each action costs your
                                  attention. Proposals ride as one digest row
                                  per project; dispatch rows give way to a
                                  capacity line once max_running are in flight
  next                            the single highest-scoring ready task
  stats                           task counts by state — the triage backlog
                                  (§12) plus ready, running, needs-input,
                                  review, waiting, stalled, done; excludes
                                  parked projects
  explain <task-id>               score decomposition, and the divisor the
                                  inbox ranks it by
  seed [--force]                  fill the dev store with fixture data — a
                                  board covering every task state. A build run
                                  from a target/ directory seeds it by itself
                                  on first run; --force rebuilds it. Refused
                                  against your real store
  migrate [--yes]                 apply pending schema migrations to your real
                                  store, the one store that never migrates as
                                  a side effect of being opened (the TUI asks
                                  the same question at launch). Asks at a
                                  terminal; --yes consents from a script and
                                  is recorded in the migration journal. Dev
                                  and scratch stores migrate on open and need
                                  none of this
  import <project> [--repo NAME] [--gh-repo owner/name]
                                  import open GitHub issues as proposed
                                  tasks via `gh issue list`; idempotent.
                                  --repo picks which of the project's repos to
                                  import from (default: the default repo), and
                                  imported tasks run in it; --gh-repo overrides
                                  that checkout's own remote

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
  viewer list                     list effective viewers (built-in + user)
                                  with provenance; * marks the default used
                                  when nothing names one
  viewer add <name> [cmd]         define a [viewers.NAME] entry in voro.toml
                                  (comment-preserving). With no cmd it runs
                                  '<name> {path}' — the built-in's own line if
                                  NAME is one, which is how you override it.
                                  A cmd may carry {path}, {branch}, {base}
                                  (e.g. 'code -n {path}')
  viewer remove <name>            delete a viewer; refused while a project
                                  still names it, and for a built-in, which is
                                  overridden rather than removed
  open <task-id>                  open a review/running task's checkout in a
                                  viewer to see its diff — the only local-diff
                                  spelling, and the one `project viewer` names
                                  a viewer for. Uses the built-in code/cursor/
                                  zed found on PATH when voro.toml names none
  pr <task-id> [--yes]            show the task's diff on GitHub, always: jump
                                  to the tracked PR in a browser, or push the
                                  review task's branch and open a ready PR
                                  from its summary, recording the URL (--yes
                                  skips the confirm; track an existing PR with
                                  `set --pr`). A checkout GitHub cannot take
                                  errors pointing at `open`

transitions
  triage <task-id> <parked|ready|reject|refine>
                                  the three verdicts move the proposal; refine
                                  is not a verdict — it dispatches an agent to
                                  rewrite the body against --note TEXT (or
                                  --note-file PATH), moving the task proposed or
                                  ready → refining until the round concludes. It
                                  returns to proposed marked ↻ refined for a
                                  re-triage of the improved version, or ⚠ refine
                                  failed if the agent died having written
                                  nothing. A task refined out of ready comes back
                                  through triage because the verdict it carried
                                  was issued against the replaced body. A verdict
                                  on a refining task is refused: it is out of the
                                  queue while the rewrite is in flight. The
                                  note-less interactive variant is a conversation
                                  with the agent, so it lives in the TUI, on `R`
                                  over the row
  start <task-id>                 ready → running
  ask <task-id> --question TEXT   running → needs-input
  resume <task-id>                needs-input → running, once you have answered
                                  the question in the agent's own session
  done <task-id> [--summary TEXT | --summary-file PATH] [--branch NAME]
                                  running | stalled → review (from stalled:
                                  reporting a dead session's finished work on
                                  its behalf); the summary is the agent's
                                  completion report, kept as a summary event
                                  and used as the body `pr` opens the pull
                                  request with — so write it as a PR
                                  description, what changed and why then how
                                  you verified it, not a status line; --branch
                                  records the git branch the work landed on
                                  (agent return path). Warns but succeeds when
                                  branch or summary is absent
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
    Repo {
        #[command(subcommand)]
        cmd: RepoCmd,
    },
    Doc {
        #[command(subcommand)]
        cmd: DocCmd,
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
        #[arg(long, value_name = "EVENT-ID")]
        event: Option<i64>,
    },
    List(ListArgs),
    Inbox,
    Next,
    Stats,
    /// Fill the dev store with fixture data.
    Seed {
        /// Discard what is there and rebuild it.
        #[arg(long)]
        force: bool,
    },
    /// Apply pending migrations to a protected store (DESIGN.md §5). The
    /// pending case never reaches this handler — the store refuses to open, and
    /// `main` collects consent before there is a `Store` to pass — so what is
    /// left to report here is that nothing was pending.
    Migrate {
        /// Consent from a script; recorded in the migration journal.
        #[arg(long)]
        yes: bool,
    },
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
    Triage(TriageArgs),
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
    Add {
        name: String,
        path: String,
    },
    List,
    Rename {
        project: String,
        name: String,
    },
    Path {
        project: String,
        path: String,
    },
    Archive {
        project: String,
    },
    Unarchive {
        project: String,
    },
    Delete {
        project: String,
    },
    Viewer {
        project: String,
        name: Option<String>,
    },
}

/// The checkouts under a project (DESIGN.md §3/§5). A project always has at
/// least one — `project add` makes it — so these verbs manage the rest.
#[derive(Subcommand)]
enum RepoCmd {
    Add {
        project: String,
        name: String,
        path: String,
    },
    List {
        project: Option<String>,
    },
    Path {
        project: String,
        name: String,
        path: String,
    },
    Default {
        project: String,
        name: String,
    },
    Remove {
        project: String,
        name: String,
    },
}

/// The plan and design documents a project's work derives from (DESIGN.md
/// §3/§5). `<doc>` throughout is a document id or its exact stored location.
#[derive(Subcommand)]
enum DocCmd {
    Add {
        project: String,
        location: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        repo: Option<String>,
    },
    List {
        project: Option<String>,
    },
    Show {
        doc: String,
    },
    Link {
        doc: String,
        #[arg(value_name = "TASK-IDS", required = true)]
        task_ids: Vec<String>,
    },
    Unlink {
        doc: String,
        #[arg(value_name = "TASK-IDS", required = true)]
        task_ids: Vec<String>,
    },
    Remove {
        doc: String,
    },
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
    /// A command is optional: with none, the viewer runs
    /// `voro_core::config_edit::assumed_viewer_cmd` — the built-in's own line
    /// when the name is one, else `<name> {path}`.
    Add {
        name: String,
        cmd: Option<String>,
    },
    Remove {
        name: String,
    },
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
    #[arg(long)]
    deep: bool,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    doc: Option<String>,
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
    #[arg(long, conflicts_with_all = ["body", "body_file"])]
    append_body: Option<String>,
    #[arg(long, conflicts_with_all = ["body", "body_file", "append_body"])]
    append_body_file: Option<String>,
    #[arg(long)]
    allow_empty: bool,
    #[arg(long)]
    blocked_by: Option<String>,
    #[arg(long)]
    blocks: Option<String>,
    #[arg(long, value_name = "KIND:ID")]
    unlink: Vec<String>,
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
    deep: bool,
    #[arg(long, conflicts_with = "deep")]
    no_deep: bool,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long, conflicts_with = "summary")]
    summary_file: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long, conflicts_with = "repo")]
    no_repo: bool,
    #[arg(long)]
    doc: Option<String>,
    #[arg(long, conflicts_with = "doc")]
    no_doc: bool,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)]
    state: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long)]
    doc: Option<String>,
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
    /// Which of the project's repos to import from — a Voro repo name, the
    /// same noun `add --repo` takes. The GitHub-side override is `--gh-repo`.
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    gh_repo: Option<String>,
}

#[derive(Args)]
struct AskArgs {
    task_id: i64,
    #[arg(value_name = "TEXT")]
    text: Vec<String>,
    #[arg(long)]
    question: Option<String>,
}

#[derive(Args)]
struct TriageArgs {
    task_id: i64,
    target: TriageTarget,
    /// The one-line brief a `refine` hands its agent: what is wrong with the
    /// body as it stands. Required for `refine` — the interactive variant,
    /// which needs no note, is TUI-only (DESIGN.md §6).
    #[arg(long)]
    note: Option<String>,
    #[arg(long, conflicts_with = "note")]
    note_file: Option<String>,
}

/// The triage outcomes. Three are verdicts that transition the task; `refine`
/// is not — it dispatches an agent to rewrite the body and leaves the task
/// `proposed` for a re-triage of the improved version (DESIGN.md §6).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum TriageTarget {
    Parked,
    Ready,
    Reject,
    Refine,
}

impl TryFrom<TriageTarget> for Triage {
    type Error = ();

    fn try_from(target: TriageTarget) -> Result<Triage, ()> {
        match target {
            TriageTarget::Parked => Ok(Triage::Parked),
            TriageTarget::Ready => Ok(Triage::Ready),
            TriageTarget::Reject => Ok(Triage::Reject),
            TriageTarget::Refine => Err(()),
        }
    }
}

pub fn run(store: &mut Store, args: Vec<String>, ctx: &DispatchCtx) -> Result<String, String> {
    // Reconcile-on-read (DESIGN.md §8): before any verb consults session or
    // task state, close out sessions whose process has already exited.
    crate::reconcile::reconcile_live_sessions(store, ctx).map_err(|e| e.to_string())?;

    // Every help request gets the hand-written overview page; clap's own help
    // machinery is disabled so it can never shadow this.
    if args.is_empty() || args[0] == "help" || args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(HELP.to_string());
    }
    let cli = Cli::try_parse_from(std::iter::once("voro".to_string()).chain(args))
        .map_err(|e| e.to_string().trim_end().to_string())?;
    match cli.verb {
        Verb::Project { cmd } => project_verb(store, cmd),
        Verb::Repo { cmd } => repo_verb(store, cmd),
        Verb::Doc { cmd } => doc_verb(store, cmd),
        Verb::Weight { project, weight } => weight_verb(store, &project, weight),
        Verb::Add(args) => add_verb(store, args),
        Verb::Propose(args) => propose_verb(store, args),
        Verb::Set(args) => set_verb(store, args),
        Verb::Show { task_id, event } => match event {
            Some(event_id) => show_event(store, task_id, event_id),
            None => show_verb(store, task_id),
        },
        Verb::List(args) => list_verb(store, &args),
        Verb::Inbox => inbox_verb(store, ctx),
        Verb::Next => next_verb(store),
        Verb::Stats => stats_verb(store),
        Verb::Seed { force } => seed_verb(store, ctx, force),
        Verb::Migrate { .. } => migrate_verb(store, ctx),
        Verb::Explain { task_id } => explain_verb(store, task_id, ctx),
        Verb::Agent { cmd } => agent_verb(cmd, ctx),
        Verb::Dispatch { task_id, agent } => {
            dispatch::dispatch(store, ctx, task_id, agent.as_deref())
        }
        Verb::Open { task_id } => {
            dispatch::open(store, ctx, task_id, None).map_err(|e| e.to_string())
        }
        Verb::Viewer { cmd } => viewer_verb(store, cmd, ctx),
        Verb::Pr { task_id, yes } => pr_verb(store, task_id, yes),
        Verb::Reject(args) => reject_verb(store, args),
        Verb::Done(args) => done_verb(store, args),
        Verb::Import(args) => import_verb(store, args),
        Verb::Triage(args) => triage_verb(store, args, ctx),
        Verb::Start { task_id } => apply_action(store, ctx, task_id, Action::Start, false),
        Verb::Ask(args) => ask_verb(store, ctx, args),
        Verb::Resume { task_id } => apply_action(store, ctx, task_id, Action::Resume, false),
        Verb::Accept { task_id, yes } => apply_action(store, ctx, task_id, Action::Accept, yes),
        Verb::Wait { task_id } => apply_action(store, ctx, task_id, Action::HandOff, false),
        Verb::Reclaim { task_id } => apply_action(store, ctx, task_id, Action::Reclaim, false),
        Verb::Abort { task_id } => apply_action(store, ctx, task_id, Action::Abort, false),
        Verb::Park { task_id } => apply_action(store, ctx, task_id, Action::Park, false),
        Verb::Unpark { task_id } => apply_action(store, ctx, task_id, Action::Unpark, false),
        Verb::Abandon { task_id, yes } => apply_action(store, ctx, task_id, Action::Abandon, yes),
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

/// One `--unlink KIND:ID` argument: the kind and the task at the far end of
/// the edge, in the direction `show` prints — the edge belongs to the task
/// being edited.
fn parse_edge(raw: &str) -> Result<(DepKind, i64), String> {
    let (kind, id) = raw
        .rsplit_once(':')
        .ok_or_else(|| format!("unlink must be KIND:ID, got '{raw}'"))?;
    let kind = DepKind::parse(kind.trim()).map_err(|e| e.to_string())?;
    let id = id
        .trim()
        .parse()
        .map_err(|_| format!("unlink must be KIND:ID, got '{raw}'"))?;
    Ok((kind, id))
}

/// Apply `--unlink KIND:ID`: drop exactly the named edges of `task_id`, and
/// echo each the way `show` reads it, so a `blocks` edge is never reported
/// backwards. Removing a blocker reconciles readiness in the store, and the
/// promotion it can produce is echoed for the same reason `--blocks` echoes
/// its demotion — the graph edit's effect on the queue is the point of it.
fn apply_unlink_flag(store: &mut Store, task_id: i64, specs: &[String]) -> Result<String, String> {
    let mut out = String::new();
    for spec in specs {
        let (kind, other) = parse_edge(spec)?;
        let before = store.task(task_id).map_err(|e| e.to_string())?.state;
        store
            .remove_dep(task_id, other, kind)
            .map_err(|e| e.to_string())?;
        match kind {
            DepKind::Blocks => {
                write!(out, "\ntask {task_id} no longer blocked by #{other}").unwrap()
            }
            _ => write!(out, "\ntask {task_id} no longer {kind} #{other}").unwrap(),
        }
        let after = store.task(task_id).map_err(|e| e.to_string())?.state;
        if before == TaskState::Parked && after == TaskState::Ready {
            write!(out, " — #{task_id} promoted to ready").unwrap();
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

/// The body a `set` lands on (DESIGN.md §8). `--body`/`--body-file` replace it
/// wholesale; `--append-body`/`--append-body-file` add to what is already there
/// after a blank line, which is the "record a finding on the task" case that
/// otherwise gets spelled as a replacement and loses the brief. A replacement
/// that would leave a non-empty body empty is refused unless `--allow-empty`,
/// since nothing legitimate reads as "blank the brief" and the commonest way to
/// arrive at one is a slip. Emptying an already-empty body destroys nothing and
/// is left alone.
fn set_body(current: &str, args: &mut SetArgs, id: i64) -> Result<String, String> {
    if let Some(added) = text_or_file(args.append_body.take(), args.append_body_file.take())? {
        return Ok(appended_body(current, &added));
    }
    let Some(replacement) = text_or_file(args.body.take(), args.body_file.take())? else {
        return Ok(current.to_string());
    };
    if replacement.trim().is_empty() && !current.trim().is_empty() && !args.allow_empty {
        return Err(format!(
            "refusing to empty the body of task {id} ({} lines) — pass --allow-empty if you mean it",
            current.lines().count()
        ));
    }
    Ok(replacement)
}

/// An addition to an existing body, separated from it by one blank line. An
/// empty body takes the addition as-is, so the first append reads like a write.
fn appended_body(current: &str, added: &str) -> String {
    let base = current.trim_end();
    if base.is_empty() {
        return added.to_string();
    }
    format!("{base}\n\n{}", added.trim_start_matches('\n'))
}

/// How an event's recorded detail reads in a history listing. Everything is its
/// own detail except a `body` event, whose detail is the whole text a body edit
/// replaced (DESIGN.md §8) — bulk kept for recovery, not for reading, so the
/// line says what is there and how to get it back instead of unrolling it.
pub(crate) fn event_detail(event: &Event) -> String {
    let detail = event.detail.clone().unwrap_or_default();
    if event.kind != "body" {
        return detail;
    }
    let lines = detail.lines().count();
    match event.task_id {
        Some(task_id) => format!(
            "replaced body kept ({lines} lines) — voro show {task_id} --event {}",
            event.id
        ),
        None => format!("replaced body kept ({lines} lines)"),
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
                let viewer = match &p.viewer {
                    Some(name) => format!("  [viewer:{name}]"),
                    None => String::new(),
                };
                let archived = if p.archived { "  [archived]" } else { "" };
                // The path column stays the default repo's, so a single-repo
                // project reads exactly as it did; extras are tagged.
                let repos = store.repos(p.id).map_err(|e| e.to_string())?;
                let path = repos
                    .iter()
                    .find(|r| r.is_default)
                    .map(|r| r.path.as_str())
                    .unwrap_or_default();
                let extra = match repos.len() {
                    0 | 1 => String::new(),
                    n => format!("  [+{} repo(s)]", n - 1),
                };
                writeln!(
                    out,
                    "{:3}  w{}  {}  {}{extra}{viewer}{archived}",
                    p.id, p.weight, p.name, path
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
            let repo = store
                .set_default_repo_path(project.id, &path)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "project {} default repo '{}' path -> {}",
                project.id, repo.name, repo.path
            ))
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
        ProjectCmd::Viewer { project, name } => {
            let project = resolve_project(store, &project)?;
            let p = store
                .set_viewer(project.id, name.as_deref())
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{} viewer: {} -> {}",
                p.name,
                viewer_label(project.viewer.as_deref()),
                viewer_label(p.viewer.as_deref())
            ))
        }
    }
}

/// The `repo` verbs (DESIGN.md §3/§5). Every refusal — last repo, default
/// repo, referenced repo — comes from the store API, so the CLI only names
/// which repo is meant and relays the store's answer.
fn repo_verb(store: &mut Store, cmd: RepoCmd) -> Result<String, String> {
    match cmd {
        RepoCmd::Add {
            project,
            name,
            path,
        } => {
            let project = resolve_project(store, &project)?;
            let repo = store
                .add_repo(project.id, &name, &path)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "repo '{}' added to {} at {} — dispatch a task there with \
                 `voro add {} <title> --repo {}`",
                repo.name, project.name, repo.path, project.name, repo.name
            ))
        }
        RepoCmd::List { project } => {
            let projects = match project {
                Some(key) => vec![resolve_project(store, &key)?],
                None => store.projects().map_err(|e| e.to_string())?,
            };
            let mut out = String::new();
            for project in projects {
                for repo in store.repos(project.id).map_err(|e| e.to_string())? {
                    let marker = if repo.is_default { "* " } else { "  " };
                    writeln!(
                        out,
                        "{marker}{:14} {:14} {}",
                        project.name, repo.name, repo.path
                    )
                    .unwrap();
                }
            }
            if !out.is_empty() {
                writeln!(out, "\n(* is each project's default repo)").unwrap();
            }
            Ok(out)
        }
        RepoCmd::Path {
            project,
            name,
            path,
        } => {
            let project = resolve_project(store, &project)?;
            let repo = resolve_repo(store, &project, &name)?;
            let repo = store
                .set_repo_path(repo.id, &path)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{} repo '{}' path -> {}",
                project.name, repo.name, repo.path
            ))
        }
        RepoCmd::Default { project, name } => {
            let project = resolve_project(store, &project)?;
            let repo = resolve_repo(store, &project, &name)?;
            let repo = store.set_default_repo(repo.id).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} default repo -> '{}' ({}) — tasks that name no repo now run there",
                project.name, repo.name, repo.path
            ))
        }
        RepoCmd::Remove { project, name } => {
            let project = resolve_project(store, &project)?;
            let repo = resolve_repo(store, &project, &name)?;
            store.delete_repo(repo.id).map_err(|e| e.to_string())?;
            Ok(format!("{} repo '{}' removed", project.name, repo.name))
        }
    }
}

/// Name one of a project's repos. An unknown name errors listing the project's
/// repos, so a filing agent gets a correction rather than a wrong checkout.
fn resolve_repo(store: &Store, project: &Project, name: &str) -> Result<Repo, String> {
    store
        .repo_by_name(project.id, name)
        .map_err(|e| e.to_string())
}

/// The `doc` verbs (DESIGN.md §3/§5): register the plan a body of work derives
/// from, and link it to the tasks it spawned. Every refusal — an unknown repo,
/// a path outside the checkout it names, a duplicate registration — comes from
/// the store API; this only resolves which project, repo, doc, and tasks are
/// meant.
fn doc_verb(store: &mut Store, cmd: DocCmd) -> Result<String, String> {
    match cmd {
        DocCmd::Add {
            project,
            location,
            title,
            repo,
        } => {
            let project = resolve_project(store, &project)?;
            let repo = match &repo {
                Some(name) => Some(resolve_repo(store, &project, name)?),
                None => None,
            };
            let doc = store
                .create_doc(
                    project.id,
                    repo.as_ref().map(|r| r.id),
                    &location,
                    title.as_deref(),
                )
                .map_err(|e| e.to_string())?;
            let mut out = format!(
                "doc {} '{}' registered against {}",
                doc.id,
                doc.label(),
                project.name
            );
            // The stored location is often not the one typed — an absolute path
            // inside a checkout is relativised — so echo what will be resolved.
            if doc.location != location.trim() {
                write!(out, " as {}", doc.location).unwrap();
            }
            write!(
                out,
                "\nlink tasks to it with `voro doc link {} <task-ids>`",
                doc.id
            )
            .unwrap();
            Ok(out)
        }
        DocCmd::List { project } => {
            let projects = match project {
                Some(key) => vec![resolve_project(store, &key)?],
                None => store.projects().map_err(|e| e.to_string())?,
            };
            let mut out = String::new();
            for project in projects {
                for doc in store.docs(project.id).map_err(|e| e.to_string())? {
                    let linked = store
                        .tasks_for_doc(doc.id)
                        .map_err(|e| e.to_string())?
                        .len();
                    let title = match &doc.title {
                        Some(title) => format!("  {title}"),
                        None => String::new(),
                    };
                    writeln!(
                        out,
                        "{:3}  {:14} {}{title}  [{linked} task(s)]",
                        doc.id, project.name, doc.location
                    )
                    .unwrap();
                }
            }
            Ok(out)
        }
        DocCmd::Show { doc } => doc_show(store, &doc),
        DocCmd::Link { doc, task_ids } => {
            let doc = resolve_doc(store, &doc)?;
            let mut out = String::new();
            for id in parse_ids("doc link", &task_ids.join(","))? {
                let linked = store.link_doc(id, doc.id).map_err(|e| e.to_string())?;
                let verb = if linked { "linked" } else { "already linked" };
                writeln!(out, "#{id} {verb} to doc {} '{}'", doc.id, doc.label()).unwrap();
            }
            Ok(out)
        }
        DocCmd::Unlink { doc, task_ids } => {
            let doc = resolve_doc(store, &doc)?;
            let mut out = String::new();
            for id in parse_ids("doc unlink", &task_ids.join(","))? {
                let unlinked = store.unlink_doc(id, doc.id).map_err(|e| e.to_string())?;
                let verb = if unlinked {
                    "unlinked from"
                } else {
                    "not linked to"
                };
                writeln!(out, "#{id} {verb} doc {} '{}'", doc.id, doc.label()).unwrap();
            }
            Ok(out)
        }
        DocCmd::Remove { doc } => {
            let doc = resolve_doc(store, &doc)?;
            let freed = store.delete_doc(doc.id).map_err(|e| e.to_string())?;
            let mut out = format!("doc {} '{}' removed", doc.id, doc.label());
            if !freed.is_empty() {
                write!(out, " — unlinked from {} task(s)", freed.len()).unwrap();
            }
            Ok(out)
        }
    }
}

/// `doc show <doc>`: the document, where it resolves to, and the rollup of
/// every task that cites it with its state — the per-plan progress view.
fn doc_show(store: &mut Store, key: &str) -> Result<String, String> {
    let doc = resolve_doc(store, key)?;
    let project = store.project(doc.project_id).map_err(|e| e.to_string())?;
    let mut out = format!("doc {}  {}\n", doc.id, doc.label());
    writeln!(out, "project: {}", project.name).unwrap();
    writeln!(out, "location: {}", doc.location).unwrap();
    let resolved = store.resolve_doc(&doc).map_err(|e| e.to_string())?;
    if resolved != doc.location {
        writeln!(out, "resolves to: {resolved}").unwrap();
    }
    writeln!(out, "registered {}", doc.created_at).unwrap();

    let tasks = store.tasks_for_doc(doc.id).map_err(|e| e.to_string())?;
    let projects = store.projects().map_err(|e| e.to_string())?;
    if tasks.is_empty() {
        writeln!(
            out,
            "\nno tasks linked yet — `voro doc link {} <task-ids>`",
            doc.id
        )
        .unwrap();
        return Ok(out);
    }
    writeln!(out, "\ntasks ({}):", tasks.len()).unwrap();
    for task in &tasks {
        let name = projects
            .iter()
            .find(|p| p.id == task.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        writeln!(out, "  {}", task_line(task, name)).unwrap();
    }
    // The rollup the plan is actually read for: how far its work has got.
    let mut counts: Vec<(TaskState, usize)> = Vec::new();
    for task in &tasks {
        match counts.iter_mut().find(|(s, _)| *s == task.state) {
            Some((_, n)) => *n += 1,
            None => counts.push((task.state, 1)),
        }
    }
    counts.sort_by_key(|(s, _)| s.as_str());
    let rollup: Vec<String> = counts
        .into_iter()
        .map(|(state, n)| format!("{n} {state}"))
        .collect();
    writeln!(out, "\n{}", rollup.join(" · ")).unwrap();
    Ok(out)
}

/// Name a registered document by id or by its exact stored location. A location
/// shared by two projects' registrations is an ambiguity the operator resolves
/// by id rather than one this picks a winner for.
fn resolve_doc(store: &Store, key: &str) -> Result<Doc, String> {
    if let Ok(id) = key.parse::<i64>() {
        return store.doc(id).map_err(|e| e.to_string());
    }
    let matches = store.docs_at(key).map_err(|e| e.to_string())?;
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one match")),
        0 => Err(format!(
            "no document at '{key}' — register one with `voro doc add <project> {key}`, or name \
             an id from `voro doc list`"
        )),
        _ => Err(format!(
            "'{key}' is registered by more than one project — name it by id ({})",
            matches
                .iter()
                .map(|d| d.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Resolve a `--doc` flag's comma- or space-separated list of ids and
/// locations into concrete documents, in the order given.
fn resolve_docs(store: &Store, raw: &str) -> Result<Vec<Doc>, String> {
    raw.split([',', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|key| resolve_doc(store, key))
        .collect()
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
                let verbs = template.verbs();
                let suffix = if verbs.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", verbs.join(" "))
                };
                // What `{model}` resolves to, so the template's placeholder is
                // readable without opening the config; the fallbacks are
                // spelled out rather than left implicit.
                let models = match template.model() {
                    None => String::new(),
                    Some(model) => {
                        let deep = template.model_deep().unwrap_or(model);
                        let plan = template.model_plan().unwrap_or(model);
                        format!("  <model {model} · deep {deep} · plan {plan}>")
                    }
                };
                writeln!(
                    out,
                    "{marker}{name}  {}{suffix}{models}  ({})",
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
    let repo = match &args.repo {
        Some(name) => Some(resolve_repo(store, &project, name)?),
        None => None,
    };
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
            repo_id: repo.as_ref().map(|r| r.id),
            title,
            body: text_or_file(args.body, args.body_file)?.unwrap_or_default(),
            priority: args.priority.unwrap_or(Priority::P2),
            state,
            agent: args.agent,
            human: args.human,
            deep: args.deep,
        })
        .map_err(|e| e.to_string())?;
    let task = match &args.blocked_by {
        Some(raw) => store
            .set_blocks_deps(task.id, &parse_ids("blocked-by", raw)?)
            .map_err(|e| e.to_string())?,
        None => task,
    };
    let mut out = format!("task {} '{}' created ({})", task.id, task.title, task.state);
    if let Some(repo) = &repo {
        out.push_str(&format!(" in repo '{}'", repo.name));
    }
    if let Some(raw) = &args.doc {
        let docs = resolve_docs(store, raw)?;
        for doc in &docs {
            store.link_doc(task.id, doc.id).map_err(|e| e.to_string())?;
        }
        write!(out, "\n{}", doc_link_echo(&docs)).unwrap();
    }
    if let Some(raw) = &args.blocks {
        out.push_str(&apply_blocks_flag(store, task.id, raw)?);
    }
    Ok(out)
}

/// The agent return-path form of `add` (DESIGN.md §8): always lands in
/// `proposed`, and links the new task discovered-from the `--from` task when
/// one is given. The source is only ever the explicit flag — `run` reads no
/// ambient `VORO_TASK_ID`; a dispatched session gets the flag rendered into its
/// preamble with the running task's id instead.
fn propose_verb(store: &mut Store, args: ProposeArgs) -> Result<String, String> {
    if args.state.is_some() {
        return Err("propose always creates 'proposed' tasks — use 'add --state' instead".into());
    }
    let project = resolve_project(store, &args.project)?;
    let title = joined(&args.title, "title")?;
    let source = match args.from {
        Some(id) => Some(store.task(id).map_err(|e| e.to_string())?),
        None => None,
    };
    let task = store
        .create_task(NewTask {
            project_id: project.id,
            repo_id: None,
            title,
            body: text_or_file(args.body, args.body_file)?.unwrap_or_default(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
            deep: false,
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

fn set_verb(store: &mut Store, mut args: SetArgs) -> Result<String, String> {
    let id = args.task_id;
    let current = store.task(id).map_err(|e| e.to_string())?;
    // A refine round ends when the rewritten body lands (DESIGN.md §6), which is
    // the verb the refine prompts already end with — so a `set` that *replaces*
    // the body concludes the round. An append, or a `set` touching anything else,
    // leaves the round running.
    let replaces_body = args.body.is_some() || args.body_file.is_some();
    let concludes_refine = replaces_body && current.state == TaskState::Refining;
    // The backstop for a rewrite that arrives after its round was already
    // concluded failed (DESIGN.md §6): the body lands on a `proposed` task, too
    // late to conclude anything, so the recorded outcome is corrected instead.
    let corrects_late_refine = replaces_body && current.state == TaskState::Proposed;
    let body = set_body(&current.body, &mut args, id)?;
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
    let deep = match (args.deep, args.no_deep) {
        (true, _) => true,
        (_, true) => false,
        (false, false) => current.deep,
    };
    let edit = TaskEdit {
        title: args.title.unwrap_or(current.title),
        body,
        priority: args.priority.unwrap_or(current.priority),
        agent,
        human,
        deep,
    };
    let task = store.update_task(id, edit).map_err(|e| e.to_string())?;
    let task = if concludes_refine {
        store
            .conclude_refine(id, RefineOutcome::Applied)
            .map_err(|e| e.to_string())?
    } else {
        task
    };
    let refine_echo =
        if corrects_late_refine && store.correct_late_refine(id).map_err(|e| e.to_string())? {
            "\nthe failed refine round is marked applied — its rewrite landed late".to_string()
        } else {
            String::new()
        };
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
    // Unlinking a blocker can promote this task, so read it back rather than
    // echoing the state it held before the edge went.
    let (task, unlink_echo) = if args.unlink.is_empty() {
        (task, String::new())
    } else {
        let echo = apply_unlink_flag(store, id, &args.unlink)?;
        (store.task(id).map_err(|e| e.to_string())?, echo)
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
    let task = if args.no_repo {
        store.set_task_repo(id, None).map_err(|e| e.to_string())?
    } else if let Some(name) = &args.repo {
        let project = store.project(task.project_id).map_err(|e| e.to_string())?;
        let repo = resolve_repo(store, &project, name)?;
        store
            .set_task_repo(id, Some(repo.id))
            .map_err(|e| e.to_string())?
    } else {
        task
    };
    // `--doc` replaces the whole list, as `--blocked-by` does, so the flag can
    // drop a link as well as add one; `voro doc link` is the additive spelling.
    let docs_echo = if args.no_doc {
        store.set_task_docs(id, &[]).map_err(|e| e.to_string())?;
        "\nno documents linked".to_string()
    } else if let Some(raw) = &args.doc {
        let ids: Vec<i64> = resolve_docs(store, raw)?.iter().map(|d| d.id).collect();
        let docs = store.set_task_docs(id, &ids).map_err(|e| e.to_string())?;
        format!("\n{}", doc_link_echo(&docs))
    } else {
        String::new()
    };
    Ok(format!(
        "task {} updated ({}){blocks_echo}{unlink_echo}{docs_echo}{refine_echo}",
        task.id, task.state
    ))
}

/// How a mutated document list reads back — the ids and labels now linked, so
/// the operator sees the resolution a bare id or a path matched.
fn doc_link_echo(docs: &[Doc]) -> String {
    let listed: Vec<String> = docs
        .iter()
        .map(|d| format!("{} '{}'", d.id, d.label()))
        .collect();
    format!("docs: {}", listed.join(", "))
}

/// `pr <task-id> [--yes]` (DESIGN.md §8/§11c): the GitHub half of "show me this
/// task's diff", whatever viewer the project names. With a tracked PR,
/// open it in a browser. Without one, create the PR from the review task's
/// done-time state — asserting PR-readiness, confirming unless `--yes`. A
/// checkout that cannot take a PR errors pointing at `voro open`, which is the
/// only local-diff spelling.
fn pr_verb(store: &mut Store, id: i64, yes: bool) -> Result<String, String> {
    let task = store.task(id).map_err(|e| e.to_string())?;
    if task.pr_url.is_some() {
        return crate::pr::open(store, id);
    }
    // Assert PR-ready and learn the branch before prompting, so a task missing
    // state, branch, or summary fails naming the gap rather than at the prompt.
    let plan = crate::pr::plan(store, id)?;
    let local_diff = format!("`voro open {id}`");
    // Then the medium, before the prompt rather than after it: nothing is worth
    // confirming on a checkout GitHub cannot take.
    let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;
    crate::pr::ensure_github_repo(&repo.path, &local_diff)?;
    if !yes && !confirm(&format!("push `{}` and open a PR for #{id}?", plan.branch))? {
        return Ok(format!("cancelled — no PR opened for #{id}"));
    }
    crate::pr::create(store, id, &local_diff).map(|url| format!("opened {url} for task {id}"))
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
    // The head at the moment of rejection is exactly what the operator judged,
    // so it is recorded to narrow the re-review to the rework (DESIGN.md §8).
    // Best-effort, and after the transition: an unreadable revision costs a full
    // diff rather than a reject, and a refused reject records nothing.
    crate::pr::record_reviewed(store, id);
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
    // A code-producing task's complete report carries both a branch and a
    // summary whichever key reviews it, so warn (never fail) about whichever
    // is absent — this note is ephemeral and costs the operator nothing, so it
    // stays symmetric where the durable flag only marks a branch without a
    // summary (DESIGN.md §8). A human task lands straight in `done` with no
    // report to read, so it earns no warning.
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
            // Naming the shape of the half that is missing, where that half is
            // the summary: an agent told to supply one is told it is the PR
            // description (DESIGN.md §8).
            let shape = if has_summary {
                ""
            } else {
                "; the summary is the PR description `pr` opens the pull request from"
            };
            write!(
                out,
                "\nnote: no {} recorded — complete the report with `voro set {id}` if this task produced code{shape}",
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
    if task.deep {
        writeln!(
            out,
            "deep: dispatches on the agent's strongest model, not its workhorse"
        )
        .unwrap();
    }
    // The checkout this task runs in, spelled out only when it is not the
    // project default — a single-repo project reads exactly as it always did.
    if task.repo_id.is_some() {
        let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;
        writeln!(out, "repo: {} ({})", repo.name, repo.path).unwrap();
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
    // The incomplete-report flag withholds `pr` and nothing else (DESIGN.md §8),
    // so it is read before the verb is advertised: a pull request is built from
    // the summary a half-written report is missing, and recommending one above
    // the line explaining why it cannot be built is a recommendation that could
    // only fail, merely spelled out. The marker itself keeps its own line.
    let incomplete = store
        .incomplete_report_flag(id)
        .map_err(|e| e.to_string())?;
    if let Some(verb) = task
        .next_action()
        .map(|verb| advertised(store, &mut ForgeMemo::default(), &task, verb))
        .filter(|verb| !incomplete || *verb != NextAction::Pr)
    {
        writeln!(out, "next: {verb}").unwrap();
    }
    if incomplete {
        writeln!(
            out,
            "incomplete report: a branch is recorded but no summary — complete it with `voro set {id} --summary ...`, written as the PR description `pr` opens the pull request from"
        )
        .unwrap();
    }
    // The refine markers (DESIGN.md §6): whether the last round on this proposal
    // reworked the body — so the operator triages the improved version knowing
    // it moved — or died having applied nothing, which must not read as a
    // proposal nobody refined.
    if store.refined_flag(id).map_err(|e| e.to_string())? {
        let note = store
            .latest_refine_note(id)
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        writeln!(out, "refined: {note}").unwrap();
    }
    if store.refine_failed_flag(id).map_err(|e| e.to_string())? {
        writeln!(
            out,
            "refine failed: the agent ended without rewriting the body — the brief below is \
             the one it was given"
        )
        .unwrap();
    }
    // The stale-branch marker (DESIGN.md §8): one on-demand `gh` probe of the
    // tracked PR's mergeability, only for a review task that has one. Purely
    // informational — a CONFLICTING verdict flags that the branch needs
    // resolving before it can merge; MERGEABLE, UNKNOWN, and a missing `gh`
    // all show nothing.
    if task.state == TaskState::Review
        && task.pr_url.is_some()
        && crate::pr::conflict_status(store, id).conflicts()
    {
        writeln!(
            out,
            "conflicts: branch no longer merges with the base — resolve before merging"
        )
        .unwrap();
    }
    // What the agent reported (DESIGN.md §8), the CLI's mirror of the detail
    // pane's block: the completion summary of the cycle in hand, headed on a
    // rework by the feedback it answers, rather than dug out of the event log.
    if matches!(task.state, TaskState::Review | TaskState::Waiting)
        && let Some(report) =
            voro_core::completion_report(&store.events_for(id).map_err(|e| e.to_string())?)
    {
        let heading = match report.feedback {
            Some(_) => "response to the review feedback:",
            None => "completion summary:",
        };
        writeln!(out, "{heading}\n{}", report.summary).unwrap();
    }
    // The plans this task implements (DESIGN.md §3), resolved to where they
    // actually are — the same absolute location dispatch hands the agent.
    for doc in store.docs_for_task(id).map_err(|e| e.to_string())? {
        let resolved = store.resolve_doc(&doc).map_err(|e| e.to_string())?;
        match &doc.title {
            Some(title) => writeln!(out, "doc: {title} — {resolved}").unwrap(),
            None => writeln!(out, "doc: {resolved}").unwrap(),
        }
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
        writeln!(out, "  {}  {}  {}", e.at, e.kind, event_detail(&e)).unwrap();
    }
    Ok(out)
}

/// One event's recorded detail, alone and undecorated, so it can be redirected
/// straight into a file: `voro show 62 --event 512 > body.md` is how the body a
/// `set` replaced comes back (DESIGN.md §8). The event must belong to the task
/// named, so a mistyped id reads as an error rather than another task's text.
fn show_event(store: &mut Store, task_id: i64, event_id: i64) -> Result<String, String> {
    let events = store.events_for(task_id).map_err(|e| e.to_string())?;
    let event = events
        .iter()
        .find(|e| e.id == event_id)
        .ok_or_else(|| format!("task {task_id} has no event {event_id}"))?;
    Ok(event.detail.clone().unwrap_or_default())
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
    // "Which tasks derive from this plan?" — the query the doc link exists for.
    let doc_filter: Option<Vec<i64>> = match &args.doc {
        Some(key) => {
            let doc = resolve_doc(store, key)?;
            Some(
                store
                    .tasks_for_doc(doc.id)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|t| t.id)
                    .collect(),
            )
        }
        None => None,
    };
    let projects = store.projects().map_err(|e| e.to_string())?;
    let mut forges = ForgeMemo::default();
    let mut out = String::new();
    for task in store.tasks().map_err(|e| e.to_string())? {
        if state_filter.is_some_and(|s| task.state != s)
            || project_filter.is_some_and(|p| task.project_id != p)
            || doc_filter
                .as_ref()
                .is_some_and(|ids| !ids.contains(&task.id))
        {
            continue;
        }
        let name = projects
            .iter()
            .find(|p| p.id == task.project_id)
            .map(|p| p.name.as_str())
            .unwrap_or("?");
        let incomplete = incomplete_report_suffix(store, task.id);
        writeln!(
            out,
            "{}{}{}{}",
            task_line(&task, name),
            refined_suffix(store, task.id),
            review_next_suffix(store, &mut forges, &task, !incomplete.is_empty()),
            incomplete
        )
        .unwrap();
    }
    Ok(out)
}

/// A review row's next action as a browser suffix (DESIGN.md §3). The list
/// shows state in its own column, so only `review` — whose verb reads the
/// tracked PR, not the state alone — earns the suffix.
///
/// `incomplete` says the row already carries the `[incomplete report]` marker,
/// which withholds `pr` and nothing else (§8): a pull request is built from the
/// summary the row is missing, but reading a diff is not, so a checkout with no
/// remote keeps its `next: open` and wears the marker beside it.
fn review_next_suffix(
    store: &Store,
    forges: &mut ForgeMemo,
    task: &Task,
    incomplete: bool,
) -> String {
    if task.state != TaskState::Review {
        return String::new();
    }
    task.next_action()
        .map(|verb| advertised(store, forges, task, verb))
        .filter(|verb| !incomplete || *verb != NextAction::Pr)
        .map_or_else(String::new, |verb| format!("  next: {verb}"))
}

/// The verb a row advertises (DESIGN.md §3): the derived one, except that `pr`
/// degrades to the local review path in a checkout with no remote to open a
/// pull request on (§8) — a recommendation that could only fail is worse than
/// none. A checkout that cannot be resolved keeps the derived verb, so nothing
/// but a definite answer moves a row.
fn advertised(store: &Store, forges: &mut ForgeMemo, task: &Task, verb: NextAction) -> NextAction {
    if verb != NextAction::Pr {
        return verb;
    }
    match store.repo_for_task(task) {
        Ok(repo) if !forges.takes_pull_requests(&repo.path) => verb.without_pull_requests(),
        _ => verb,
    }
}

fn inbox_verb(store: &mut Store, ctx: &DispatchCtx) -> Result<String, String> {
    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let candidates = store.candidates().map_err(|e| e.to_string())?;
    let gate = WipGate {
        running: store.state_counts().map_err(|e| e.to_string())?.running,
        max_running: config.max_running(),
    };
    let queue = scheduler::queue(&candidates, &config.costs(), gate);
    let mut forges = ForgeMemo::default();
    let mut out = String::new();
    // The capacity line stands in for the dispatch rows the gate suppressed,
    // so an inbox with no startable work still says why (DESIGN.md §7).
    if let Some(gate) = queue.at_capacity {
        writeln!(
            out,
            "⏸ dispatch at capacity ({}/{} running)",
            gate.running, gate.max_running
        )
        .unwrap();
    }
    for row in &queue.rows {
        match row {
            QueueRow::Digest(digest) => {
                // Informational on the CLI: `voro list --state proposed` is
                // where the constituents are triaged one by one.
                // The digest hides its constituents' own markers, so the count
                // of reworked bodies rides the summary row (DESIGN.md §6).
                let refined = digest
                    .tasks
                    .iter()
                    .filter(|row| store.refined_flag(row.candidate.task.id).unwrap_or(false))
                    .count();
                let failed = digest
                    .tasks
                    .iter()
                    .filter(|row| {
                        store
                            .refine_failed_flag(row.candidate.task.id)
                            .unwrap_or(false)
                    })
                    .count();
                writeln!(
                    out,
                    "{:5.1}  ▲ {} awaiting triage ({}){}{}",
                    digest.effective,
                    plural(digest.tasks.len(), "proposal"),
                    digest.project_name,
                    if refined > 0 {
                        format!("  ↻ {refined} refined")
                    } else {
                        String::new()
                    },
                    if failed > 0 {
                        format!("  ⚠ {failed} refine failed")
                    } else {
                        String::new()
                    }
                )
                .unwrap();
            }
            QueueRow::Action(row) => {
                let c = &row.candidate;
                // The queue row carries the verb instead of the state: every
                // inbox row is a next action (DESIGN.md §3), like the TUI queue.
                // The score is the effective one the row is ranked by; `explain`
                // is where the division back to the raw score is shown.
                write!(
                    out,
                    "{:5.1}  #{} {:10} {} {}: {}",
                    row.effective,
                    c.task.id,
                    advertised(store, &mut forges, &c.task, row.action),
                    c.task.priority,
                    c.project_name,
                    c.task.title
                )
                .unwrap();
                if let Some(q) = &c.task.question {
                    write!(out, "  — {q}").unwrap();
                }
                write!(out, "{}", refined_suffix(store, c.task.id)).unwrap();
                write!(out, "{}", incomplete_report_suffix(store, c.task.id)).unwrap();
                writeln!(out).unwrap();
            }
        }
    }
    if out.is_empty() {
        out = "nothing needs you\n".to_string();
    }
    Ok(out)
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Task counts by state (DESIGN.md §12) as a scriptable readout, excluding
/// parked projects so the numbers match the queue and header.
/// Fixture data, for the dev store only (DESIGN.md §5). The refusal is on the
/// database in play, not on how the binary was built, so no combination of
/// flags and environment seeds over the operator's board.
fn seed_verb(store: &mut Store, ctx: &DispatchCtx, force: bool) -> Result<String, String> {
    if ctx.db_path == Store::production_db_path() {
        return Err(format!(
            "refusing to seed {} — that is your real store, not the dev one. A build run from \
             a target/ directory seeds {} by itself on first run.",
            Store::production_db_path().display(),
            Store::dev_db_path().display()
        ));
    }
    let empty = voro_core::seed::is_empty(store).map_err(|e| e.to_string())?;
    if !empty {
        if !force {
            return Err(format!(
                "{} already has projects — pass --force to discard them and rebuild the fixture",
                ctx.db_path.display()
            ));
        }
        store.truncate_all().map_err(|e| e.to_string())?;
    }
    let summary = voro_core::seed::seed(store).map_err(|e| e.to_string())?;
    Ok(format!(
        "seeded {} with {} projects and {} tasks",
        ctx.db_path.display(),
        summary.projects,
        summary.tasks
    ))
}

fn migrate_verb(store: &Store, ctx: &DispatchCtx) -> Result<String, String> {
    let version = store.schema_version().map_err(|e| e.to_string())?;
    Ok(format!(
        "nothing pending: {} is at schema {version}, the newest this build carries",
        ctx.db_path.display()
    ))
}

fn stats_verb(store: &mut Store) -> Result<String, String> {
    let c = store.state_counts().map_err(|e| e.to_string())?;
    let mut out = String::new();
    for (label, n) in [
        ("triage", c.proposed),
        ("refining", c.refining),
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

/// `  [incomplete report]` when a `review` task carries a branch and no summary
/// (DESIGN.md §8), else empty. Read from task and event state alone — nothing
/// here touches `gh`, since this renders per line.
fn incomplete_report_suffix(store: &Store, task_id: i64) -> &'static str {
    if store.incomplete_report_flag(task_id).unwrap_or(false) {
        "  [incomplete report]"
    } else {
        ""
    }
}

/// How the last refine round on a proposal ended (DESIGN.md §6), as a row
/// suffix: `  ↻ refined` for a body that was reworked — so the triage queue
/// shows which rows are an improved version awaiting a fresh verdict — and
/// `  ⚠ refine failed` for a round that died having applied nothing, which must
/// not read as a proposal nobody refined. Both are cleared by triage, since
/// they are gated on `proposed`.
fn refined_suffix(store: &Store, task_id: i64) -> &'static str {
    if store.refined_flag(task_id).unwrap_or(false) {
        "  ↻ refined"
    } else if store.refine_failed_flag(task_id).unwrap_or(false) {
        "  ⚠ refine failed"
    } else {
        ""
    }
}

fn explain_verb(store: &mut Store, id: i64, ctx: &DispatchCtx) -> Result<String, String> {
    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
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
    writeln!(
        out,
        "blocks          {:>6}  (bonus +{}, 1/dependent, cap 2)",
        format!("×{}", b.open_dependents),
        b.unblock_bonus
    )
    .unwrap();
    writeln!(out, "base w×(p+s+u)  {:>6.1}", b.base).unwrap();
    writeln!(out, "age             {:>6.1} days", b.age_days).unwrap();
    writeln!(
        out,
        "age bonus       {:>6.2}  (0.1/day, cap 2)",
        b.age_bonus
    )
    .unwrap();
    writeln!(out, "total           {:>6.2}", b.total).unwrap();
    // What the inbox actually ranks by: the total priced by what the row asks
    // of the operator (DESIGN.md §7).
    if let Some(e) = scheduler::effective_score(&task, b.total, &config.costs()) {
        writeln!(
            out,
            "action          {:>6}  (cost ÷{})",
            e.action.as_str(),
            e.cost
        )
        .unwrap();
        writeln!(out, "effective       {:>6.2}  (inbox rank)", e.effective).unwrap();
    }
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
/// deleting a viewer a project still names.
fn viewer_verb(store: &mut Store, cmd: ViewerCmd, ctx: &DispatchCtx) -> Result<String, String> {
    let path = &ctx.agents_path;
    match cmd {
        ViewerCmd::Add { name, cmd } => {
            // With no command, the viewer is its own name handed the checkout
            // (or the built-in's line, overriding one) — resolved here as well
            // as in the writer so the reply says what was actually recorded.
            let cmd = match cmd {
                Some(cmd) if !cmd.trim().is_empty() => cmd,
                _ => voro_core::config_edit::assumed_viewer_cmd(&name),
            };
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
                    "viewer '{name}' is the viewer of {} — repoint {} with `voro project \
                     viewer <project> [NAME]` before removing it",
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
            let default = config.default_viewer_name();
            let mut out = String::new();
            // The built-ins are listed beside the user's tables, as `agent
            // list` does: each is a viewer `open` can actually run.
            for (name, cmd, provenance) in config.viewer_entries() {
                let marker = if Some(name) == default.as_deref() {
                    "* "
                } else {
                    "  "
                };
                writeln!(out, "{marker}{name:<12} {:<14} {cmd}", provenance.label()).unwrap();
            }
            // The anonymous [viewer] table has no name but still resolves as
            // the default; show it so `list` reflects what `open` will run.
            if default.is_none()
                && let Some(cmd) = config.anonymous_viewer_cmd()
            {
                writeln!(out, "* {:<12} {:<14} {cmd}", "[viewer]", "user").unwrap();
            }
            writeln!(out, "\n({} — * is the default)", path.display()).unwrap();
            // Nothing starred and no anonymous table means no built-in is
            // installed either, so `open` has nothing to run: say so here
            // rather than leaving it to be discovered by pressing `o`.
            if default.is_none() && config.anonymous_viewer_cmd().is_none() {
                writeln!(
                    out,
                    "no viewer set up — run `voro viewer add <name> '<cmd>'`, e.g. \
                     `voro viewer add zed 'zed {{path}}'`; none of the built-in {} are on PATH",
                    voro_core::BUILTIN_VIEWER_NAMES.join("/")
                )
                .unwrap();
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
    // Import runs against one checkout — the named repo, else the default —
    // and the tasks it creates carry that repo, so they dispatch where their
    // issues live (DESIGN.md §8).
    let repo = match &args.repo {
        Some(name) => resolve_repo(store, &project, name)?,
        None => store.default_repo(project.id).map_err(|e| e.to_string())?,
    };
    let json = import::fetch_issues(&repo.path, args.gh_repo.as_deref())?;
    let repo_id = if args.repo.is_some() {
        Some(repo.id)
    } else {
        None
    };
    let summary = import::import_issues(store, &project, repo_id, &json)?;
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

fn ask_verb(store: &mut Store, ctx: &DispatchCtx, args: AskArgs) -> Result<String, String> {
    let question = match args.question {
        Some(q) => q,
        None => joined(&args.text, "question (--question TEXT)")?,
    };
    apply_action(store, ctx, args.task_id, Action::Ask(question), false)
}

/// Triage a proposal (DESIGN.md §6). Three of the four outcomes are verdicts
/// that transition the task; `refine` is the fourth — it dispatches an agent to
/// rewrite the body against the operator's note, so the improved version comes
/// back round for a real verdict. The verdicts act on a proposal alone; refine
/// also accepts a `ready` id, whose rewritten body then returns through triage.
/// A note is required here: the note-less interactive variant is an agent
/// conversation, which is TUI-only for the same reason planning sessions are
/// (§8) — the CLI is how an LLM drives Voro.
fn triage_verb(store: &mut Store, args: TriageArgs, ctx: &DispatchCtx) -> Result<String, String> {
    let note = text_or_file(args.note, args.note_file)?;
    match Triage::try_from(args.target) {
        Ok(verdict) => {
            if note.is_some() {
                return Err("--note applies to `triage <id> refine` only".into());
            }
            apply_action(store, ctx, args.task_id, Action::Triage(verdict), false)
        }
        Err(()) => {
            let Some(note) = note else {
                return Err(format!(
                    "refine needs a note saying what to fix: `voro triage {} refine --note \
                     \"...\"`. The note-less interactive variant is a conversation with the \
                     agent, so it lives in the TUI, on `R` over a proposal",
                    args.task_id
                ));
            };
            dispatch::refine(store, ctx, args.task_id, &note)
        }
    }
}

/// Apply a plain state-machine action. `accept`/`abandon` are the terminal
/// transitions that own the dispatch worktree's teardown (§8; `yes` skips its
/// confirmation). The transition applies first and stands regardless of cleanup
/// — and regardless of the agent-session stop that rides the same close (§8),
/// which is fire-and-forget.
fn apply_action(
    store: &mut Store,
    ctx: &DispatchCtx,
    id: i64,
    action: Action,
    yes: bool,
) -> Result<String, String> {
    let closes = matches!(action, Action::Accept | Action::Abandon);
    let (task, stopped) = store.apply_closing(id, action).map_err(|e| e.to_string())?;
    if let Some(session) = stopped {
        dispatch::stop_closed_session(ctx, &session);
    }
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
    // The task's own checkout: its worktree lives in the repo it ran in.
    let repo = store.repo_for_task(task).map_err(|e| e.to_string())?;
    let Some(plan) = crate::worktree::Cleanup::plan(task, &repo.path)? else {
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
    use voro_core::LivenessSource;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn ctx() -> DispatchCtx {
        DispatchCtx::without_config(std::path::Path::new("/nonexistent/voro.db"))
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
    fn repo_add_list_default_and_remove_through_the_cli() {
        let mut s = store();
        ok(&mut s, &["project", "add", "odm", "/tmp/odm"]);

        // The project's own checkout is already a repo, named after it.
        let out = ok(&mut s, &["repo", "list", "odm"]);
        assert!(out.contains("* odm"), "{out}");
        assert!(out.contains("/tmp/odm"), "{out}");

        ok(&mut s, &["repo", "add", "odm", "oats", "/tmp/oats"]);
        let out = ok(&mut s, &["repo", "list", "odm"]);
        assert!(out.contains("/tmp/oats"), "{out}");
        // `repo list` with no project spans every project.
        assert!(ok(&mut s, &["repo", "list"]).contains("oats"));

        ok(&mut s, &["repo", "path", "odm", "oats", "/tmp/oats2"]);
        assert!(ok(&mut s, &["repo", "list", "odm"]).contains("/tmp/oats2"));

        // Removing the default is refused until another is promoted.
        let refusal = err(&mut s, &["repo", "remove", "odm", "odm"]);
        assert!(refusal.contains("default repo"), "{refusal}");
        let out = ok(&mut s, &["repo", "default", "odm", "oats"]);
        assert!(out.contains("oats"), "{out}");
        // The projects listing follows the new default and tags the extra.
        let out = ok(&mut s, &["project", "list"]);
        assert!(out.contains("/tmp/oats2"), "{out}");
        assert!(out.contains("+1 repo"), "{out}");

        ok(&mut s, &["repo", "remove", "odm", "odm"]);
        let refusal = err(&mut s, &["repo", "remove", "odm", "oats"]);
        assert!(refusal.contains("only repo"), "{refusal}");
    }

    #[test]
    fn add_repo_names_the_task_checkout_and_a_bad_name_lists_the_valid_ones() {
        let mut s = store();
        ok(&mut s, &["project", "add", "odm", "/tmp/odm"]);
        ok(&mut s, &["repo", "add", "odm", "oats", "/tmp/oats"]);

        let out = ok(
            &mut s,
            &[
                "add",
                "odm",
                "keyed results",
                "--repo",
                "oats",
                "--state",
                "ready",
            ],
        );
        assert!(out.contains("in repo 'oats'"), "{out}");
        // The message is not the point — the stored checkout is.
        let task = s.task(1).unwrap();
        assert_eq!(s.repo_for_task(&task).unwrap().path, "/tmp/oats");
        assert!(ok(&mut s, &["show", "1"]).contains("repo: oats (/tmp/oats)"));

        // A task filed without --repo stays on the project default.
        ok(&mut s, &["add", "odm", "elsewhere", "--state", "ready"]);
        let plain = s.task(2).unwrap();
        assert_eq!(plain.repo_id, None);
        assert_eq!(s.repo_for_task(&plain).unwrap().path, "/tmp/odm");
        assert!(!ok(&mut s, &["show", "2"]).contains("repo:"));

        // A filing agent that names a repo the project does not have gets a
        // correction listing the real ones, not a wrong checkout.
        let refusal = err(&mut s, &["add", "odm", "t", "--repo", "nope"]);
        assert!(refusal.contains("nope"), "{refusal}");
        assert!(refusal.contains("odm, oats"), "{refusal}");
    }

    #[test]
    fn set_repo_repoints_a_task_and_no_repo_returns_it_to_the_default() {
        let mut s = store();
        ok(&mut s, &["project", "add", "odm", "/tmp/odm"]);
        ok(&mut s, &["repo", "add", "odm", "oats", "/tmp/oats"]);
        ok(&mut s, &["add", "odm", "t", "--state", "ready"]);

        ok(&mut s, &["set", "1", "--repo", "oats"]);
        let task = s.task(1).unwrap();
        assert_eq!(s.repo_for_task(&task).unwrap().path, "/tmp/oats");

        let refusal = err(&mut s, &["set", "1", "--repo", "nope"]);
        assert!(refusal.contains("odm, oats"), "{refusal}");

        ok(&mut s, &["set", "1", "--no-repo"]);
        let task = s.task(1).unwrap();
        assert_eq!(task.repo_id, None);
        assert_eq!(s.repo_for_task(&task).unwrap().path, "/tmp/odm");
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
            message_grace: std::time::Duration::from_millis(300),
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
        // every optional verb the agent defines, the quick message included —
        // named plainly, since the built-in resumes its session in place
        assert!(
            listed.contains("[sessions attach resume message logs stop plan]"),
            "{listed}"
        );

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
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut s = store();
        let call = |s: &mut Store, args: &[&str]| {
            run(s, args.iter().map(|x| x.to_string()).collect(), &ctx)
        };

        // add then list shows it — `viewer list` gains its inverse
        let out = call(&mut s, &["viewer", "add", "mine", "mine {path}"]).unwrap();
        assert!(out.contains("mine"), "{out}");
        let listed = call(&mut s, &["viewer", "list"]).unwrap();
        assert!(
            listed.contains("mine") && listed.contains("mine {path}"),
            "{listed}"
        );

        // a duplicate name is refused
        let e = call(&mut s, &["viewer", "add", "mine", "mine ."]).unwrap_err();
        assert!(e.contains("already exists"), "{e}");

        // a built-in is overridden rather than removed (#405)
        let e = call(&mut s, &["viewer", "remove", "zed"]).unwrap_err();
        assert!(e.contains("built into voro"), "{e}");
        call(&mut s, &["viewer", "add", "zed", "zed --wait {path}"]).unwrap();
        let listed = call(&mut s, &["viewer", "list"]).unwrap();
        assert!(listed.contains("user override"), "{listed}");
        call(&mut s, &["viewer", "remove", "zed"]).unwrap();

        // no command at all assumes the obvious one and says what it recorded
        let out = call(&mut s, &["viewer", "add", "emacs"]).unwrap();
        assert!(out.contains("emacs {path}"), "{out}");
        call(&mut s, &["viewer", "remove", "emacs"]).unwrap();

        // a command with no {path} succeeds but warns
        let out = call(&mut s, &["viewer", "add", "difftool", "git difftool -d"]).unwrap();
        assert!(out.contains("{path}"), "{out}");

        // a project naming mine blocks its removal, naming the project
        call(&mut s, &["project", "add", "demo", "/tmp/demo"]).unwrap();
        call(&mut s, &["project", "viewer", "demo", "mine"]).unwrap();
        let e = call(&mut s, &["viewer", "remove", "mine"]).unwrap_err();
        assert!(e.contains("demo") && e.contains("is the viewer of"), "{e}");

        // repoint the project, then removal succeeds and list loses it
        call(&mut s, &["project", "viewer", "demo"]).unwrap();
        let out = call(&mut s, &["viewer", "remove", "mine"]).unwrap();
        assert!(out.contains("removed"), "{out}");
        let listed = call(&mut s, &["viewer", "list"]).unwrap();
        assert!(!listed.contains("mine"), "{listed}");
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

    // --- closing verdicts stop the agent's session (task #433) ---

    /// A context whose `claude` agent records what its `stop` verb was fired at,
    /// plus that marker's path and the directory to clean up.
    fn stop_ctx(name: &str) -> (DispatchCtx, std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("voro-cli-stop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("stopped");
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            format!(
                "default_agent = \"claude\"\n\n[agents.claude]\n\
                 dispatch = \"cat {{prompt_file}}\"\n\
                 stop = \"printf '%s' {{session}} >> '{}'\"\n",
                marker.display()
            ),
        )
        .unwrap();
        let ctx = DispatchCtx {
            agents_path,
            ..DispatchCtx::from_db_path(&dir.join("voro.db"))
        };
        (ctx, marker, dir)
    }

    /// Wait for the detached stop to have written its marker — nothing in the
    /// transition path waits on it — and return what it recorded.
    fn stopped_ref(marker: &std::path::Path) -> Option<String> {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(marker)
                && !text.is_empty()
            {
                return Some(text);
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        None
    }

    /// A project, a task in `review`, and an open session on it carrying the
    /// reference the agent knows it by — the state each closing verdict acts on.
    fn task_in_review_with_a_session(s: &mut Store, ctx: &DispatchCtx) {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        run(s, args(&["project", "add", "demo", "/tmp"]), ctx).unwrap();
        run(s, args(&["add", "demo", "A task", "--state", "ready"]), ctx).unwrap();
        let (_, session) = s
            .record_dispatch(1, "claude", Some(1), LivenessSource::Listing, None)
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();
        run(s, args(&["done", "1"]), ctx).unwrap();
    }

    /// Each closing verdict retires the agent's own entry for the session it
    /// closed, at the reference Voro captured (DESIGN.md §8) — so the agent's
    /// listing shows work in flight rather than every dispatch ever made.
    #[test]
    fn a_closing_verdict_stops_the_agents_session() {
        for (name, verb) in [("accept", "accept"), ("abandon", "abandon")] {
            let (ctx, marker, dir) = stop_ctx(name);
            let mut s = store();
            task_in_review_with_a_session(&mut s, &ctx);

            let out = run(
                &mut s,
                vec![verb.to_string(), "1".to_string(), "--yes".to_string()],
                &ctx,
            )
            .unwrap();
            assert!(out.contains("task 1"), "{out}");
            assert_eq!(
                stopped_ref(&marker).as_deref(),
                Some("full-uuid-1"),
                "{name}"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// `abort` is the third, from `running` rather than `review`: the operator
    /// calling the work off, which takes the session down with it.
    #[test]
    fn abort_stops_the_agents_session() {
        let (ctx, marker, dir) = stop_ctx("abort");
        let mut s = store();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        run(&mut s, args(&["project", "add", "demo", "/tmp"]), &ctx).unwrap();
        run(
            &mut s,
            args(&["add", "demo", "A task", "--state", "ready"]),
            &ctx,
        )
        .unwrap();
        let (_, session) = s
            .record_dispatch(
                1,
                "claude",
                Some(std::process::id() as i64),
                LivenessSource::Pid,
                None,
            )
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        run(&mut s, args(&["abort", "1"]), &ctx).unwrap();
        assert_eq!(stopped_ref(&marker).as_deref(), Some("full-uuid-1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stop rides the transition rather than gating it: a `stop` verb that
    /// fails, and an agent that names none at all, both leave the accept
    /// committed and reported exactly as before.
    #[test]
    fn a_failing_or_absent_stop_leaves_the_transition_committed() {
        for (name, stop) in [("failing", "false # {session}"), ("absent", "")] {
            let dir =
                std::env::temp_dir().join(format!("voro-cli-stop-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let agents_path = dir.join("voro.toml");
            let stop_line = if stop.is_empty() {
                String::new()
            } else {
                format!("stop = \"{stop}\"\n")
            };
            std::fs::write(
                &agents_path,
                format!(
                    "default_agent = \"claude\"\n\n[agents.claude]\n\
                     dispatch = \"cat {{prompt_file}}\"\n{stop_line}"
                ),
            )
            .unwrap();
            let ctx = DispatchCtx {
                agents_path,
                ..DispatchCtx::from_db_path(&dir.join("voro.db"))
            };
            let mut s = store();
            task_in_review_with_a_session(&mut s, &ctx);

            let out = run(&mut s, vec!["accept".into(), "1".into()], &ctx).unwrap();
            assert!(out.contains("-> done"), "{name}: {out}");
            assert_eq!(
                s.task(1).unwrap().state,
                voro_core::TaskState::Done,
                "{name}"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The transitions that keep the session stop nothing: the operator answers
    /// into a `needs-input` session and rejects into a `review` one, so both
    /// must still be there afterwards.
    #[test]
    fn a_transition_that_keeps_the_session_stops_nothing() {
        for (name, argv) in [
            ("ask", vec!["ask", "1", "--question", "A or B?"]),
            ("reject", vec!["reject", "1", "tests missing"]),
            ("wait", vec!["wait", "1"]),
        ] {
            let (ctx, marker, dir) = stop_ctx(name);
            let mut s = store();
            task_in_review_with_a_session(&mut s, &ctx);
            if name == "ask" {
                // ask is legal from running, so put the task back there first
                run(
                    &mut s,
                    vec!["reject".into(), "1".into(), "wip".into()],
                    &ctx,
                )
                .unwrap();
            }

            run(&mut s, argv.iter().map(|a| a.to_string()).collect(), &ctx).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(!marker.exists(), "{name} stopped a session it kept open");

            let _ = std::fs::remove_dir_all(&dir);
        }
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
        // Proposals ride the inbox as one digest row per project (DESIGN.md §7).
        assert!(
            ok(&mut s, &["inbox"]).contains("1 proposal awaiting triage (demo)"),
            "{}",
            ok(&mut s, &["inbox"])
        );
        ok(&mut s, &["triage", "1", "ready"]);
        assert!(ok(&mut s, &["inbox"]).contains("P2 demo: An idea"));
        assert!(ok(&mut s, &["next"]).contains("An idea"));
    }

    /// The fourth triage outcome (DESIGN.md §6). The dispatch half needs a real
    /// agent, so it is covered in `dispatch.rs`; what belongs here is the verb's
    /// own contract — a note is required, it is refine's alone, and the three
    /// verdicts are untouched.
    #[test]
    fn triage_refine_requires_a_note_and_leaves_the_verdicts_alone() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);

        let e = err(&mut s, &["triage", "1", "refine"]);
        assert!(e.contains("--note"), "{e}");
        assert!(e.contains("TUI"), "{e}");
        assert_eq!(s.task(1).unwrap().state, TaskState::Proposed);

        // A note on a verdict is a misuse, not a silently ignored flag.
        let e = err(&mut s, &["triage", "1", "ready", "--note", "hmm"]);
        assert!(e.contains("refine"), "{e}");
        assert_eq!(s.task(1).unwrap().state, TaskState::Proposed);

        // refine is parsed as a target: reaching the dispatch is what fails
        // here, since the test context names no config or checkout.
        let e = err(&mut s, &["triage", "1", "refine", "--note", "thin body"]);
        assert!(!e.contains("invalid value"), "{e}");
        assert_eq!(s.task(1).unwrap().state, TaskState::Proposed);
        assert!(!s.refined_flag(1).unwrap());

        ok(&mut s, &["triage", "1", "parked"]);
        assert_eq!(s.task(1).unwrap().state, TaskState::Parked);
    }

    /// The `↻ refined` marker (DESIGN.md §6) rides the concluded round, so it
    /// shows on every proposal view until triage takes the task out of
    /// `proposed` — and never while the round is still in flight.
    #[test]
    fn a_refined_proposal_is_marked_until_it_is_triaged() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        assert!(!ok(&mut s, &["inbox"]).contains("refined"));

        s.record_refine_launch(
            1,
            "name the files it touches",
            "claude",
            None,
            LivenessSource::Pid,
            None,
        )
        .unwrap();
        // Mid-round the proposal is out of the queue entirely, and nothing
        // claims a rewrite that has not landed.
        assert!(!ok(&mut s, &["inbox"]).contains("awaiting triage"));
        assert!(!ok(&mut s, &["list"]).contains("↻ refined"));
        assert!(ok(&mut s, &["stats"]).contains("refining    1"));

        s.conclude_refine(1, RefineOutcome::Applied).unwrap();
        // The inbox collapses proposals into their project's digest row, so
        // the marker rides that row as a count; `list` carries it per task.
        assert!(ok(&mut s, &["inbox"]).contains("↻ 1 refined"));
        assert!(ok(&mut s, &["list"]).contains("↻ refined"));

        // `show` names the note both as a header line and in the event log.
        let out = ok(&mut s, &["show", "1"]);
        assert!(out.contains("refined: name the files it touches"), "{out}");
        assert!(
            out.lines()
                .any(|l| l.contains("refined") && l.contains("name the files it touches")),
            "{out}"
        );

        ok(&mut s, &["triage", "1", "ready"]);
        assert!(!ok(&mut s, &["inbox"]).contains("refined"));
        assert!(!ok(&mut s, &["list"]).contains("↻ refined"));
    }

    /// A round that died having applied nothing must not read as a proposal
    /// nobody refined (DESIGN.md §6) — the operator should never have to notice
    /// an absence.
    #[test]
    fn a_failed_refine_round_is_marked_distinctly() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        s.record_refine_launch(
            1,
            "name the files",
            "claude",
            None,
            LivenessSource::Pid,
            None,
        )
        .unwrap();
        s.conclude_refine(1, RefineOutcome::Failed).unwrap();

        assert!(ok(&mut s, &["inbox"]).contains("⚠ 1 refine failed"));
        assert!(ok(&mut s, &["list"]).contains("⚠ refine failed"));
        let out = ok(&mut s, &["show", "1"]);
        assert!(out.contains("refine failed"), "{out}");
        assert!(!out.contains("↻ refined"), "{out}");

        // A quit that concluded nothing is a no-op, not a failure: no marker.
        s.record_refine_launch(1, "again", "claude", None, LivenessSource::Pid, None)
            .unwrap();
        s.conclude_refine(1, RefineOutcome::Cancelled).unwrap();
        assert!(!ok(&mut s, &["list"]).contains("refine failed"));
        assert!(!ok(&mut s, &["list"]).contains("↻ refined"));

        ok(&mut s, &["triage", "1", "ready"]);
        assert!(!ok(&mut s, &["list"]).contains("refine failed"));
    }

    /// The agent's half of a refine: it applies the rewritten body through the
    /// ordinary `set --body-file`, which is what concludes the round — the task
    /// returns to `proposed` marked for a re-triage of the improved version.
    #[test]
    fn a_refine_agent_applies_the_body_through_set_which_ends_the_round() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea", "--body", "thin"]);
        s.record_refine_launch(
            1,
            "make it dispatchable",
            "claude",
            None,
            LivenessSource::Pid,
            None,
        )
        .unwrap();
        let session = s.sessions_for(1).unwrap()[0].id;

        let path = std::env::temp_dir().join(format!("voro-refine-{}.md", std::process::id()));
        std::fs::write(&path, "# Rewritten\n\nNames real files and criteria.\n").unwrap();
        let out = ok(&mut s, &["set", "1", "--body-file", path.to_str().unwrap()]);
        std::fs::remove_file(&path).unwrap();

        assert!(out.contains("(proposed)"), "{out}");
        let task = s.task(1).unwrap();
        assert!(task.body.contains("Names real files"), "{}", task.body);
        assert_eq!(task.state, TaskState::Proposed);
        assert_eq!(task.priority, Priority::P2);
        assert!(s.refined_flag(1).unwrap());
        // the round's session closed with it
        let session = s.session(session).unwrap();
        assert_eq!(session.outcome, Some(voro_core::SessionOutcome::Completed));
    }

    /// Only a body *replacement* ends the round: a `set` touching anything else
    /// leaves the agent to carry on, since the rewrite has not landed yet.
    #[test]
    fn a_set_without_a_body_leaves_the_round_running() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea", "--body", "thin"]);
        s.record_refine_launch(
            1,
            "make it dispatchable",
            "claude",
            None,
            LivenessSource::Pid,
            None,
        )
        .unwrap();

        ok(&mut s, &["set", "1", "--priority", "1"]);
        assert_eq!(s.task(1).unwrap().state, TaskState::Refining);

        ok(&mut s, &["set", "1", "--append-body", "a finding"]);
        assert_eq!(s.task(1).unwrap().state, TaskState::Refining);
    }

    /// The late-rewrite backstop (DESIGN.md §6): a round finalised `failed`
    /// while its agent was in fact still working leaves the rewrite landing on a
    /// `proposed` task, too late to conclude anything. The body must not sit
    /// under a marker saying no rewrite happened, so the `set` corrects the
    /// round's recorded outcome instead.
    #[test]
    fn a_body_landing_after_a_failed_round_flips_the_marker() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea", "--body", "thin"]);
        s.record_refine_launch(
            1,
            "make it dispatchable",
            "claude",
            None,
            LivenessSource::Pid,
            None,
        )
        .unwrap();
        s.conclude_refine(1, RefineOutcome::Failed).unwrap();
        assert!(s.refine_failed_flag(1).unwrap());

        let path = std::env::temp_dir().join(format!("voro-late-refine-{}.md", std::process::id()));
        std::fs::write(&path, "# Rewritten\n\nNames real files and criteria.\n").unwrap();
        let out = ok(&mut s, &["set", "1", "--body-file", path.to_str().unwrap()]);
        std::fs::remove_file(&path).unwrap();

        assert!(out.contains("its rewrite landed late"), "{out}");
        assert!(s.refined_flag(1).unwrap());
        assert!(!s.refine_failed_flag(1).unwrap());
        assert!(ok(&mut s, &["list"]).contains("↻ refined"));
        // The correction transitions nothing: the proposal still awaits triage.
        assert_eq!(s.task(1).unwrap().state, TaskState::Proposed);

        // An ordinary edit of a proposal nobody refined says nothing about
        // refining, and a second body change corrects nothing further.
        let out = ok(&mut s, &["set", "1", "--body", "another pass"]);
        assert!(!out.contains("rewrite landed late"), "{out}");
    }

    /// A verdict on a refining task is refused by the transition API itself
    /// (DESIGN.md §6) — the mid-refine race closes at the store layer, with no
    /// guard code in the verb.
    #[test]
    fn triage_is_refused_on_a_refining_task() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        s.record_refine_launch(1, "thin body", "claude", None, LivenessSource::Pid, None)
            .unwrap();

        for verdict in ["ready", "parked", "reject"] {
            let e = err(&mut s, &["triage", "1", verdict]);
            assert!(e.contains("refining"), "{e}");
            assert_eq!(s.task(1).unwrap().state, TaskState::Refining);
        }

        // and a second round cannot be started on top of the first
        let e = err(&mut s, &["triage", "1", "refine", "--note", "again"]);
        assert!(e.contains("already being refined"), "{e}");
    }

    /// A body replacement is destructive, so the two guards of DESIGN.md §8
    /// hold: one that would leave the body empty is refused outright, and the
    /// text any accepted replacement discards is recoverable from the log.
    #[test]
    fn emptying_a_body_is_refused_and_a_replaced_one_is_recoverable() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(
            &mut s,
            &["add", "demo", "An idea", "--body", "the brief\nline two"],
        );

        let e = err(&mut s, &["set", "1", "--body", ""]);
        assert!(e.contains("--allow-empty"), "{e}");
        assert_eq!(s.task(1).unwrap().body, "the brief\nline two");

        ok(&mut s, &["set", "1", "--body", "a rewrite"]);
        assert_eq!(s.task(1).unwrap().body, "a rewrite");

        // The history says a body was replaced and names the event to get it
        // back, rather than unrolling the old text into the listing.
        let out = ok(&mut s, &["show", "1"]);
        assert!(out.contains("replaced body kept (2 lines)"), "{out}");
        assert!(!out.contains("line two"), "{out}");

        let event = out
            .lines()
            .find(|l| l.contains("--event"))
            .and_then(|l| l.rsplit(' ').next().map(str::to_string))
            .expect("the body event names its own id");
        assert_eq!(
            ok(&mut s, &["show", "1", "--event", &event]),
            "the brief\nline two"
        );
        assert!(err(&mut s, &["show", "1", "--event", "999"]).contains("no event 999"));

        // Emptying it is allowed once said explicitly — and still recoverable.
        ok(&mut s, &["set", "1", "--body", "", "--allow-empty"]);
        assert_eq!(s.task(1).unwrap().body, "");
    }

    /// `--append-body-file` is the additive spelling for the common "record a
    /// finding on the task" case, which otherwise gets written as a replacement
    /// and takes the brief with it (DESIGN.md §8).
    #[test]
    fn append_body_adds_to_the_brief_instead_of_replacing_it() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea", "--body", "the brief\n"]);

        let path = std::env::temp_dir().join(format!("voro-append-{}.md", std::process::id()));
        std::fs::write(&path, "a finding\n").unwrap();
        ok(
            &mut s,
            &["set", "1", "--append-body-file", path.to_str().unwrap()],
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(s.task(1).unwrap().body, "the brief\n\na finding\n");

        ok(&mut s, &["set", "1", "--append-body", "another"]);
        assert_eq!(s.task(1).unwrap().body, "the brief\n\na finding\n\nanother");

        // Replacement and addition are different intents, not two spellings of
        // one, so asking for both at once is a parse error rather than a race.
        let e = err(&mut s, &["set", "1", "--body", "x", "--append-body", "y"]);
        assert!(e.contains("cannot be used with"), "{e}");
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
        // The proposal is inside the digest rather than carrying a row itself.
        assert!(out.contains("▲ 1 proposal awaiting triage (demo)"), "{out}");
        assert!(!out.contains("#1 triage"), "{out}");
        assert!(out.contains("#2 dispatch"), "{out}");
        assert!(out.contains("#3 do"), "{out}");
        assert!(out.contains("#4 answer"), "{out}");
    }

    /// The inbox ranks by attention price, not raw score (DESIGN.md §7): a
    /// question outranks a review worth more on paper, and the digest sits
    /// where its best proposal would.
    #[test]
    fn inbox_ranks_a_question_above_a_higher_scoring_review() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["weight", "demo", "3"]);
        // P0 review: 3×(8+2) = 30 raw, ÷1.4 = 21.4
        ok(
            &mut s,
            &[
                "add",
                "demo",
                "Big diff",
                "--priority",
                "0",
                "--state",
                "ready",
            ],
        );
        ok(&mut s, &["start", "1"]);
        ok(
            &mut s,
            &["done", "1", "--summary", "did it", "--branch", "b"],
        );
        // P2 question: 3×(2+4) = 18 raw, ÷0.8 = 22.5
        ok(&mut s, &["add", "demo", "Blocked", "--state", "ready"]);
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["ask", "2", "--question", "A or B?"]);

        let out = ok(&mut s, &["inbox"]);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].contains("#2 answer"), "{out}");
        assert!(
            lines[1].contains("#1 review PR") || lines[1].contains("#1 pr"),
            "{out}"
        );
    }

    /// The `[costs]` table re-prices the same queue (DESIGN.md §7).
    #[test]
    fn costs_overrides_in_voro_toml_change_the_inbox_order() {
        let mut s = store();
        let ctx = ctx_with_toml("[costs]\nreview = 0.5\n");
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["weight", "demo", "3"]);
        ok(
            &mut s,
            &[
                "add",
                "demo",
                "Big diff",
                "--priority",
                "0",
                "--state",
                "ready",
            ],
        );
        ok(&mut s, &["start", "1"]);
        ok(
            &mut s,
            &["done", "1", "--summary", "did it", "--branch", "b"],
        );
        ok(&mut s, &["add", "demo", "Blocked", "--state", "ready"]);
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["ask", "2", "--question", "A or B?"]);

        // Default costs put the question first; pricing review at 0.5 (30 ÷0.5
        // = 60 against the question's 22.5) puts the diff back on top.
        let out = run_with(&mut s, &["inbox"], &ctx).unwrap();
        assert!(out.lines().next().unwrap().contains("#1 "), "{out}");
    }

    /// The dispatch WIP gate (DESIGN.md §7): at the cap the inbox offers no
    /// more dispatches and says why.
    #[test]
    fn the_wip_gate_replaces_dispatch_rows_with_a_capacity_line() {
        let mut s = store();
        let ctx = ctx_with_toml("max_running = 1\n");
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "In flight", "--state", "ready"]);
        ok(&mut s, &["add", "demo", "Startable", "--state", "ready"]);

        // Nothing running yet: the startable row is offered.
        let out = run_with(&mut s, &["inbox"], &ctx).unwrap();
        assert!(out.contains("dispatch"), "{out}");
        assert!(!out.contains("at capacity"), "{out}");

        ok(&mut s, &["start", "1"]);
        let out = run_with(&mut s, &["inbox"], &ctx).unwrap();
        assert!(
            out.contains("⏸ dispatch at capacity (1/1 running)"),
            "{out}"
        );
        assert!(!out.contains("dispatch   "), "{out}");
    }

    /// `explain` shows the division the inbox ranked by (DESIGN.md §7).
    #[test]
    fn explain_shows_the_action_divisor_and_effective_score() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["weight", "demo", "3"]);
        ok(&mut s, &["add", "demo", "Startable", "--state", "ready"]);

        let out = ok(&mut s, &["explain", "1"]);
        assert!(out.contains("total"), "{out}");
        assert!(out.contains("dispatch"), "{out}");
        assert!(out.contains("cost ÷1"), "{out}");
        assert!(out.contains("effective"), "{out}");

        // A parked task asks nothing of the operator, so there is no action to
        // price and the effective line is absent.
        ok(&mut s, &["park", "1"]);
        let out = ok(&mut s, &["explain", "1"]);
        assert!(!out.contains("effective"), "{out}");
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

    /// `show` prints the completion summary of the cycle awaiting a verdict —
    /// on a first review as well as a rework (task #407) — and stops once the
    /// verdict has been given, when the summary is history.
    #[test]
    fn show_prints_the_completion_summary_under_review() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "A task", "--state", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--summary", "README.md: +2 lines"]);
        let out = ok(&mut s, &["show", "1"]);
        assert!(
            out.contains("completion summary:\nREADME.md: +2 lines"),
            "{out}"
        );

        // Handed off, the verdict is still pending, so the report still shows.
        ok(&mut s, &["wait", "1"]);
        assert!(ok(&mut s, &["show", "1"]).contains("README.md: +2 lines"));

        // Sent back: the rejected round's summary is not an answer to the
        // feedback, so nothing shows until the rework reports.
        ok(&mut s, &["reject", "1", "tests missing"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("completion summary:"));
        ok(&mut s, &["done", "1", "--summary", "added the tests"]);
        let out = ok(&mut s, &["show", "1"]);
        assert!(
            out.contains("response to the review feedback:\nadded the tests"),
            "{out}"
        );

        ok(&mut s, &["accept", "1"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("summary:"));
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

        // This task produced no code, so its summary is the deliverable and the
        // move is to accept it (DESIGN.md §6).
        ok(&mut s, &["done", "1", "--summary", "did it"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: accept"));

        ok(&mut s, &["accept", "1"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("next:"));
    }

    /// The `review` arm reads what the task carries (DESIGN.md §6): a branch
    /// makes the move `pr`, a tracked PR makes it `review PR`, and neither —
    /// an investigation, an audit — makes it `accept`, since `pr` could only
    /// refuse.
    #[test]
    fn show_names_the_review_verb_by_what_the_task_carries() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "An idea"]);
        ok(&mut s, &["triage", "1", "ready"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--summary", "did it"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: accept"));
        // and `pr` on it refuses, which is what makes `accept` the honest verb
        assert!(err(&mut s, &["pr", "1", "--yes"]).contains("branch"));

        ok(&mut s, &["set", "1", "--branch", "feat/idea"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: pr"));

        ok(&mut s, &["set", "1", "--pr", "acme/widget#42"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: review PR"));

        // clearing the branch does not undo a tracked PR: there is a diff to
        // read, so the verb stays `review PR`
        ok(&mut s, &["set", "1", "--no-branch"]);
        assert!(ok(&mut s, &["show", "1"]).contains("next: review PR"));
    }

    /// `list` shows state in its own column, so only `review` earns a next
    /// suffix — and where that suffix would read `pr`, the incomplete-report
    /// marker takes its place when the report is half-finished, as in the TUI
    /// browser.
    #[test]
    fn list_suffixes_review_rows_with_the_next_action() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let complete = review_task(&mut s, Some("feat/thing"), Some("did it"));
        let half = review_task(&mut s, Some("feat/other"), None);
        // no branch and a summary: a task whose report is the whole product
        let report = review_task(&mut s, None, Some("what I found"));
        ok(&mut s, &["add", "demo", "Startable", "--state", "ready"]);

        let out = ok(&mut s, &["list"]);
        let line = |id: i64| {
            out.lines()
                .find(|l| l.starts_with(&format!("#{id} ")))
                .unwrap_or_else(|| panic!("no row for #{id}: {out}"))
        };
        assert!(line(complete).ends_with("next: pr"), "{out}");
        assert!(line(half).ends_with("[incomplete report]"), "{out}");
        assert!(!line(half).contains("next:"), "{out}");
        assert!(line(report).ends_with("next: accept"), "{out}");
        assert!(!line(4).contains("next:"), "{out}");

        ok(&mut s, &["set", &complete.to_string(), "--pr", "acme/w#1"]);
        let out = ok(&mut s, &["list"]);
        assert!(out.contains("next: review PR"), "{out}");
    }

    /// A checkout with no remote has nowhere to open a pull request, so every
    /// surface that advertises a verb names the local review path instead of a
    /// `pr` that could only fail (DESIGN.md §8). Adding a remote puts `pr`
    /// back, and a tracked PR is untouched either way.
    #[test]
    fn a_review_row_in_a_remoteless_checkout_advertises_open() {
        let project = remoteless_checkout("remoteless");
        let git = |args: &[&str]| git_in(&project, args);

        let mut s = store();
        ok(
            &mut s,
            &["project", "add", "demo", project.to_str().unwrap()],
        );
        let id = review_task(&mut s, Some("feat/thing"), Some("did it"));

        let show = ok(&mut s, &["show", &id.to_string()]);
        assert!(show.contains("next: open"), "{show}");
        let list = ok(&mut s, &["list"]);
        assert!(list.contains("next: open"), "{list}");
        let inbox = ok(&mut s, &["inbox"]);
        assert!(inbox.contains(&format!("#{id} open ")), "{inbox}");

        // A tracked PR outranks the checkout: the diff already lives there.
        ok(&mut s, &["set", &id.to_string(), "--pr", "acme/w#1"]);
        let show = ok(&mut s, &["show", &id.to_string()]);
        assert!(show.contains("next: review PR"), "{show}");

        ok(&mut s, &["set", &id.to_string(), "--no-pr"]);
        git(&["remote", "add", "origin", "https://github.com/acme/w.git"]);
        let show = ok(&mut s, &["show", &id.to_string()]);
        assert!(show.contains("next: pr"), "{show}");
        let inbox = ok(&mut s, &["inbox"]);
        assert!(inbox.contains(&format!("#{id} pr ")), "{inbox}");

        std::fs::remove_dir_all(&project).ok();
    }

    /// A throwaway checkout with no remotes — the shape of a first project
    /// (DESIGN.md §8), which advertises `open` rather than `pr`.
    fn remoteless_checkout(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voro-cli-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        git_in(&path, &["init", "-q"]);
        path
    }

    fn git_in(path: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// The `[incomplete report]` marker withholds one verb and no more
    /// (DESIGN.md §8): `pr` builds its body from the summary the row is
    /// missing, so it cannot be recommended, but `open` reads a diff and needs
    /// no summary — a remoteless row therefore keeps `next: open` and wears the
    /// marker beside it rather than in its place.
    #[test]
    fn an_incomplete_report_keeps_the_local_review_verb() {
        let project = remoteless_checkout("incomplete");
        let mut s = store();
        ok(
            &mut s,
            &["project", "add", "demo", project.to_str().unwrap()],
        );
        let half = review_task(&mut s, Some("feat/thing"), None);

        let row = |out: &str| {
            out.lines()
                .find(|l| l.starts_with(&format!("#{half} ")))
                .unwrap_or_else(|| panic!("no row for #{half}: {out}"))
                .to_string()
        };
        let out = ok(&mut s, &["list"]);
        let line = row(&out);
        assert!(line.contains("next: open"), "{out}");
        assert!(line.ends_with("[incomplete report]"), "{out}");

        // Give the checkout somewhere to push and the row advertises `pr`,
        // which a half-written report cannot reach: the marker stands alone.
        git_in(
            &project,
            &["remote", "add", "origin", "https://github.com/acme/w.git"],
        );
        let out = ok(&mut s, &["list"]);
        let line = row(&out);
        assert!(!line.contains("next:"), "{out}");
        assert!(line.ends_with("[incomplete report]"), "{out}");

        std::fs::remove_dir_all(&project).ok();
    }

    /// `show` prints the marker as a sentence of its own rather than in the
    /// `next:` line's place, but it withholds the same one verb the single-line
    /// surfaces do (DESIGN.md §8): recommending `pr` above a line explaining
    /// that the summary it is built from is missing is the recommendation that
    /// could only fail, merely spelled out. The local verb still stands.
    #[test]
    fn show_withholds_the_pr_recommendation_from_a_half_written_report() {
        let project = remoteless_checkout("show-incomplete");
        let mut s = store();
        ok(
            &mut s,
            &["project", "add", "demo", project.to_str().unwrap()],
        );
        let half = review_task(&mut s, Some("feat/thing"), None);
        let id = half.to_string();
        let next = |out: &str| {
            out.lines()
                .find(|l| l.starts_with("next:"))
                .map(str::to_string)
        };

        let out = ok(&mut s, &["show", &id]);
        assert_eq!(next(&out).as_deref(), Some("next: open"), "{out}");
        assert!(out.contains("incomplete report:"), "{out}");

        git_in(
            &project,
            &["remote", "add", "origin", "https://github.com/acme/w.git"],
        );
        let out = ok(&mut s, &["show", &id]);
        assert_eq!(next(&out), None, "{out}");
        assert!(out.contains("incomplete report:"), "{out}");

        // Supplying the missing half restores the recommendation it withheld.
        ok(&mut s, &["set", &id, "--summary", "What it did"]);
        let out = ok(&mut s, &["show", &id]);
        assert_eq!(next(&out).as_deref(), Some("next: pr"), "{out}");
        assert!(!out.contains("incomplete report:"), "{out}");

        std::fs::remove_dir_all(&project).ok();
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

    fn propose(store: &mut Store, args: &[&str]) -> Result<String, String> {
        let cli = Cli::try_parse_from(std::iter::once("voro").chain(args.iter().copied()))
            .map_err(|e| e.to_string())?;
        let Verb::Propose(parsed) = cli.verb else {
            panic!("{args:?} is not a propose invocation");
        };
        propose_verb(store, parsed)
    }

    #[test]
    fn propose_lands_proposed() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let out = propose(&mut s, &["propose", "demo", "An idea"]).unwrap();
        assert!(out.contains("task 1 'An idea' proposed"), "{out}");
        assert!(ok(&mut s, &["show", "1"]).contains("#1 proposed"));
        assert!(s.deps_of(1).unwrap().is_empty());
    }

    #[test]
    fn propose_cannot_specify_a_state() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let e = propose(&mut s, &["propose", "demo", "An idea", "--state", "ready"]).unwrap_err();
        assert!(e.contains("proposed"), "{e}");
        assert!(ok(&mut s, &["list"]).is_empty());
    }

    #[test]
    fn propose_from_records_the_discovered_from_dep() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up", "--from", "1"]).unwrap();
        assert!(ok(&mut s, &["show", "2"]).contains("dep: discovered-from 1"));
        assert!(ok(&mut s, &["show", "2"]).contains("#2 proposed"));
    }

    /// The reported failure end to end: a task proposed from another and then
    /// gated on it keeps both edges, and `show` renders each.
    #[test]
    fn blocked_by_survives_an_existing_discovered_from_edge() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up", "--from", "1"]).unwrap();

        ok(&mut s, &["set", "2", "--blocked-by", "1"]);
        let shown = ok(&mut s, &["show", "2"]);
        assert!(shown.contains("dep: blocked by #1"), "{shown}");
        assert!(shown.contains("dep: discovered-from 1"), "{shown}");
    }

    #[test]
    fn unlink_drops_one_kind_of_a_pair_carrying_two() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up", "--from", "1"]).unwrap();
        ok(&mut s, &["set", "2", "--blocked-by", "1"]);

        let out = ok(&mut s, &["set", "2", "--unlink", "blocks:1"]);
        assert!(out.contains("task 2 no longer blocked by #1"), "{out}");
        let shown = ok(&mut s, &["show", "2"]);
        assert!(!shown.contains("blocked by #1"), "{shown}");
        assert!(shown.contains("dep: discovered-from 1"), "{shown}");

        // and the other way round: the provenance edge goes, the blocker stays
        ok(&mut s, &["set", "2", "--blocked-by", "1"]);
        let out = ok(&mut s, &["set", "2", "--unlink", "discovered-from:1"]);
        assert!(out.contains("task 2 no longer discovered-from #1"), "{out}");
        let shown = ok(&mut s, &["show", "2"]);
        assert!(shown.contains("dep: blocked by #1"), "{shown}");
        assert!(!shown.contains("discovered-from"), "{shown}");
    }

    #[test]
    fn unlink_reconciles_readiness_and_says_so() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(
            &mut s,
            &["add", "demo", "Closed blocker", "--state", "ready"],
        );
        ok(&mut s, &["add", "demo", "Open blocker", "--state", "ready"]);
        let out = ok(
            &mut s,
            &[
                "add",
                "demo",
                "Dependent",
                "--state",
                "ready",
                "--blocked-by",
                "1,2",
            ],
        );
        assert!(out.contains("(parked)"), "{out}");
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1"]);
        ok(&mut s, &["accept", "1"]);

        // dropping the one open blocker leaves a closed one behind it, so the
        // dependent is genuinely actionable and the store promotes it
        let out = ok(&mut s, &["set", "3", "--unlink", "blocks:2"]);
        assert!(out.contains("task 3 no longer blocked by #2"), "{out}");
        assert!(out.contains("#3 promoted to ready"), "{out}");
        assert!(out.contains("task 3 updated (ready)"), "{out}");
        assert!(ok(&mut s, &["list", "--state", "ready"]).contains("Dependent"));
    }

    #[test]
    fn unlinking_the_last_blocker_leaves_the_task_parked() {
        // A parked task with no blockers at all is deliberately deferred
        // (DESIGN.md §5), so emptying the blocker set — however it is spelled —
        // never promotes; `unpark` is the manual escape.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Blocker", "--state", "ready"]);
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

        let out = ok(&mut s, &["set", "2", "--unlink", "blocks:1"]);
        assert!(out.contains("task 2 updated (parked)"), "{out}");
        assert!(!out.contains("promoted"), "{out}");
        assert!(s.deps_of(2).unwrap().is_empty());
        assert!(ok(&mut s, &["unpark", "2"]).contains("ready"));
    }

    #[test]
    fn unlink_refuses_an_edge_that_is_not_there() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up", "--from", "1"]).unwrap();

        let e = err(&mut s, &["set", "2", "--unlink", "related:1"]);
        assert!(e.contains("#2 has no related dependency on #1"), "{e}");
        // the edge that *is* there survives the refusal
        assert!(ok(&mut s, &["show", "2"]).contains("dep: discovered-from 1"));

        let e = err(&mut s, &["set", "2", "--unlink", "sibling:1"]);
        assert!(e.contains("unknown dep kind 'sibling'"), "{e}");
        let e = err(&mut s, &["set", "2", "--unlink", "blocks"]);
        assert!(e.contains("unlink must be KIND:ID"), "{e}");
    }

    #[test]
    fn unlink_repeats_for_several_edges() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);
        propose(&mut s, &["propose", "demo", "Follow-up", "--from", "1"]).unwrap();
        ok(&mut s, &["set", "2", "--blocked-by", "1"]);

        let out = ok(
            &mut s,
            &[
                "set",
                "2",
                "--unlink",
                "blocks:1",
                "--unlink",
                "discovered-from:1",
            ],
        );
        assert!(out.contains("no longer blocked by #1"), "{out}");
        assert!(out.contains("no longer discovered-from #1"), "{out}");
        assert!(s.deps_of(2).unwrap().is_empty());
    }

    #[test]
    fn run_propose_without_from_links_nothing() {
        // `run` consults no environment: a bare `propose` never picks up an
        // ambient VORO_TASK_ID, so the discovered-from link comes only from an
        // explicit `--from`. This keeps the suite deterministic when it runs
        // inside a dispatched session (which exports VORO_TASK_ID).
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Source", "--state", "ready"]);

        run(
            &mut s,
            ["propose", "demo", "Orphan"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            &ctx(),
        )
        .unwrap();
        assert!(s.deps_of(2).unwrap().is_empty());
    }

    #[test]
    fn propose_rejects_an_unknown_source_without_creating() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        propose(&mut s, &["propose", "demo", "Orphan", "--from", "99"]).unwrap_err();
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

    // --- the deep flag (task #241) ---

    #[test]
    fn add_and_set_carry_the_deep_flag_and_show_displays_it() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);

        ok(&mut s, &["add", "demo", "Hard one", "--deep"]);
        assert!(ok(&mut s, &["show", "1"]).contains("deep:"));

        ok(&mut s, &["set", "1", "--no-deep"]);
        assert!(!ok(&mut s, &["show", "1"]).contains("deep:"));
        ok(&mut s, &["set", "1", "--deep"]);
        assert!(ok(&mut s, &["show", "1"]).contains("strongest model"));

        // an unrelated edit leaves the flag alone
        ok(&mut s, &["set", "1", "--priority", "0"]);
        assert!(ok(&mut s, &["show", "1"]).contains("deep:"));

        let e = err(&mut s, &["set", "1", "--deep", "--no-deep"]);
        assert!(e.contains("cannot be used with"), "{e}");
    }

    #[test]
    fn deep_is_refused_on_a_human_task_both_ways() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);

        let e = err(&mut s, &["add", "demo", "By hand", "--human", "--deep"]);
        assert!(e.contains("deep"), "{e}");

        ok(&mut s, &["add", "demo", "By hand", "--human"]);
        let e = err(&mut s, &["set", "1", "--deep"]);
        assert!(e.contains("deep"), "{e}");

        // ...and the flag blocks the human flag symmetrically
        ok(&mut s, &["add", "demo", "Agent work", "--deep"]);
        let e = err(&mut s, &["set", "2", "--human"]);
        assert!(e.contains("deep"), "{e}");
        ok(&mut s, &["set", "2", "--human", "--no-deep"]);
        assert!(ok(&mut s, &["show", "2"]).contains("human-only"));
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
        let (_, session) = s
            .record_dispatch(1, "claude", None, LivenessSource::Pid, None)
            .unwrap();
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
        assert!(out.contains("--unlink KIND:ID"), "{out}");
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

    // --- the project's viewer, and the static `pr` (DESIGN.md §8/§11a) ---

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
            message_grace: std::time::Duration::from_millis(300),
        }
    }

    fn run_with(store: &mut Store, args: &[&str], ctx: &DispatchCtx) -> Result<String, String> {
        run(store, args.iter().map(|s| s.to_string()).collect(), ctx)
    }

    #[test]
    fn project_viewer_sets_clears_and_shows() {
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        let out = ok(&mut s, &["project", "viewer", "demo", "zed"]);
        assert!(out.contains("default viewer -> zed"), "{out}");
        assert!(
            ok(&mut s, &["project", "list"]).contains("[viewer:zed]"),
            "list must show the viewer a project names"
        );

        // naming no viewer clears it back to the default, which earns no marker
        let out = ok(&mut s, &["project", "viewer", "demo"]);
        assert!(out.contains("zed -> default viewer"), "{out}");
        assert!(
            !ok(&mut s, &["project", "list"]).contains("[viewer"),
            "the default viewer earns no marker"
        );

        let e = err(&mut s, &["project", "viewer", "nope", "zed"]);
        assert!(e.contains("nope"), "{e}");
        assert!(ok(&mut s, &["help"]).contains("project viewer"), "help");
    }

    #[test]
    fn viewer_list_shows_viewers_flagging_the_default() {
        let mut s = store();
        let ctx = ctx_with_toml(
            "default_viewer = \"zed\"\n\n[viewers.zed]\ncmd = \"zed {path}\"\n\n\
             [viewers.difftool]\ncmd = \"git difftool -d\"\n",
        );
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(out.contains("* zed"), "{out}");
        assert!(out.contains("user override"), "{out}");
        assert!(out.contains("git difftool -d"), "{out}");

        // the anonymous [viewer] table shows as the default it resolves to
        let ctx = ctx_with_toml("[viewer]\ncmd = \"zed {path}\"\n");
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(out.contains("* [viewer]"), "{out}");

        // nothing configured: the built-ins are still listed, with provenance,
        // since each is a viewer `open` can run (#405)
        let ctx = ctx_with_toml("");
        let out = run_with(&mut s, &["viewer", "list"], &ctx).unwrap();
        assert!(!out.contains("no viewers configured"), "{out}");
        for name in ["code", "cursor", "zed"] {
            assert!(out.contains(name), "{out}");
        }
        assert!(out.contains("built-in"), "{out}");
        assert!(ok(&mut s, &["help"]).contains("viewer list"), "help");
    }

    /// `pr` is statically the GitHub medium (DESIGN.md §8): on a checkout that
    /// cannot take a pull request it errors pointing at `voro open`, and the
    /// viewer the project names does not redirect it. The
    /// viewer would leave a marker behind, so its absence is the assertion.
    #[test]
    fn pr_on_a_non_github_checkout_errors_pointing_at_open() {
        let mut s = store();
        let ctx = ctx_with_toml("[viewers.marker]\ncmd = \"touch {path}/opened.marker\"\n");
        let project_dir = ctx.db_path.parent().unwrap().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();

        ok(
            &mut s,
            &["project", "add", "demo", project_dir.to_str().unwrap()],
        );
        ok(&mut s, &["project", "viewer", "demo", "marker"]);
        // PR-ready in every way but the checkout, so the refusal can only be
        // about the medium.
        let id = review_task(&mut s, Some("feat/thing"), Some("did it"));

        let e = run_with(&mut s, &["pr", &id.to_string(), "--yes"], &ctx).unwrap_err();
        assert!(e.contains("voro open"), "{e}");
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !project_dir.join("opened.marker").exists(),
            "pr must not fall back to the project's viewer"
        );
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
        ok(&mut s, &["project", "viewer", "demo", "zed"]);
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
        // Only the summary half carries the shape note.
        assert!(!out.contains("PR description"), "{out}");

        // branch given, no summary: the note names summary only, and says what
        // shape the half it is asking for takes (DESIGN.md §8).
        ok(&mut s, &["add", "demo", "T2", "--state", "ready"]);
        ok(&mut s, &["set", "2", "--branch", "feat/x"]);
        ok(&mut s, &["start", "2"]);
        let out = ok(&mut s, &["done", "2"]);
        assert!(out.contains("note: no summary recorded"), "{out}");
        assert!(out.contains("the summary is the PR description"), "{out}");
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
        let shown = ok(&mut s, &["show", "1"]);
        assert!(
            shown.contains("incomplete report: a branch is recorded but no summary"),
            "{shown}"
        );
        // The nudge names the shape of the half it is asking for.
        assert!(shown.contains("the PR description"), "{shown}");
    }

    #[test]
    fn a_complete_report_is_not_flagged_incomplete() {
        // Both halves present: a complete report, no anomaly. A planning task
        // with neither is likewise not flagged, and so is the no-code report —
        // an investigation whose summary is the whole deliverable.
        let mut s = store();
        ok(&mut s, &["project", "add", "demo", "/tmp"]);
        ok(&mut s, &["add", "demo", "Coding", "--state", "ready"]);
        ok(&mut s, &["set", "1", "--branch", "feat/x"]);
        ok(&mut s, &["start", "1"]);
        ok(&mut s, &["done", "1", "--summary", "did it"]);
        ok(&mut s, &["add", "demo", "Planning", "--state", "ready"]);
        ok(&mut s, &["start", "2"]);
        ok(&mut s, &["done", "2"]);
        ok(&mut s, &["add", "demo", "Audit", "--state", "ready"]);
        ok(&mut s, &["start", "3"]);
        ok(&mut s, &["done", "3", "--summary", "already fixed"]);

        assert!(!ok(&mut s, &["list"]).contains("[incomplete report]"));
        assert!(!ok(&mut s, &["show", "1"]).contains("incomplete report"));
        assert!(!ok(&mut s, &["show", "2"]).contains("incomplete report"));
        assert!(!ok(&mut s, &["show", "3"]).contains("incomplete report"));
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

    /// Both verbs that record one say what a summary is: the description `pr`
    /// opens the pull request from (DESIGN.md §8).
    #[test]
    fn help_describes_a_summary_as_the_pr_description() {
        let mut s = store();
        // The help is hand-wrapped into its column, so match on the prose
        // rather than on where the lines happen to break.
        let out = ok(&mut s, &["help"]);
        let flowed = out.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flowed.contains("write it as a PR description"), "{out}");
        assert!(
            flowed.contains("the PR description `pr` opens the pull request from"),
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
        assert!(out.contains("base w×(p+s+u)    16.0"), "{out}");
        assert!(out.contains("state            ready  (bonus +0)"), "{out}");
        assert!(out.contains("blocks              ×0  (bonus +0"), "{out}");

        // a task another one waits on shows the dependent count and its bonus
        ok(
            &mut s,
            &[
                "add", "demo", "blocker", "--state", "ready", "--blocks", "1",
            ],
        );
        let out = ok(&mut s, &["explain", "2"]);
        assert!(out.contains("blocks              ×1  (bonus +1"), "{out}");
        assert!(out.contains("base w×(p+s+u)     6.0"), "{out}");
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
            message_grace: std::time::Duration::from_millis(300),
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
            message_grace: std::time::Duration::from_millis(300),
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
                repo_id: None,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
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

    #[test]
    fn a_doc_links_tasks_across_projects_and_list_answers_which_derive_from_it() {
        // The acceptance case (task #267): register a plan, link tasks in
        // three projects to it, and ask which tasks came from that plan.
        let mut s = store();
        ok(&mut s, &["project", "add", "augere", "/tmp/augere"]);
        ok(&mut s, &["project", "add", "clachdev", "/tmp/clachdev"]);
        ok(&mut s, &["project", "add", "mote", "/tmp/mote"]);

        let out = ok(
            &mut s,
            &[
                "doc",
                "add",
                "augere",
                "/tmp/augere/docs/strategy.md",
                "--title",
                "Strategy 2026",
            ],
        );
        // An absolute path inside the checkout is stored relative to it, and
        // the echo says so rather than leaving the operator guessing.
        assert!(out.contains("Strategy 2026"), "{out}");
        assert!(out.contains("as docs/strategy.md"), "{out}");

        ok(&mut s, &["add", "augere", "fleet plan"]);
        ok(&mut s, &["add", "clachdev", "blog post"]);
        ok(&mut s, &["add", "mote", "milestone M0"]);
        let out = ok(&mut s, &["doc", "link", "1", "1", "2", "3"]);
        assert!(out.contains("#1 linked"), "{out}");
        assert!(out.contains("#3 linked"), "{out}");
        // Re-linking is heard as a no-op, not an error.
        assert!(ok(&mut s, &["doc", "link", "1", "1"]).contains("already linked"));

        let derived = ok(&mut s, &["list", "--doc", "1"]);
        for title in ["fleet plan", "blog post", "milestone M0"] {
            assert!(derived.contains(title), "{derived}");
        }
        // ...and a task from no plan is not swept in.
        ok(&mut s, &["add", "mote", "unrelated"]);
        assert!(!ok(&mut s, &["list", "--doc", "1"]).contains("unrelated"));

        // A doc is nameable by its stored location as well as its id.
        assert!(ok(&mut s, &["list", "--doc", "docs/strategy.md"]).contains("blog post"));
    }

    #[test]
    fn doc_show_rolls_up_the_states_of_every_task_linked_to_a_plan() {
        let mut s = store();
        ok(&mut s, &["project", "add", "mote", "/tmp/mote"]);
        ok(&mut s, &["doc", "add", "mote", "docs/design/fleet.md"]);
        let empty = ok(&mut s, &["doc", "show", "1"]);
        assert!(empty.contains("no tasks linked yet"), "{empty}");
        assert!(
            empty.contains("resolves to: /tmp/mote/docs/design/fleet.md"),
            "{empty}"
        );

        ok(
            &mut s,
            &["add", "mote", "M0", "--state", "ready", "--doc", "1"],
        );
        ok(
            &mut s,
            &["add", "mote", "M1", "--state", "ready", "--doc", "1"],
        );
        ok(&mut s, &["add", "mote", "M2", "--doc", "1"]);
        ok(&mut s, &["start", "1"]);

        let out = ok(&mut s, &["doc", "show", "1"]);
        assert!(out.contains("tasks (3)"), "{out}");
        assert!(out.contains("1 proposed"), "{out}");
        assert!(out.contains("1 ready"), "{out}");
        assert!(out.contains("1 running"), "{out}");
        assert!(
            ok(&mut s, &["doc", "list"]).contains("[3 task(s)]"),
            "{out}"
        );
    }

    #[test]
    fn show_renders_a_task_s_documents_at_their_resolved_locations() {
        let mut s = store();
        ok(&mut s, &["project", "add", "voro", "/tmp/voro"]);
        ok(
            &mut s,
            &["doc", "add", "voro", "docs/DESIGN.md", "--title", "Design"],
        );
        ok(&mut s, &["doc", "add", "voro", "https://example.com/rfc"]);
        ok(&mut s, &["add", "voro", "implement docs"]);
        ok(&mut s, &["doc", "link", "1", "1"]);

        let out = ok(&mut s, &["show", "1"]);
        assert!(
            out.contains("doc: Design — /tmp/voro/docs/DESIGN.md"),
            "{out}"
        );
        // An untitled URL reads as itself, and an unlinked doc is not shown.
        assert!(!out.contains("example.com"), "{out}");

        ok(&mut s, &["set", "1", "--doc", "1,2"]);
        let out = ok(&mut s, &["show", "1"]);
        assert!(out.contains("doc: https://example.com/rfc"), "{out}");
    }

    #[test]
    fn set_doc_replaces_the_list_and_no_doc_clears_it() {
        let mut s = store();
        ok(&mut s, &["project", "add", "voro", "/tmp/voro"]);
        ok(&mut s, &["doc", "add", "voro", "docs/a.md"]);
        ok(&mut s, &["doc", "add", "voro", "docs/b.md"]);
        ok(&mut s, &["add", "voro", "t", "--doc", "1"]);

        // Replace, matching --blocked-by, so the flag can drop a link too.
        let out = ok(&mut s, &["set", "1", "--doc", "2"]);
        assert!(out.contains("docs: 2 'docs/b.md'"), "{out}");
        // The link is gone, though the append-only event trail still records
        // that it was made and dropped.
        let shown = ok(&mut s, &["show", "1"]);
        assert!(!shown.contains("doc: /tmp/voro/docs/a.md"), "{shown}");
        assert!(shown.contains("doc: /tmp/voro/docs/b.md"), "{shown}");
        assert!(shown.contains("doc-unlinked"), "{shown}");

        let out = ok(&mut s, &["set", "1", "--no-doc"]);
        assert!(out.contains("no documents linked"), "{out}");
        assert!(!ok(&mut s, &["show", "1"]).contains("doc:"));

        // `doc unlink` is the subtractive spelling that names no other doc.
        ok(&mut s, &["set", "1", "--doc", "1,2"]);
        assert!(ok(&mut s, &["doc", "unlink", "1", "1"]).contains("unlinked from"));
        assert!(ok(&mut s, &["doc", "unlink", "1", "1"]).contains("not linked to"));
        assert!(ok(&mut s, &["show", "1"]).contains("docs/b.md"));
    }

    #[test]
    fn doc_remove_unlinks_its_tasks_and_an_unknown_doc_says_what_to_do() {
        let mut s = store();
        ok(&mut s, &["project", "add", "voro", "/tmp/voro"]);
        ok(&mut s, &["doc", "add", "voro", "docs/a.md"]);
        ok(&mut s, &["add", "voro", "t", "--doc", "1"]);

        let out = ok(&mut s, &["doc", "remove", "1"]);
        assert!(out.contains("unlinked from 1 task(s)"), "{out}");
        assert!(!ok(&mut s, &["show", "1"]).contains("doc:"));
        assert!(ok(&mut s, &["doc", "list"]).is_empty());

        let refusal = err(&mut s, &["doc", "show", "docs/gone.md"]);
        assert!(
            refusal.contains("no document at 'docs/gone.md'"),
            "{refusal}"
        );
        assert!(err(&mut s, &["doc", "show", "9"]).contains("not found"));
    }

    #[test]
    fn a_location_two_projects_register_is_resolved_by_id_not_guessed_at() {
        let mut s = store();
        ok(&mut s, &["project", "add", "one", "/tmp/one"]);
        ok(&mut s, &["project", "add", "two", "/tmp/two"]);
        ok(&mut s, &["doc", "add", "one", "docs/plan.md"]);
        ok(&mut s, &["doc", "add", "two", "docs/plan.md"]);
        ok(&mut s, &["add", "one", "t"]);

        let refusal = err(&mut s, &["set", "1", "--doc", "docs/plan.md"]);
        assert!(refusal.contains("more than one project"), "{refusal}");
        assert!(refusal.contains("1, 2"), "{refusal}");
        ok(&mut s, &["set", "1", "--doc", "2"]);
        assert!(ok(&mut s, &["show", "1"]).contains("/tmp/two/docs/plan.md"));
    }

    #[test]
    fn doc_add_reads_a_relative_path_from_the_repo_it_names() {
        let mut s = store();
        ok(&mut s, &["project", "add", "odm", "/tmp/odm"]);
        ok(&mut s, &["repo", "add", "odm", "oats", "/tmp/oats"]);
        ok(
            &mut s,
            &["doc", "add", "odm", "docs/plan.md", "--repo", "oats"],
        );
        ok(&mut s, &["add", "odm", "t", "--doc", "1"]);
        assert!(ok(&mut s, &["show", "1"]).contains("/tmp/oats/docs/plan.md"));

        // A repo of another project cannot be the reader.
        ok(&mut s, &["project", "add", "voro", "/tmp/voro"]);
        assert!(err(&mut s, &["doc", "add", "voro", "x.md", "--repo", "oats"]).contains("no repo"));
    }
}
