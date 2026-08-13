//! Dispatching a ready (or stalled) task to a headless agent session (DESIGN.md
//! §8): the I/O half of dispatch — resolving the agent, guarding that the
//! checkout is a git repository, writing the prompt, and spawning the process
//! detached — kept out of voro-core, which stays pure of process and filesystem
//! I/O. The atomic state-plus-session write is voro-core's `Store::record_dispatch`.
//!
//! For agents that define a `sessions` verb (task #75), dispatch additionally
//! captures the agent's own session reference after launch — by polling the
//! `sessions` listing for a session started in this project since the spawn,
//! falling back to the `backgrounded · <id>` line launchers print into the
//! log — and records it on the session row for later attach/resume.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use voro_core::{
    AgentsConfig, Launch, LaunchSpec, ResolvedAgent, ReviewDiff, Store, TaskState,
    VIEWER_BASE_PLACEHOLDER, VIEWER_BRANCH_PLACEHOLDER, VIEWER_PATH_PLACEHOLDER, render,
};

/// Command assembly owns its own quoting, so this is voro-core's; re-exported
/// here because the TUI reaches it through this module.
pub(crate) use voro_core::shell_quote;

/// Prepended to every dispatched prompt so the agent learns the return-path
/// verbs (DESIGN.md §8). [`render_preamble`] fills the `{task_id}` and `{db}`
/// placeholders literally rather than relying on inherited `VORO_TASK_ID`/
/// `VORO_DB` env vars, which do not survive a launcher that hands the session
/// to a supervisor daemon (e.g. `claude --bg`).
const RETURN_PATH_PREAMBLE_TEMPLATE: &str = "\
<!-- Voro dispatch: how to report back -->
You are an agent dispatched by Voro on task {task_id}. Voro tracks this task in a
local SQLite database; report progress through these CLI verbs rather than
editing that database yourself:

    voro ask {task_id}{db} --question \"...\"     # blocked on a human decision (-> needs-input)
    voro resume {task_id}{db}                     # question answered in this session, carry on (-> running)
    voro done {task_id}{db} --summary \"...\"      # work finished, await review (-> review)
    voro propose <project> \"<title>\"{db} --from {task_id} --body-file <path>   # file a follow-up task (-> proposed)

Run these commands exactly as shown: they name task {task_id} explicitly, so they
work from anywhere without any environment variable being set. After you `ask`,
the operator answers right here in this session — so when your question has been
answered, run `voro resume {task_id}` before continuing, to move the task back to
running (Voro records no answer text; the exchange is already in this transcript).
The `--from {task_id}` on `propose` links each follow-up discovered-from this
task; drop it for a proposal that stands on its own. Finish
with your work committed on a branch and a PR-ready `--summary` on `done` — what
changed, why, and how you verified it — since `voro pr` opens the pull request
straight from that summary. Never modify the database with raw SQL, which would
bypass the state machine and event log.{branch}{docs}{rework}

---

";

/// Shared by the rework message and the redispatch preamble so the
/// answer-the-feedback instruction cannot drift (DESIGN.md §8). A re-review is
/// narrowed to the diff since the rejected revision, so the summary is what
/// carries the operator from each of their points to the change that answers
/// it; without it they are back to rediscovering the rework from the diff.
const REWORK_SUMMARY_SENTENCE: &str = "Report with `voro done {task_id}{db} --summary \"...\"` whose summary answers that \
     feedback point by point — one item per point, saying what you changed or \
     why you did not. The operator re-reviews only the diff since the revision \
     they rejected, so that summary is what connects your changes to their \
     points.";

/// The `{rework}` block for a task that has already been through review and was
/// sent back (DESIGN.md §8). It rides the preamble because a redispatch is the
/// path where the feedback reaches a *fresh* session — one that has none of the
/// rejected round's context — and the body's `## Feedback` section is where the
/// transition put the points themselves.
const REWORK_TEMPLATE: &str = "\n\n\
This task has been through review once already and was sent back: the `## Feedback`
section of the body below carries the operator's points. {instruction}";

/// One rejection said into a session that is still open (DESIGN.md §8). Unlike
/// the preamble block this carries the feedback itself, since the session has
/// the task body from its original dispatch and will not re-read it.
const REWORK_MESSAGE_TEMPLATE: &str = "\
<!-- Voro: task {task_id} was reviewed and sent back -->
The work you reported on task {task_id} has been reviewed. It was not accepted;
the operator's feedback follows.

{feedback}

{instruction}";

/// The `{docs}` block for a task linked to plan or design documents (DESIGN.md
/// §3/§8). Each is named at its resolved location — absolute for a path, since
/// a linked doc may live in another project's checkout entirely — so the agent
/// reads the plan rather than rediscovering it from hints in the body.
const LINKED_DOCS_TEMPLATE: &str = "\n\n\
This task derives from the document(s) below. Read them before the task body:
they carry the plan this task implements, and the body assumes them.

{list}";

/// Shared by both branch blocks below so the early-registration instruction
/// cannot drift (task #96). `{name}` is the assigned branch or the `<name>` the
/// agent will choose; `{task_id}`/`{db}` keep the command copy-pasteable under
/// launch styles that drop the environment.
const BRANCH_REGISTER_SENTENCE: &str = "register it with `voro set {task_id}{db} --branch {name}` as you do, so Voro \
     tracks the real branch while the task runs";

/// Shared by both branch blocks so the stale-branch instruction cannot drift:
/// if the base moved while the task sat in review, the agent resolves the
/// conflict itself, inside its own worktree, touching neither the primary
/// checkout nor the remote. Fetching only updates remote-tracking refs, so it
/// leaves the trust model — dispatched agents cannot push — intact (DESIGN.md §8).
const BRANCH_REBASE_SENTENCE: &str = "If the branch conflicts with the project's base branch (typically because the \
     base moved while the task sat in review), run `git fetch origin <base>` from \
     inside the worktree and rebase or merge onto `origin/<base>`, resolving the \
     conflicts there — never modify the primary checkout, and never push.";

/// Shared by both branch blocks so the isolation instruction cannot drift. An
/// agent whose harness owns worktree creation must use that mechanism rather
/// than `git worktree add`: Claude Code refuses file edits until its own
/// `EnterWorktree` tool has run, and pointing that tool at an already-made
/// worktree raises an approval prompt no headless session can answer. Since the
/// harness names that branch itself, the sentence ends with the rename that puts
/// the work on the branch Voro tracks. `{name}` is the assigned branch or the
/// `<name>` the agent will choose.
const BRANCH_ISOLATE_SENTENCE: &str = "Isolate your work in a worktree, through your harness's own mechanism for that \
     where it has one — in Claude Code that is the `EnterWorktree` tool, which \
     creates and enters a worktree under `.claude/worktrees/` unprompted, on a \
     branch it names itself, so run `git switch -c {name}` inside the worktree \
     before you commit. Lacking such a mechanism, make the throwaway worktree \
     yourself: `git worktree add <path> -b {name}`.";

/// The `{branch}` block for a task that carries an intended git branch (task
/// #81): the agent is told the name, to do its work in a throwaway worktree on
/// it rather than the primary checkout via [`BRANCH_ISOLATE_SENTENCE`], to
/// register it early via [`BRANCH_REGISTER_SENTENCE`], to confirm it at
/// completion, and to self-serve a rebase onto a moved base via
/// [`BRANCH_REBASE_SENTENCE`].
const ASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
This task is assigned the git branch `{name}`. You are spawned in the project
checkout — never modify it. {isolate} Voro runs no git, so the branch and its
worktree are yours to make; {register}. Confirm the branch your work landed on
with `voro done {task_id}{db} --branch {name}`. {rebase}";

/// The `{branch}` block when no branch is assigned: the agent picks its own
/// name, isolates the same way via [`BRANCH_ISOLATE_SENTENCE`], and must still
/// register the name early via [`BRANCH_REGISTER_SENTENCE`], so Voro learns the
/// branch while the task runs rather than only at `done`, and self-serve a
/// rebase onto a moved base via [`BRANCH_REBASE_SENTENCE`].
const UNASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
Pick a git branch for this work. You are spawned in the project checkout — never
modify it. {isolate} Voro runs no git, so the branch and its worktree are yours
to make; {register}; `<name>` is the name you choose. {rebase}";

/// Rendered per planning session (DESIGN.md §8) and written as the whole
/// prompt — unlike dispatch there is no task body to prepend it to, because
/// producing one is the session's job. `{project}` is the project's name,
/// `{project_arg}` the same name shell-quoted for the `voro add` line, and
/// `{db}` the same conditional `--db` flag the return-path preamble renders,
/// for the same reason: the command must name its database literally.
const PLANNING_PROMPT_TEMPLATE: &str = "\
<!-- Voro planning session: the deliverable is a task, not a PR -->
You are an agent launched by Voro to help the operator plan a new task for the
project `{project}`. This is an interactive planning conversation: ask the
operator what they want, and interview them until the task is well defined —
scope, approach where it is settled, and what done looks like. Do not modify
the project's files; the checkout is there to read, so the task can name real
files and code.

Write the task body as a self-contained dispatchable prompt: the agent that
picks it up later gets no other context, so name the relevant files, spell out
the decisions already made, and give concrete acceptance criteria.

When the operator confirms the draft, write the body to a file outside the
checkout and create the task with:

    voro add {project_arg} \"<title>\"{db} --body-file <path>

Add `--priority <0-3>` if the operator wants something other than the default
P2. The task is created in `proposed` and the operator triages it from the
queue, so do not pass a state. If the operator decides against creating a
task, end the session without creating one — that is a no-op, not a failure.
Never modify Voro's database with raw SQL, which would bypass the state
machine and event log.
";

/// The note-driven refine (DESIGN.md §6): a headless agent rewrites a proposed
/// task's body to honour the operator's one-line note, and applies it through
/// `voro set --body-file` — the same "the CLI is the agent's interface" shape as
/// dispatch and planning. `{seed}` carries the task as it stands plus the
/// context of the task it was discovered from; `{note}` is the operator's brief.
const REFINE_PROMPT_TEMPLATE: &str = "\
<!-- Voro refine: the deliverable is a rewritten task body -->
You are an agent launched by Voro to rewrite the body of proposed task {task_id}
so that it reads as a dispatchable prompt: the agent that picks the task up later
gets no other context, so name the relevant files, spell out the decisions
already made, and give concrete acceptance criteria. The checkout you are running
in is there to read, so the body can name real files and code. Do not modify the
project's files, and do not do the task itself — writing its brief is the job.

The operator's note is what they want fixed. It is the brief; honour it:

    {note}

{seed}
When the rewrite is ready, write it to a file outside the checkout and apply it
with exactly this command:

    voro set {task_id}{db} --body-file <path> [--title \"...\"]

Add `--title` when the note asks for a retitle or the rewrite leaves the
current title inaccurate, and put it on that same command, because
a title-only `voro set` replaces no body, so it concludes no round and would
leave this one hanging.

That one command is the only change you may make. The task stays `proposed` so
the operator re-triages the improved version, and everything else stays as it
is: do not change its state, priority, or dependencies — no `voro triage`, no
`--priority`, no `--blocked-by`, no `voro add` or `voro propose` — and never
modify Voro's database with raw SQL, which would bypass the state machine and
event log. Rewrite the body and stop.
";

/// The interactive refine (DESIGN.md §6): the planning harness pointed at a task
/// that already exists. Same conversation as [`PLANNING_PROMPT_TEMPLATE`], but
/// seeded with the current body and ending in `set --body-file` rather than
/// `add`, so it edits in place and creates nothing.
const REFINE_PLAN_PROMPT_TEMPLATE: &str = "\
<!-- Voro refine session: the deliverable is a rewritten task body -->
You are an agent launched by Voro to help the operator rework the body of
proposed task {task_id}. This is an interactive conversation: ask the operator
what is wrong with the task as it stands, and interview them until it is well
defined — scope, approach where it is settled, and what done looks like. Do not
modify the project's files; the checkout is there to read, so the task can name
real files and code. Do not do the task itself — writing its brief is the job.

{seed}
Write the body as a self-contained dispatchable prompt: the agent that picks it
up later gets no other context, so name the relevant files, spell out the
decisions already made, and give concrete acceptance criteria.

When the operator confirms the rewrite, write the body to a file outside the
checkout and apply it with:

    voro set {task_id}{db} --body-file <path> [--title \"...\"]

Add `--title` when the operator wants the task retitled or the rewrite leaves
the current title inaccurate, and put it on that same command, because
a title-only `voro set` replaces no body, so it concludes no round and would
leave this one hanging.

That one command is the only change to make. The task stays `proposed` so the
operator re-triages the improved version, and everything else stays as it is:
do not change its state, priority, or dependencies, and do not create a new
task — this edits the one that exists. If the operator decides against changing
anything, end the session without applying a body — that is a no-op, not a
failure. Never modify Voro's database with raw SQL, which would bypass the state
machine and event log.
";

/// How often the session-ref capture re-polls the agent's `sessions` command
/// while waiting for the freshly-launched session to appear in the listing.
const REF_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Clock slack when matching a listing entry's `startedAt` against the spawn
/// time: the agent stamps its session with its own clock, which may sit a
/// beat behind the timestamp taken here just before the spawn.
const SPAWN_CLOCK_SLACK_MS: i64 = 2000;

/// The ` --db <path>` flag every rendered verb carries when the database in play
/// is not the operator's store — empty otherwise, since that is what a bare
/// verb resolves to (DESIGN.md §8). The agent a dispatch launches runs the
/// installed `voro` from `PATH`, so the comparison is against the production
/// path (§5) rather than whichever store this binary defaults to: a dispatch
/// from the dev store renders the flag and keeps the return path on it.
fn db_flag(db_path: &Path) -> String {
    if db_path == Store::production_db_path() {
        String::new()
    } else {
        format!(" --db {}", shell_quote(db_path))
    }
}

/// Fill [`RETURN_PATH_PREAMBLE_TEMPLATE`] for a concrete dispatch: the literal
/// task id in every verb, plus a `--db <path>` flag only when the dispatching
/// database is not the default one the verbs resolve to on their own. `docs` is
/// the task's linked documents as `(label, resolved location)` pairs, already
/// resolved by the store so this renders paths rather than joining them.
fn render_preamble(
    task_id: i64,
    db_path: &Path,
    branch: Option<&str>,
    docs: &[(String, String)],
    rework: bool,
) -> String {
    let db_flag = db_flag(db_path);
    let task_id = task_id.to_string();
    let name = branch.unwrap_or("<name>");
    // Rendered innermost-first, each block finished before it is bound into the
    // next: a single pass emits a value verbatim, so a branch name spelling
    // `{task_id}` survives composition rather than being rewritten by the outer
    // template's own bindings.
    let leaf = [
        ("{name}", name),
        ("{task_id}", task_id.as_str()),
        ("{db}", db_flag.as_str()),
    ];
    let register = render(BRANCH_REGISTER_SENTENCE, &leaf);
    let isolate = render(BRANCH_ISOLATE_SENTENCE, &leaf);
    let branch_template = match branch {
        Some(_) => ASSIGNED_BRANCH_TEMPLATE,
        None => UNASSIGNED_BRANCH_TEMPLATE,
    };
    let branch_block = render(
        branch_template,
        &[
            ("{isolate}", isolate.as_str()),
            ("{register}", register.as_str()),
            ("{rebase}", BRANCH_REBASE_SENTENCE),
            ("{name}", name),
            ("{task_id}", task_id.as_str()),
            ("{db}", db_flag.as_str()),
        ],
    );
    // A task with no linked document renders no block at all, so an unlinked
    // dispatch's prompt is byte-for-byte what it was before docs existed.
    let docs_block = if docs.is_empty() {
        String::new()
    } else {
        let list = docs
            .iter()
            .map(|(label, location)| {
                if label == location {
                    format!("    {location}")
                } else {
                    format!("    {location}  — {label}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        render(LINKED_DOCS_TEMPLATE, &[("{list}", list.as_str())])
    };
    // A task nobody has rejected renders no block, so a first dispatch's prompt
    // is byte-for-byte what it was before delta re-review existed.
    let rework_block = if rework {
        render(
            REWORK_TEMPLATE,
            &[(
                "{instruction}",
                rework_instruction(&task_id, &db_flag).as_str(),
            )],
        )
    } else {
        String::new()
    };
    render(
        RETURN_PATH_PREAMBLE_TEMPLATE,
        &[
            ("{branch}", branch_block.as_str()),
            ("{docs}", docs_block.as_str()),
            ("{rework}", rework_block.as_str()),
            ("{task_id}", task_id.as_str()),
            ("{db}", db_flag.as_str()),
        ],
    )
}

/// [`REWORK_SUMMARY_SENTENCE`] filled for a concrete task, so both the message
/// and the preamble name the same copy-pasteable `done` line.
fn rework_instruction(task_id: &str, db_flag: &str) -> String {
    render(
        REWORK_SUMMARY_SENTENCE,
        &[("{task_id}", task_id), ("{db}", db_flag)],
    )
}

/// The rejection as the agent's still-open session receives it (DESIGN.md §8):
/// the operator's feedback, and the instruction to answer it point by point in
/// the completion summary. The feedback is bound last-and-verbatim through
/// [`render`], so a point that itself spells `{task_id}` reaches the agent as
/// the operator wrote it.
pub fn rework_message(task_id: i64, db_path: &Path, feedback: &str) -> String {
    let db_flag = db_flag(db_path);
    let task_id = task_id.to_string();
    let instruction = rework_instruction(&task_id, &db_flag);
    render(
        REWORK_MESSAGE_TEMPLATE,
        &[
            ("{feedback}", feedback.trim()),
            ("{instruction}", instruction.as_str()),
            ("{task_id}", task_id.as_str()),
            ("{db}", db_flag.as_str()),
        ],
    )
}

/// Fill [`PLANNING_PROMPT_TEMPLATE`] for a concrete project: the `voro add`
/// line carries the shell-quoted project name and, exactly as
/// [`render_preamble`] does, a `--db` flag only when the database is not the
/// default one the verb resolves to unaided.
fn render_planning_prompt(project: &str, db_path: &Path) -> String {
    render(
        PLANNING_PROMPT_TEMPLATE,
        &[
            ("{project_arg}", shell_quote(Path::new(project)).as_str()),
            ("{project}", project),
            ("{db}", db_flag(db_path).as_str()),
        ],
    )
}

/// The seed context both refine flavours open with: the task as it stands, the
/// plan documents it is linked to, and the body and completion summary of the
/// task it was discovered from. Each is context a sloppy proposal is usually
/// missing (DESIGN.md §6), so it is pulled in rather than left for the agent to
/// hunt — the same argument that puts linked docs in a dispatch preamble, and
/// sharper here, since a body rewritten against the plan is the whole job.
fn refine_seed(store: &Store, task: &voro_core::Task) -> Result<String, String> {
    let mut seed = format!(
        "## The task as it stands\n\nTitle: {}\n\nBody:\n\n{}\n\n",
        task.title,
        if task.body.trim().is_empty() {
            "(empty)"
        } else {
            task.body.trim_end()
        }
    );
    let docs = store.docs_for_task(task.id).map_err(|e| e.to_string())?;
    if !docs.is_empty() {
        seed.push_str(
            "## Linked documents\n\nRead these before rewriting: they carry the plan this task \
             implements, and the body should assume them rather than restate them.\n\n",
        );
        for doc in &docs {
            let location = store.resolve_doc(doc).map_err(|e| e.to_string())?;
            seed.push_str(&format!("- {} — {}\n", doc.label(), location));
        }
        seed.push('\n');
    }
    if let Some(parent) = store.discovered_from(task.id).map_err(|e| e.to_string())? {
        seed.push_str(&format!(
            "## Discovered from task {} — {}\n\nThat task's body:\n\n{}\n\n",
            parent.id,
            parent.title,
            if parent.body.trim().is_empty() {
                "(empty)"
            } else {
                parent.body.trim_end()
            }
        ));
        if let Some(summary) = store.latest_summary(parent.id).map_err(|e| e.to_string())? {
            seed.push_str(&format!(
                "That task's completion summary:\n\n{}\n\n",
                summary.trim_end()
            ));
        }
    }
    Ok(seed)
}

/// Fill a refine template for a concrete task: the literal task id in the `set`
/// command, the same conditional `--db` flag the other prompts render, the seed
/// context, and — for the note-driven flavour — the operator's note. The seed
/// carries the task's own body, so it goes through the single-pass renderer:
/// a body that discusses `{task_id}` or `{db}` must reach the agent as written,
/// and rewriting the subject of a rewrite is the one thing this must not do.
fn render_refine_prompt(
    template: &str,
    task_id: i64,
    db_path: &Path,
    seed: &str,
    note: &str,
) -> String {
    render(
        template,
        &[
            ("{seed}", seed),
            ("{note}", note.trim()),
            ("{task_id}", task_id.to_string().as_str()),
            ("{db}", db_flag(db_path).as_str()),
        ],
    )
}

/// The assembled launch of a planning session: the agent's `plan` template
/// with the prompt file substituted, and the project checkout to run it in.
/// The TUI turns this into a foreground terminal round-trip, the same
/// suspend/restore as `$EDITOR` and attach/resume.
#[derive(Debug)]
pub struct PlanLaunch {
    pub command: String,
    pub cwd: String,
    /// Which flow this is — `plan` or `refine` — for the launch log's
    /// breadcrumbs, since both run through the same foreground round-trip.
    pub label: &'static str,
    /// The refine round this launch opens, when it is one (DESIGN.md §6). The
    /// round-trip records the round once the child's pid is known and concludes
    /// it on return; a `Create` session carries `None`, having no task to
    /// transition and so earning no session row.
    pub refine: Option<RefineLaunch>,
}

/// What the foreground round-trip needs to open a refine round's session once
/// it knows the child's pid: which task is being refined, and by whom.
#[derive(Debug, Clone)]
pub struct RefineLaunch {
    pub task_id: i64,
    pub agent: String,
}

/// What an interactive agent session is pointed at (DESIGN.md §8/§6): the front
/// of a task's life, or a proposed task that already exists. Both run the same
/// `plan` verb in the foreground and both end in a CLI verb the agent calls —
/// `add` for a new task, `set --body-file` for a refine.
#[derive(Debug, Clone, Copy)]
pub enum PlanTarget {
    Create { project_id: i64 },
    Refine { task_id: i64 },
}

/// Assemble an interactive planning session (DESIGN.md §8): resolve the default
/// agent, require its `plan` verb, write the prompt outside the checkout, and
/// substitute it into the template. A `Create` session records nothing and
/// changes no task state — its deliverable is a `proposed` task the agent
/// creates through `voro add`, so one that exits without creating anything has
/// simply done nothing. A [`PlanTarget::Refine`] session is a refine round like
/// the headless one: the caller's round-trip moves the task `proposed →
/// refining` and opens its session once the child's pid is known, and the round
/// ends either with the agent's `voro set --body-file` or with the quit that
/// concludes it as cancelled. There is deliberately no dispatch-style guard:
/// planning only reads the checkout and writes nothing to it. A `Create` session
/// runs in the project's *default* repo, since it drafts a task rather than
/// executing one and the drafted task picks its own repo with `voro add
/// --repo`; a `Refine` runs in the task's resolved repo, which is where the code
/// its body must name actually lives.
pub fn plan_session(
    store: &Store,
    ctx: &DispatchCtx,
    target: PlanTarget,
) -> Result<PlanLaunch, String> {
    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config.resolve(None).map_err(|e| e.to_string())?;
    if agent.plan.is_none() {
        return Err(format!(
            "agent '{}' defines no plan template in {} — add plan = \"<interactive command> \
             {{prompt_file}}\" to its [agents.{}] table to plan tasks with it",
            agent.name,
            ctx.agents_path.display(),
            agent.name
        ));
    }
    let (label, launch, cwd, prompt, refine) = match target {
        PlanTarget::Create { project_id } => {
            let project = store.project(project_id).map_err(|e| e.to_string())?;
            let repo = store.default_repo(project_id).map_err(|e| e.to_string())?;
            (
                "plan",
                Launch::Plan {
                    project: project.name.clone(),
                },
                repo.path,
                render_planning_prompt(&project.name, &ctx.db_path),
                None,
            )
        }
        PlanTarget::Refine { task_id } => {
            let task = guard_refinable(store, task_id)?;
            let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;
            let seed = refine_seed(store, &task)?;
            (
                "refine",
                Launch::Refine { task_id },
                repo.path,
                render_refine_prompt(
                    REFINE_PLAN_PROMPT_TEMPLATE,
                    task_id,
                    &ctx.db_path,
                    &seed,
                    "",
                ),
                Some(RefineLaunch {
                    task_id,
                    agent: agent.name.clone(),
                }),
            )
        }
    };
    let prompt_path = write_prompt(ctx, &launch.slug(), &prompt)?;
    let command = agent
        .plan_launch_command(&LaunchSpec {
            launch,
            prompt_file: &prompt_path,
            deep: false,
        })
        .expect("the plan verb is checked above");
    Ok(PlanLaunch {
        command,
        cwd,
        label,
        refine,
    })
}

/// The precondition both refine flavours share: a refine round starts from
/// `proposed` or `ready` (DESIGN.md §6), so anything else is refused before a
/// prompt is written or a process spawned. The transition API refuses it again
/// when the round is recorded; this is the early, spelled-out refusal —
/// including of a task already `refining`, whose round the operator can only
/// cancel.
fn guard_refinable(store: &Store, task_id: i64) -> Result<voro_core::Task, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    if task.state == TaskState::Refining {
        return Err(format!(
            "task {task_id} is already being refined — cancel that round first"
        ));
    }
    if !matches!(task.state, TaskState::Proposed | TaskState::Ready) {
        return Err(format!(
            "only a proposed or ready task can be refined; task {task_id} is {}",
            task.state
        ));
    }
    Ok(task)
}

/// Write a prompt file outside every checkout, named for the flow that wrote it
/// and stamped so successive runs never clobber each other.
fn write_prompt(ctx: &DispatchCtx, name: &str, prompt: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = ctx.runtime_dir.join(format!("{name}-{stamp}.prompt.md"));
    std::fs::write(&path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", path.display()))?;
    Ok(path)
}

/// A terse human intent handed to a headless agent to expand into a formal
/// artefact, which the agent applies through a CLI verb (DESIGN.md §6). Refine
/// is the first instance — a one-line note becomes a dispatchable task body —
/// and the shape is deliberately general, because expanding review-reject
/// feedback the same way is the next one. Unlike a dispatch this opens no
/// session row and moves no task state: what comes back is whatever the verb the
/// agent calls does, so nothing here has to observe the process.
struct Expansion<'a> {
    /// Who this launch is, which names its session, its prompt and log files,
    /// and its launch-log lines (DESIGN.md §8).
    launch: Launch,
    /// The agent that will run it — the *default* one, since expanding an
    /// intent is not executing the task.
    agent: &'a ResolvedAgent,
    /// Whether the task carries `deep`, so the brief is written with the same
    /// model the work would be.
    deep: bool,
    prompt: String,
    cwd: String,
}

/// A spawned [`Expansion`], handed back so the caller can record it before
/// letting it run: the pid and log path the session row wants, the session name
/// the summary line names, and the child itself, which is either reaped
/// ([`reap_expansion`]) or killed if the recording fails.
struct Spawned {
    pid: i64,
    log_path: PathBuf,
    session_name: String,
    label: String,
    child: Child,
}

/// Spawn an [`Expansion`] detached, with its output captured to a log beside the
/// session logs. Both the log and the session name matter to the operator: the
/// launcher exits at birth, so the log holds its banner rather than the rewrite,
/// and the session name is what they attach to.
fn spawn_expansion(ctx: &DispatchCtx, exp: Expansion) -> Result<Spawned, String> {
    let label = exp.launch.slug();
    let prompt_path = write_prompt(ctx, &label, &exp.prompt)?;
    let command = exp.agent.launch_command(&LaunchSpec {
        launch: exp.launch.clone(),
        prompt_file: &prompt_path,
        deep: exp.deep,
    });
    spawn_logged(
        ctx,
        label,
        &prompt_path,
        &command,
        &exp.cwd,
        exp.launch.session_name(),
    )
}

/// Spawn a rendered command detached in `cwd`, with its output captured to a log
/// stamped like the prompt beside it and a breadcrumb in the launch log. The
/// shared tail of every background launch that is not a dispatch: the caller
/// decides which verb template rendered the command and what the session it
/// belongs to is called.
fn spawn_logged(
    ctx: &DispatchCtx,
    label: String,
    prompt_path: &Path,
    command: &str,
    cwd: &str,
    session_name: String,
) -> Result<Spawned, String> {
    let stamp = prompt_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim_end_matches(".prompt").to_string())
        .unwrap_or_else(|| label.clone());
    let log_path = ctx.runtime_dir.join(format!("{stamp}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    let launch_log = ctx.launch_log_path();
    append_launch_log(&launch_log, &format!("{label}: {command} (cwd {cwd})"));
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("VORO_DB", &ctx.db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn agent in {cwd}: {e}"))?;

    Ok(Spawned {
        pid: i64::from(child.id()),
        log_path,
        session_name,
        label,
        child,
    })
}

/// Hand a spawned expansion to a detached reaper thread, which records its exit
/// in the launch log. Nothing waits on it otherwise, and a zombie would linger
/// for the life of a long-running TUI — and `kill -0` on a zombie still reports
/// it alive, which would defeat the reconciler's probe (DESIGN.md §8).
fn reap_expansion(spawned: Spawned, launch_log: PathBuf) {
    let Spawned {
        mut child, label, ..
    } = spawned;
    std::thread::spawn(move || {
        let line = match child.wait() {
            Ok(status) => format!("{label}: exited with {status}"),
            Err(e) => format!("{label}: could not be waited on: {e}"),
        };
        append_launch_log(&launch_log, &line);
    });
}

/// Kill a spawned expansion's process group and reap it, for the case where the
/// store write that should have recorded it failed: an agent whose round was
/// never recorded must not keep rewriting a body unobserved.
fn kill_expansion(spawned: Spawned) {
    let Spawned { mut child, pid, .. } = spawned;
    let _ = Command::new("kill")
        .args(["-TERM", "--"])
        .arg(format!("-{pid}"))
        .status();
    let _ = child.wait();
}

/// Note-driven refine (DESIGN.md §6): hand a proposed or ready task's body, the
/// operator's note, and the context of the task it was discovered from to a
/// headless agent, which rewrites the body and applies it with `voro set
/// --body-file`. The task moves `proposed | ready → refining` and opens a
/// session row in one write, so the rewrite is visible in every window and out
/// of the queue until it concludes — which is what stops a second window
/// dispatching a `ready` task against the body being replaced. However the round
/// began it concludes to `proposed`, so the rewritten body earns a fresh
/// verdict. The *default* agent runs it whatever override the
/// task carries, since a task's agent override picks who executes the task, not
/// who writes its brief; the task's `deep` flag still applies, because a task
/// worth the strongest model is worth a brief written with it. A human task
/// refines like any other: no agent can *execute* it, but its brief is still
/// text an agent can write.
///
/// The session is named `voro-<id>-refine` ([`Launch::Refine`]), so it is
/// findable in the agent's fleet listing and cannot collide with the dispatch of
/// the same task.
pub fn refine(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    note: &str,
) -> Result<String, String> {
    if note.trim().is_empty() {
        return Err("a refine note is required".into());
    }
    let task = guard_refinable(store, task_id)?;
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    if project.archived {
        return Err(format!(
            "project '{}' is archived — `voro project unarchive {}` first",
            project.name, project.name
        ));
    }
    let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;
    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config.resolve(None).map_err(|e| e.to_string())?;
    let seed = refine_seed(store, &task)?;
    let prompt = render_refine_prompt(REFINE_PROMPT_TEMPLATE, task_id, &ctx.db_path, &seed, note);

    let cwd = repo.path.clone();
    let spawn_ms = now_ms();
    let spawned = spawn_expansion(
        ctx,
        Expansion {
            launch: Launch::Refine { task_id },
            agent: &agent,
            deep: task.deep,
            prompt,
            cwd: repo.path,
        },
    )?;
    let pid = spawned.pid;
    let log_path = spawned.log_path.clone();
    let session_name = spawned.session_name.clone();

    // Recorded after the spawn, exactly as dispatch records its session, so the
    // pid the reconciler probes is the real one; a round that could not be
    // recorded takes its agent down with it rather than rewriting a body no
    // window knows is being rewritten.
    let recorded = store.record_refine_launch(
        task_id,
        note,
        &agent.name,
        Some(pid),
        Some(log_path.to_string_lossy().as_ref()),
    );
    let session = match recorded {
        Ok((_, session)) => session,
        Err(e) => {
            kill_expansion(spawned);
            return Err(format!(
                "recording the refine failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
        }
    };
    reap_expansion(spawned, ctx.launch_log_path());

    // A round's ref is captured exactly as a dispatch's is, and for the same
    // reason (DESIGN.md §8): the round runs the agent's `dispatch` template, so
    // where that template detaches — `claude --bg` does — the pid above belongs
    // to a launcher that exits at birth, and only the agent's own listing can
    // say whether the round is still working.
    let ref_note = match &agent.sessions {
        Some(sessions_cmd) => match capture_session_ref(
            sessions_cmd,
            &cwd,
            spawn_ms,
            ctx.ref_capture_timeout,
            &log_path,
        ) {
            Some(session_ref) => {
                store
                    .set_session_ref(session.id, &session_ref)
                    .map_err(|e| e.to_string())?;
                format!(", ref {session_ref}")
            }
            None => ", session ref not captured".to_string(),
        },
        None => String::new(),
    };

    Ok(format!(
        "task {task_id} sent for refinement by '{}' as {session_name} (pid {pid}{ref_note}) — log {}",
        agent.name,
        log_path.display()
    ))
}

/// How long [`send_message`] watches a spawned send before calling it started.
const MESSAGE_GRACE: Duration = Duration::from_secs(2);

/// How often that window is polled.
const MESSAGE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How much of a failed send's log to look at for the line to quote back.
const LOG_TAIL_BYTES: u64 = 4096;

/// One line said into a session that already exists (DESIGN.md §8), assembled by
/// the TUI's quick-message key. Unlike an [`Expansion`] this opens no session
/// row: it joins a conversation Voro already knows about rather than starting
/// one, so there is no new identity to compose — it updates the row that
/// conversation already has (DESIGN.md §8).
pub struct SessionMessage<'a> {
    /// The task whose session is being messaged — names the prompt and log
    /// files, and the launch-log line.
    pub task_id: i64,
    /// The agent's `message` template, already known to define the verb.
    pub template: &'a str,
    /// The reference captured at dispatch, which the template's `{session}`
    /// binds to.
    pub session_ref: &'a str,
    pub message: &'a str,
    /// The task's own checkout: a headless resume has to run where its session
    /// does, since that is what the agent resolves the conversation against.
    pub cwd: String,
}

/// A quick message whose send is under way: the spawn survived its grace window
/// ([`send_message`]), so the caller may now record what it changed about the
/// session and — for a rejection — apply the transition the message carries.
/// The child is still held, so a caller whose recording fails can take the
/// agent down with it ([`abandon`](Self::abandon)) rather than leaving it acting
/// on a message no state reflects.
pub struct SentMessage {
    spawned: Spawned,
    task_id: i64,
    new_session_ref: Option<String>,
}

impl SentMessage {
    /// The process carrying the turn. Unlike a dispatch's launcher pid this is
    /// the send itself, so it is alive for as long as the agent is answering —
    /// which is what keeps the reconciler off a task whose forked turn does not
    /// appear in the agent's own listing (DESIGN.md §8).
    pub fn pid(&self) -> i64 {
        self.spawned.pid
    }

    /// The reference the session answers to from now on, when the agent's verb
    /// forked rather than resuming in place.
    pub fn new_session_ref(&self) -> Option<&str> {
        self.new_session_ref.as_deref()
    }

    /// Let the send run: hand the child to the reaper and report it. From here
    /// the turn is fire-and-forget — it is appended to a transcript the operator
    /// watches elsewhere, so an agent-side refusal after this point lands in the
    /// log rather than back in the UI.
    pub fn confirm(self, ctx: &DispatchCtx) -> String {
        let summary = format!(
            "message sent to task {}'s session — log {}",
            self.task_id,
            self.spawned.log_path.display()
        );
        reap_expansion(self.spawned, ctx.launch_log_path());
        summary
    }

    /// Kill the send's process group, for a caller whose store write failed
    /// after the spawn: an agent must not go on acting on a message the
    /// database does not record.
    pub fn abandon(self) {
        kill_expansion(self.spawned);
    }
}

/// Fire a message into a task's agent session and wait just long enough to know
/// it started (DESIGN.md §8). A headless send can be refused outright — a
/// supervisor-held session, a stale reference — and that refusal exits in well
/// under a second, so the spawn is given a grace window to fail in and an early
/// non-zero exit is reported as a send that did not happen. What comes back is
/// a [`SentMessage`] the caller records against the session before letting it
/// run; the transition a rejection carries hangs off that, so nothing is
/// committed against a message that never left.
pub fn send_message(ctx: &DispatchCtx, msg: SessionMessage) -> Result<SentMessage, String> {
    if msg.message.trim().is_empty() {
        return Err("a message is required".into());
    }
    let label = format!("message-{}", msg.task_id);
    let prompt_path = write_prompt(ctx, &label, msg.message)?;
    let rendered = voro_core::render_message(msg.template, msg.session_ref, &prompt_path);
    let mut spawned = spawn_logged(
        ctx,
        label,
        &prompt_path,
        &rendered.command,
        &msg.cwd,
        msg.session_ref.to_string(),
    )?;
    // An exit inside the window is only a failure if it failed: a verb that
    // says its piece and returns cleanly has delivered the message.
    if let Some(status) = wait_for_early_exit(&mut spawned.child, ctx.message_grace)
        && !status.success()
    {
        append_launch_log(
            &ctx.launch_log_path(),
            &format!("{}: exited with {status}", spawned.label),
        );
        return Err(format!(
            "the message was refused by '{}' ({status}){}",
            msg.template
                .split_whitespace()
                .next()
                .unwrap_or("the agent"),
            log_tail_note(&spawned.log_path)
        ));
    }
    Ok(SentMessage {
        spawned,
        task_id: msg.task_id,
        new_session_ref: rendered.new_session_ref,
    })
}

/// Poll a freshly spawned child for up to `grace`, returning its status if it
/// exits in that window and `None` if it is still going (or could not be
/// waited on, which is not evidence either way). A zero grace is a single look.
fn wait_for_early_exit(child: &mut Child, grace: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(MESSAGE_POLL_INTERVAL.min(grace));
    }
}

/// The last line a failed send wrote, for the status line — the agent's own
/// account of why it refused, which is otherwise only in the log file. Empty
/// when there is nothing to quote.
fn log_tail_note(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if file
        .seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))
        .is_err()
    {
        return String::new();
    }
    let mut tail = String::new();
    if file.read_to_string(&mut tail).is_err() {
        return String::new();
    }
    match tail.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(line) => format!(": {}", line.trim().chars().take(160).collect::<String>()),
        None => String::new(),
    }
}

/// Where dispatch finds its inputs and puts its artefacts. Built from the
/// database path in `main`; constructed directly in tests.
pub struct DispatchCtx {
    /// The active database, exported to the session as `VORO_DB` so the
    /// agent's return-path verbs write back to the same store.
    pub db_path: PathBuf,
    /// The config-file location dispatch reads (`voro.toml`).
    pub agents_path: PathBuf,
    /// Directory for prompt and log files — never inside a project checkout,
    /// so prompt and log files do not pollute the operator's working copy.
    pub runtime_dir: PathBuf,
    /// How long to keep polling for a session reference after spawning an
    /// agent that defines a `sessions` verb, before giving up (the ref stays
    /// NULL and the summary says so). Zero means a single attempt.
    pub ref_capture_timeout: Duration,
    /// How long a quick message's process gets to prove it started before the
    /// send is treated as having landed (DESIGN.md §8). Long enough for a
    /// refusal — which is immediate — to be caught, short enough that the TUI
    /// does not visibly stall. Zero means a single look.
    pub message_grace: Duration,
}

impl DispatchCtx {
    /// The real environment: the config file at its resolved default
    /// location, and a `sessions/` directory beside the database for prompts
    /// and logs.
    pub fn from_db_path(db_path: &Path) -> DispatchCtx {
        let runtime_dir = db_path
            .parent()
            .map(|d| d.join("sessions"))
            .unwrap_or_else(|| PathBuf::from("sessions"));
        DispatchCtx {
            db_path: db_path.to_path_buf(),
            agents_path: AgentsConfig::default_path(),
            runtime_dir,
            ref_capture_timeout: Duration::from_secs(5),
            message_grace: MESSAGE_GRACE,
        }
    }

    /// A context whose `voro.toml` is deliberately absent, so a test resolves
    /// agents and prices the queue from the built-ins alone rather than from
    /// whatever the developer happens to have configured.
    #[cfg(test)]
    pub fn without_config(db_path: &Path) -> DispatchCtx {
        DispatchCtx {
            agents_path: db_path.with_file_name("no-such-voro.toml"),
            ..DispatchCtx::from_db_path(db_path)
        }
    }

    /// The rolling log for subprocess launches that are *not* dispatches —
    /// viewer opens and attach/resume round-trips, which have no per-session
    /// log. They share one append-only file so a failure the TUI would paint
    /// over still leaves a breadcrumb. Single file, no rotation (DESIGN.md §8).
    pub fn launch_log_path(&self) -> PathBuf {
        self.runtime_dir.join("launches.log")
    }
}

/// Append a timestamped line to the launch log, best-effort — a launch must
/// not fail because its breadcrumb could not be written. Creates the parent
/// directory and opens in append mode so concurrent writers (a viewer's own
/// redirected stdout/stderr and this record line) interleave cleanly.
pub(crate) fn append_launch_log(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {line}");
    }
}

/// Dispatch a ready task — or redispatch a stalled one — to a headless agent
/// session, returning a summary line. `agent_override` is the CLI `--agent`
/// flag, which outranks the task's own override (DESIGN.md §8). Every check
/// that can fail runs before the process is spawned, so a failed dispatch never
/// leaves an orphan session.
pub fn dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    spawn_session(store, ctx, task_id, agent_override)
}

/// Open a `review` (or `running`) task's diff in a viewer (DESIGN.md §11a): the
/// viewer medium of the per-project review action (§8). A dispatched agent works
/// in a throwaway worktree on the task's branch, so the diff lives there, not in
/// the primary checkout — the viewer is run in that worktree when the task has a
/// live one, falling back to the task's resolved repo when it has no branch or no worktree
/// (§8). `viewer_override` names a `[viewers.<name>]` entry; `None` falls back to
/// the project's review action or the config default. The viewer template gets
/// `{path}` (the resolved dir), `{branch}` (the task's branch, empty when none),
/// and `{base}` (the checkout's default branch) so it can express a diff range.
/// Opening never touches task state, so there is no clean-tree guard (the diff
/// reviewed is often the uncommitted work itself).
pub fn open(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    viewer_override: Option<&str>,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    if !matches!(task.state, TaskState::Review | TaskState::Running) {
        return Err(format!(
            "only review or running tasks can be opened in a viewer; task {task_id} is {}",
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    // The task's own repo, not the project default: a task dispatched into a
    // second repo has its branch and worktree there (DESIGN.md §8).
    let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;

    // The project's review action names the viewer its local diffs open in
    // (DESIGN.md §8), which is all that setting still decides.
    let project_viewer = project.review_action.viewer().map(str::to_string);
    let viewer_name = viewer_override.map(str::to_string).or(project_viewer);

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let viewer = config
        .viewer_cmd(viewer_name.as_deref())
        .map_err(|e| e.to_string())?;

    // Run the viewer in the task's worktree when it has one — that is where the
    // dispatched agent's diff lives — otherwise the primary checkout.
    let viewer_dir = match task.branch.as_deref() {
        Some(branch) => crate::worktree::worktree_on_branch(&repo.path, branch)?
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo.path.clone()),
        None => repo.path.clone(),
    };
    // `{base}` is what the viewer template diffs against, so narrowing a
    // re-review to the rework is a matter of binding it to the revision the
    // operator last reviewed rather than to the checkout's default branch
    // (DESIGN.md §8). Only a `review` task that has been sent back once has
    // one; everything else diffs against the base as it always did.
    let (base, scope) = match (task.state, task.branch.as_deref()) {
        (TaskState::Review, Some(branch)) => {
            match crate::pr::local_review_diff(store, task_id, &repo.path, branch) {
                ReviewDiff::Since { since, .. } => {
                    (since, " — the changes since your last review".to_string())
                }
                ReviewDiff::Full {
                    notice: Some(notice),
                } => (default_base_branch(&repo.path), format!(" — {notice}")),
                ReviewDiff::Full { notice: None } => {
                    (default_base_branch(&repo.path), String::new())
                }
            }
        }
        _ => (default_base_branch(&repo.path), String::new()),
    };
    let branch = task.branch.clone().unwrap_or_default();
    let command = viewer
        .replace(
            VIEWER_PATH_PLACEHOLDER,
            &shell_quote(Path::new(&viewer_dir)),
        )
        .replace(VIEWER_BRANCH_PLACEHOLDER, &branch)
        .replace(VIEWER_BASE_PLACEHOLDER, &base);

    // Redirect the detached viewer's output to the shared launch log so a
    // silent failure leaves a breadcrumb.
    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    let launch_log = ctx.launch_log_path();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&launch_log)
        .map_err(|e| format!("cannot open launch log {}: {e}", launch_log.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open launch log {}: {e}", launch_log.display()))?;
    append_launch_log(
        &launch_log,
        &format!("viewer: {command} (cwd {viewer_dir})"),
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&viewer_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn viewer in {viewer_dir}: {e}"))?;
    let pid = i64::from(child.id());

    // Nothing waits on the viewer — reap it in a detached thread so an exited
    // child never lingers as a zombie, and record its exit status so a viewer
    // that failed after spawning is findable in the log.
    let reap_log = launch_log.clone();
    let reap_command = command.clone();
    std::thread::spawn(move || {
        let line = match child.wait() {
            Ok(status) => format!("viewer: {reap_command} exited with {status}"),
            Err(e) => format!("viewer: {reap_command} could not be waited on: {e}"),
        };
        append_launch_log(&reap_log, &line);
    });

    Ok(format!(
        "opened task {task_id} in {} (pid {pid}){scope} — launch log {}",
        project.name,
        launch_log.display()
    ))
}

/// How the session reference on the freshly-recorded session row came to be,
/// for the summary line.
enum RefOutcome {
    /// Captured from the `sessions` listing or the log.
    Captured(String),
    /// The agent defines `sessions` but the reference never showed up.
    NotCaptured,
    /// The agent has no capture story; nothing was attempted.
    NotApplicable,
}

fn spawn_session(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    // Checked here so a human-only task is refused before anything spawns;
    // voro-core's record_dispatch is the backstop.
    if task.human {
        return Err(format!(
            "task {task_id} is human-only — no agent can execute it; work it by hand \
             (`voro start {task_id}`, then `voro done {task_id}`)"
        ));
    }
    if !matches!(task.state, TaskState::Ready | TaskState::Stalled) {
        return Err(format!(
            "only ready or stalled tasks can be dispatched; task {task_id} is {}",
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    // Refused here so an archived project is heard before anything spawns;
    // voro-core's record_dispatch is the backstop.
    if project.archived {
        return Err(format!(
            "project '{}' is archived — `voro project unarchive {}` first",
            project.name, project.name
        ));
    }

    // The checkout the session runs in: the task's own repo when it names one,
    // else the project's default (DESIGN.md §8).
    let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config
        .resolve(agent_override.or(task.agent.as_deref()))
        .map_err(|e| e.to_string())?;

    guard_git_repo(&repo.path)?;

    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    // One stamp names both artefacts, pairing each session's prompt with its
    // log and keeping earlier sessions' files intact across redispatch.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let launch = Launch::Dispatch { task_id };
    let prompt_path = ctx
        .runtime_dir
        .join(format!("{}-{stamp}.prompt.md", launch.slug()));
    let body = if task.body.is_empty() {
        format!("# {}\n", task.title)
    } else {
        format!("# {}\n\n{}\n", task.title, task.body.trim_end())
    };
    // The plans this task derives from (DESIGN.md §3/§8), each resolved to
    // where it actually is — a linked doc may live in another project's
    // checkout, so the prompt names it absolutely rather than relative to the
    // cwd the session runs in.
    let docs = store
        .docs_for_task(task_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|doc| {
            let location = store.resolve_doc(&doc).map_err(|e| e.to_string())?;
            Ok((doc.label().to_string(), location))
        })
        .collect::<Result<Vec<_>, String>>()?;
    // A redispatch of work that was reviewed and sent back carries the
    // answer-the-feedback instruction (DESIGN.md §8): the successor session has
    // none of the rejected round's context, so the points in the body's
    // `## Feedback` section are all it has to answer.
    let rework = store
        .events_for(task_id)
        .map(|events| voro_core::was_rejected(&events))
        .unwrap_or(false);
    let prompt = format!(
        "{}{body}",
        render_preamble(task_id, &ctx.db_path, task.branch.as_deref(), &docs, rework)
    );
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;

    let log_path = ctx
        .runtime_dir
        .join(format!("{}-{stamp}.log", launch.slug()));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    // `launch_command` binds every placeholder the template may carry in one
    // pass: the prompt file, the session name `voro-<id>`, the task id, and
    // `{model}` resolved from the task's depth — the deeper model for a deep
    // task, the workhorse otherwise, and nothing at all for an agent whose
    // template names no model (DESIGN.md §8).
    let command = agent.launch_command(&LaunchSpec {
        launch,
        prompt_file: &prompt_path,
        deep: task.deep,
    });
    let spawn_ms = now_ms();
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&repo.path)
        .env("VORO_TASK_ID", task_id.to_string())
        .env("VORO_DB", &ctx.db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn agent '{}': {e}", agent.name))?;
    let pid = i64::from(child.id());

    let recorded = store.record_dispatch(
        task_id,
        &agent.name,
        Some(pid),
        Some(log_path.to_string_lossy().as_ref()),
    );
    let (task, session) = match recorded {
        Ok(pair) => pair,
        Err(e) => {
            // An agent whose session never got recorded must not keep working
            // unrecorded: kill its process group (pgid = pid, set at spawn)
            // and reap the shell before surfacing the error.
            let _ = Command::new("kill")
                .args(["-TERM", "--"])
                .arg(format!("-{pid}"))
                .status();
            let _ = child.wait();
            return Err(format!(
                "recording the session failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
        }
    };

    // Dispatch returns as soon as the session is recorded, so nothing else
    // waits on the child. Left unreaped it becomes a zombie, and `kill -0` on a
    // zombie still succeeds — permanently defeating reconciliation's
    // pid-liveness check in a long-lived TUI. A detached reaper collects the
    // exit status without blocking dispatch.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let ref_outcome = match &agent.sessions {
        Some(sessions_cmd) => {
            match capture_session_ref(
                sessions_cmd,
                &repo.path,
                spawn_ms,
                ctx.ref_capture_timeout,
                &log_path,
            ) {
                Some(session_ref) => {
                    store
                        .set_session_ref(session.id, &session_ref)
                        .map_err(|e| e.to_string())?;
                    RefOutcome::Captured(session_ref)
                }
                None => RefOutcome::NotCaptured,
            }
        }
        None => RefOutcome::NotApplicable,
    };
    let ref_note = match &ref_outcome {
        RefOutcome::Captured(session_ref) => format!(", ref {session_ref}"),
        RefOutcome::NotCaptured => ", session ref not captured".to_string(),
        RefOutcome::NotApplicable => String::new(),
    };

    Ok(format!(
        "dispatched task {} to {} (session {}, pid {}{}) — log {}",
        task.id,
        session.agent,
        session.id,
        pid,
        ref_note,
        log_path.display()
    ))
}

/// Wall-clock milliseconds, the unit an agent's `sessions` listing stamps its
/// entries in, so a spawn can be told from the sessions that preceded it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Capture the agent's reference for the session just spawned: poll the
/// `sessions` listing for a session started in this project at or after the
/// spawn, falling back to the `backgrounded · <id>` line launchers print into
/// the log. `None` once the timeout passes without either source producing one.
fn capture_session_ref(
    sessions_cmd: &str,
    repo_path: &str,
    spawn_ms: i64,
    timeout: Duration,
    log_path: &Path,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(session_ref) = query_new_session(sessions_cmd, repo_path, spawn_ms) {
            return Some(session_ref);
        }
        if let Some(session_ref) = log_backgrounded_ref(log_path) {
            return Some(session_ref);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(REF_POLL_INTERVAL);
    }
}

/// One poll of the `sessions` command: the newest listed session whose `cwd`
/// is this project and whose `startedAt` is at or after the spawn (with a
/// little slack for the agent's own clock). `None` on any failure — capture
/// is best-effort and the caller retries until its deadline.
fn query_new_session(sessions_cmd: &str, repo_path: &str, spawn_ms: i64) -> Option<String> {
    let entries =
        crate::session_probe::run_sessions_command(sessions_cmd, Some(Path::new(repo_path)))?;
    entries
        .into_iter()
        .filter(|e| e.cwd.as_deref() == Some(repo_path))
        .filter(|e| {
            e.started_at_ms
                .is_some_and(|t| t >= spawn_ms - SPAWN_CLOCK_SLACK_MS)
        })
        .max_by_key(|e| e.started_at_ms)
        .map(|e| e.session_ref)
}

/// The secondary capture source: `claude --bg`-style launchers print
/// `backgrounded · <id>` (ANSI-coloured) on stdout, which dispatch already
/// redirects into the session log. The id is the line's last token once the
/// colour codes are stripped.
fn log_backgrounded_ref(log_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_path).ok()?;
    let text = strip_ansi(&text);
    text.lines()
        .find(|line| line.contains("backgrounded"))?
        .split_whitespace()
        .last()
        .filter(|token| *token != "backgrounded" && *token != "·")
        .map(str::to_string)
}

/// Drop ANSI escape sequences (the CSI colour codes launchers wrap their
/// output in) so the log can be tokenised as plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Refuse to dispatch into a path that is not a git repository (DESIGN.md §8):
/// the dispatched agent does its work in a git worktree of the checkout, which a
/// non-repo cannot provide, and Voro's on-close cleanup resolves worktrees
/// through git. A path where `git status` cannot run or fails is refused, and
/// the error names the path. The working tree's cleanliness is not inspected:
/// `git worktree add` snapshots HEAD, so the operator's uncommitted changes
/// never enter the agent's diff.
fn guard_git_repo(path: &str) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("cannot run git in {path}: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("cannot verify {path} is a git repository: git status failed")
        } else {
            format!("cannot verify {path} is a git repository: {detail}")
        });
    }
    Ok(())
}

/// The checkout's default branch — what a viewer template's `{base}` diffs a
/// task branch against (DESIGN.md §8). Read from `refs/remotes/origin/HEAD`,
/// whose `--short` form is `origin/<branch>`; the remote prefix is dropped. A
/// checkout with no origin or no symbolic HEAD falls back to `main`.
fn default_base_branch(repo_path: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let head = String::from_utf8_lossy(&o.stdout);
            let head = head.trim();
            head.strip_prefix("origin/").unwrap_or(head).to_string()
        }
        _ => "main".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voro_core::{NewTask, Priority};

    /// A scratch database, a freshly-`git init`ed clean project, and an
    /// `voro.toml` whose one agent is a stub command that just reads the
    /// prompt. Returns the store, the dispatch context, and the project path.
    fn fixture(cmd: &str) -> (Store, DispatchCtx, PathBuf) {
        fixture_toml(&format!(
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"{cmd}\"\n"
        ))
    }

    /// Like [`fixture`], but with the whole `voro.toml` supplied, for tests
    /// exercising the session verbs.
    fn fixture_toml(agents_toml: &str) -> (Store, DispatchCtx, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "voro-dispatch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q"]);

        let db_path = root.join("voro.db");
        let agents_path = root.join("voro.toml");
        std::fs::write(&agents_path, agents_toml).unwrap();

        let store = Store::open(&db_path).unwrap();
        let ctx = DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        (store, ctx, project)
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// Poll a session log until the spawned agent has written something, so a
    /// test can assert on what it printed without racing the detached child.
    fn wait_for_log(path: &str) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && !text.trim().is_empty()
            {
                return text;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{path} stayed empty");
    }

    fn ready_task(store: &mut Store, project_path: &Path) -> i64 {
        let p = store
            .create_project("proj", project_path.to_str().unwrap())
            .unwrap();
        store
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
            .unwrap()
            .id
    }

    #[test]
    fn dispatch_spawns_records_session_and_runs_the_task() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");

        let task = store.task(id).unwrap();
        assert_eq!(task.state, TaskState::Running);

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, "stub");
        assert!(session.pid.is_some_and(|p| p > 0));
        let log = session.log_path.as_deref().unwrap();
        assert!(Path::new(log).exists(), "log file {log} should exist");

        // no sessions verb, so no capture was attempted and none is reported
        assert!(session.session_ref.is_none());
        assert!(!summary.contains("ref"), "{summary}");

        // the prompt carries the task title and body
        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        assert!(prompt.contains("Do the thing"));
        assert!(prompt.contains("Detailed prompt."));
    }

    #[test]
    fn dispatched_prompt_injects_the_return_path_preamble() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();

        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        // the return-path verbs name the literal task id, no env var
        assert!(prompt.contains(&format!("voro ask {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro resume {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro done {id}")), "{prompt}");
        // propose carries the literal --from so a mid-session proposal links
        // back to this task without leaning on an inherited env var
        assert!(prompt.contains("voro propose"), "{prompt}");
        assert!(prompt.contains(&format!("--from {id}")), "{prompt}");
        assert!(!prompt.contains("VORO_TASK_ID"), "{prompt}");
        // even with no assigned branch, the agent is told to register the branch
        // it picks (`voro set` and `--branch <name>` asserted apart, since a
        // `--db` flag may sit between them under a non-default store)
        assert!(prompt.contains(&format!("voro set {id}")), "{prompt}");
        assert!(prompt.contains("--branch <name>"), "{prompt}");
        assert!(prompt.contains("Detailed prompt."), "{prompt}");
        // the rendered preamble is dropped in ahead of the task body
        assert!(
            prompt.starts_with(&render_preamble(id, &ctx.db_path, None, &[], false)),
            "{prompt}"
        );
        assert!(
            prompt.find("# Do the thing").unwrap() > prompt.find("voro done").unwrap(),
            "the task body must follow the preamble"
        );
    }

    #[test]
    fn preamble_renders_a_db_flag_only_for_a_non_default_database() {
        // a scratch (non-default) db renders --db on every verb, shell-quoted
        let db = PathBuf::from("/tmp/scratch/voro.db");
        let rendered = render_preamble(62, &db, None, &[], false);
        assert!(
            rendered.contains("voro ask 62 --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("voro done 62 --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("voro propose <project> \"<title>\" --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );

        // the default db is what the verbs resolve to unaided, so no flag
        let default = render_preamble(62, &Store::production_db_path(), None, &[], false);
        assert!(default.contains("voro ask 62 --question"), "{default}");
        assert!(!default.contains("--db"), "{default}");
    }

    #[test]
    fn preamble_tells_both_cases_to_register_the_branch_early() {
        // no assigned branch: the agent is told to register the name it picks,
        // but branch-assignment wording and a completion `voro done --branch`
        // are absent, since no name is known.
        let plain = render_preamble(62, &Store::production_db_path(), None, &[], false);
        assert!(!plain.contains("git branch `"), "{plain}");
        assert!(plain.contains("voro set 62 --branch <name>"), "{plain}");
        assert!(!plain.contains("voro done 62 --branch"), "{plain}");
        assert!(plain.contains("Voro runs no git"), "{plain}");
        // and the mandatory worktree instruction: work in a throwaway worktree,
        // never in the primary checkout.
        assert!(plain.contains("git worktree add"), "{plain}");
        assert!(plain.contains("never\nmodify it"), "{plain}");
        // stale-branch self-serve: fetch the base and rebase onto it, no push.
        assert!(plain.contains("git fetch origin <base>"), "{plain}");
        assert!(plain.contains("origin/<base>"), "{plain}");
        assert!(plain.contains("never push"), "{plain}");

        // with a branch: the agent is told to use it, register it, and confirm
        // it at completion.
        let branched = render_preamble(
            62,
            &Store::production_db_path(),
            Some("feat/parser"),
            &[],
            false,
        );
        assert!(branched.contains("git branch `feat/parser`"), "{branched}");
        assert!(branched.contains("Voro runs no git"), "{branched}");
        // the assigned case carries the same stale-branch self-serve instruction.
        assert!(branched.contains("git fetch origin <base>"), "{branched}");
        assert!(branched.contains("origin/<base>"), "{branched}");
        assert!(branched.contains("never push"), "{branched}");
        // the worktree instruction carries the assigned branch name.
        assert!(
            branched.contains("git worktree add <path> -b feat/parser"),
            "{branched}"
        );
        assert!(branched.contains("never modify it"), "{branched}");
        assert!(
            branched.contains("voro set 62 --branch feat/parser"),
            "{branched}"
        );
        assert!(
            branched.contains("voro done 62 --branch feat/parser"),
            "{branched}"
        );
    }

    #[test]
    fn preamble_puts_the_harness_worktree_tool_ahead_of_git_worktree_add() {
        // A harness that owns worktree creation must be used on its own terms:
        // Claude Code blocks edits until `EnterWorktree` has run, and aiming it
        // at a hand-made worktree asks for an approval a headless session
        // cannot give. `git worktree add` survives only as the fallback, and
        // because the harness names the branch itself, both cases spell out the
        // rename onto the branch Voro tracks.
        for (branch, name) in [(None, "<name>"), (Some("feat/parser"), "feat/parser")] {
            let rendered = render_preamble(62, &Store::production_db_path(), branch, &[], false);
            assert!(rendered.contains("`EnterWorktree` tool"), "{rendered}");
            assert!(
                rendered.contains(&format!("git switch -c {name}")),
                "{rendered}"
            );
            let enter = rendered.find("EnterWorktree").unwrap();
            let manual = rendered.find("git worktree add").unwrap();
            assert!(
                enter < manual,
                "the harness mechanism must lead, the manual worktree follow: {rendered}"
            );
            assert!(
                rendered.contains(&format!("git worktree add <path> -b {name}")),
                "{rendered}"
            );
        }
    }

    #[test]
    fn preamble_names_a_task_s_linked_documents_at_their_resolved_locations() {
        // A task linked to a plan gets it handed over rather than having to
        // rediscover it from hints in the body (DESIGN.md §3/§8). The location
        // is absolute, since a linked doc may live in another checkout.
        let db = Store::production_db_path();
        let docs = [
            (
                "Strategy 2026".to_string(),
                "/tmp/augere/docs/strategy.md".to_string(),
            ),
            (
                "https://example.com/rfc".to_string(),
                "https://example.com/rfc".to_string(),
            ),
        ];
        let linked = render_preamble(62, &db, None, &docs, false);
        assert!(
            linked.contains("derives from the document(s) below"),
            "{linked}"
        );
        assert!(
            linked.contains("/tmp/augere/docs/strategy.md  — Strategy 2026"),
            "{linked}"
        );
        // An untitled doc is named once, not doubled up with itself.
        assert!(
            linked.contains("\n    https://example.com/rfc\n"),
            "{linked}"
        );
        // The block sits ahead of the body separator, so the plan is read first.
        let docs_at = linked.find("derives from the document").unwrap();
        assert!(docs_at < linked.rfind("---").unwrap(), "{linked}");

        // A task citing no document renders exactly the prompt it always did.
        let plain = render_preamble(62, &db, None, &[], false);
        assert!(!plain.contains("derives from the document"), "{plain}");
    }

    /// A redispatch of rejected work carries the answer-the-feedback
    /// instruction, and a task nobody rejected carries none (DESIGN.md §8).
    #[test]
    fn preamble_tells_a_redispatched_rework_to_answer_the_feedback() {
        let db = Store::production_db_path();
        let reworking = render_preamble(62, &db, None, &[], true);
        assert!(
            reworking.contains("been through review once already"),
            "{reworking}"
        );
        assert!(reworking.contains("point by point"), "{reworking}");
        assert!(
            reworking.contains("voro done 62 --summary"),
            "the instruction must be copy-pasteable: {reworking}"
        );

        let first = render_preamble(62, &db, None, &[], false);
        assert!(!first.contains("point by point"), "{first}");
    }

    /// The rejection as the live session hears it: the operator's points
    /// verbatim, plus the same instruction the preamble carries.
    #[test]
    fn the_rework_message_carries_the_feedback_and_the_instruction() {
        let message = rework_message(
            62,
            &Store::production_db_path(),
            "  tests are missing, and the docs are stale  ",
        );
        assert!(
            message.contains("task 62 was reviewed and sent back"),
            "{message}"
        );
        assert!(
            message.contains("tests are missing, and the docs are stale"),
            "{message}"
        );
        assert!(message.contains("point by point"), "{message}");
        assert!(message.contains("voro done 62 --summary"), "{message}");
    }

    /// Feedback is untrusted text: a point that spells a placeholder reaches
    /// the agent as the operator wrote it, and the message's own placeholders
    /// still resolve.
    #[test]
    fn rework_message_emits_feedback_verbatim() {
        let db = PathBuf::from("/tmp/other.db");
        let message = rework_message(62, &db, "the {task_id} handling is wrong, and {db} too");
        assert!(
            message.contains("the {task_id} handling is wrong, and {db} too"),
            "{message}"
        );
        assert!(
            message.contains("voro done 62 --db '/tmp/other.db' --summary"),
            "{message}"
        );
    }

    #[test]
    fn dispatched_prompt_injects_the_intended_branch() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        store.set_branch(id, Some("feat/parser")).unwrap();

        dispatch(&mut store, &ctx, id, None).unwrap();

        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        assert!(prompt.contains("git branch `feat/parser`"), "{prompt}");
        // registered (`voro set`) and confirmed (`voro done`); id and `--branch`
        // asserted apart, since a `--db` flag may sit between them
        assert!(prompt.contains(&format!("voro set {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro done {id}")), "{prompt}");
        assert!(prompt.contains("--branch feat/parser"), "{prompt}");
    }

    #[test]
    fn dispatch_substitutes_the_task_id_into_the_command() {
        // the stub command writes {task_id} to a marker file, so a successful
        // run proves the placeholder reached the spawned command line
        let (mut store, ctx, project) = fixture("cat {prompt_file} && echo {task_id} > marker.txt");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();

        let marker = project.join("marker.txt");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            id.to_string()
        );
    }

    #[test]
    fn dispatch_without_the_task_id_placeholder_is_unchanged() {
        // a template that never mentions {task_id} dispatches exactly as before
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
    }

    fn prompt_files(ctx: &DispatchCtx) -> Vec<PathBuf> {
        std::fs::read_dir(&ctx.runtime_dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.to_string_lossy().ends_with(".prompt.md"))
            .collect()
    }

    #[test]
    fn redispatch_keeps_each_sessions_prompt_and_log() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();
        store.apply(id, voro_core::Action::Abort).unwrap();
        dispatch(&mut store, &ctx, id, None).unwrap();

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_ne!(
            sessions[0].log_path, sessions[1].log_path,
            "each session must own its log"
        );
        assert_eq!(prompt_files(&ctx).len(), 2);
    }

    #[test]
    fn a_dirty_checkout_dispatches() {
        // The dispatched agent works in a throwaway worktree that snapshots
        // HEAD, so uncommitted changes in the primary checkout do not block
        // dispatch: it proceeds and records the session.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        std::fs::write(project.join("scratch.txt"), "uncommitted").unwrap();

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
        assert_eq!(store.sessions_for(id).unwrap().len(), 1);
    }

    /// A task that names a second repo dispatches in *that* checkout — cwd,
    /// git guard, and the worktree the agent will make (DESIGN.md §8).
    #[test]
    fn a_task_with_a_repo_dispatches_in_that_checkout() {
        let (mut store, ctx, project) = fixture("pwd; : {prompt_file}");
        let id = ready_task(&mut store, &project);
        let second = project.parent().unwrap().join("second");
        std::fs::create_dir_all(&second).unwrap();
        git(&second, &["init", "-q"]);
        let task = store.task(id).unwrap();
        let repo = store
            .add_repo(task.project_id, "second", second.to_str().unwrap())
            .unwrap();
        store.set_task_repo(id, Some(repo.id)).unwrap();

        dispatch(&mut store, &ctx, id, None).unwrap();
        let log = store.sessions_for(id).unwrap()[0].log_path.clone().unwrap();
        // `pwd` writes the cwd the agent was spawned in.
        let printed = wait_for_log(&log);
        assert!(
            printed.trim().ends_with("second"),
            "expected the second repo as cwd, got {printed}"
        );
    }

    /// The git guard runs against the task's repo, not the project default, so
    /// a second checkout that is not a git repository is refused by name.
    #[test]
    fn a_non_git_second_repo_is_refused_naming_that_repo() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        let second = project.parent().unwrap().join("not-git");
        std::fs::create_dir_all(&second).unwrap();
        let task = store.task(id).unwrap();
        let repo = store
            .add_repo(task.project_id, "second", second.to_str().unwrap())
            .unwrap();
        store.set_task_repo(id, Some(repo.id)).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains(second.to_str().unwrap()), "{err}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn a_non_git_path_is_refused_naming_the_path() {
        // A path that is not a git repository is refused, with the path in the
        // error.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        std::fs::remove_dir_all(project.join(".git")).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains(project.to_str().unwrap()), "{err}");
        // nothing was dispatched
        assert_eq!(store.task(id).unwrap().state, TaskState::Ready);
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    #[test]
    fn unknown_agent_override_is_refused_before_spawning() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let err = dispatch(&mut store, &ctx, id, Some("gemini")).unwrap_err();
        assert!(err.contains("gemini"), "{err}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Ready);
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    #[test]
    fn only_ready_or_stalled_tasks_dispatch() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        store.apply(id, voro_core::Action::Park).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("ready or stalled"), "{err}");
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    /// Redispatching a stalled task is the whole point of the state (DESIGN.md
    /// §6/§8): dispatch's precondition accepts `stalled → running` too.
    #[test]
    fn a_stalled_task_redispatches() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();
        let session = &store.sessions_for(id).unwrap()[0];
        store.reconcile_session(session.id, false, false).unwrap();
        assert_eq!(store.task(id).unwrap().state, TaskState::Stalled);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
        assert_eq!(store.sessions_for(id).unwrap().len(), 2);
    }

    // --- session-ref capture (task #75) ---

    /// A canned `sessions` listing whose one entry matches the dispatched
    /// project's cwd with a far-future start time, so the first capture poll
    /// finds it — the stub for `claude agents --json`.
    fn write_sessions_json(root: &Path, project: &Path, session_ref: &str) -> PathBuf {
        let listing = root.join("sessions.json");
        std::fs::write(
            &listing,
            format!(
                "[{{\"pid\": 1, \"id\": \"short123\", \"cwd\": \"{}\", \
                 \"startedAt\": 99999999999999, \"sessionId\": \"{session_ref}\", \
                 \"status\": \"idle\", \"state\": \"working\"}}]",
                project.display()
            ),
        )
        .unwrap();
        listing
    }

    #[test]
    fn dispatch_captures_the_session_ref_from_the_sessions_listing() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nsessions = \"cat sessions.json\"\n",
        );
        let root = project.parent().unwrap().to_path_buf();
        let listing = write_sessions_json(&root, &project, "3f6c-full-uuid");
        // the sessions command runs in the project path
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}}\"\nsessions = \"cat '{}'\"\n",
                listing.display()
            ),
        )
        .unwrap();
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("ref 3f6c-full-uuid"), "{summary}");

        let session = &store.sessions_for(id).unwrap()[0];
        assert_eq!(session.session_ref.as_deref(), Some("3f6c-full-uuid"));
    }

    #[test]
    fn capture_gives_up_gracefully_when_nothing_matches() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nsessions = \"echo []\"\n",
        );
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("session ref not captured"), "{summary}");
        assert!(
            store.sessions_for(id).unwrap()[0].session_ref.is_none(),
            "the ref stays NULL when capture fails"
        );
        // the dispatch itself still succeeded
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn capture_falls_back_to_the_backgrounded_log_line() {
        // The dispatch stub prints the ANSI-coloured `backgrounded · <id>`
        // line a real `claude --bg` launcher prints; the sessions listing
        // never matches (empty array), so capture must fall back to the log.
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = 'cat {prompt_file} >/dev/null && printf \"\\033[2mbackgrounded · \\033[1mdeadbeef\\033[0m\\n\"'\n\
             sessions = \"echo []\"\n",
        );
        let id = ready_task(&mut store, &project);

        // give the stub time to write the line before capture's single poll
        let ctx = DispatchCtx {
            ref_capture_timeout: Duration::from_secs(3),
            message_grace: std::time::Duration::from_millis(300),
            ..ctx
        };
        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("ref deadbeef"), "{summary}");
        assert_eq!(
            store.sessions_for(id).unwrap()[0].session_ref.as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn strip_ansi_and_log_parsing_lift_the_backgrounded_id() {
        assert_eq!(
            strip_ansi("\u{1b}[38;5;245mbackgrounded · \u{1b}[1mdeadbeef\u{1b}[0m"),
            "backgrounded · deadbeef"
        );
        let log = std::env::temp_dir().join(format!("voro-bg-line-{}.log", std::process::id()));
        std::fs::write(
            &log,
            "warming up\n\u{1b}[2mbackgrounded · \u{1b}[1mcafe1234\u{1b}[0m\nmanage with: claude attach\n",
        )
        .unwrap();
        assert_eq!(log_backgrounded_ref(&log).as_deref(), Some("cafe1234"));
        std::fs::remove_file(&log).unwrap();
    }

    /// A ready task moved into `review` through the transition machine, so
    /// `open`'s state guard is exercised against a genuine review row.
    fn review_task(store: &mut Store, project_path: &Path) -> i64 {
        let id = ready_task(store, project_path);
        store.apply(id, voro_core::Action::Start).unwrap();
        store.apply(id, voro_core::Action::Complete(None)).unwrap();
        id
    }

    #[test]
    fn open_runs_the_configured_viewer_in_the_project_path() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // a [viewer] whose command drops a marker at the substituted {path}
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/opened.marker\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let summary = open(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains(&format!("opened task {id}")), "{summary}");

        // the viewer is spawned detached, so wait briefly for it to run
        let marker = project.join("opened.marker");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "the viewer should have run in the project");
    }

    /// Give the checkout one commit and a worktree on `branch` beside it, so
    /// `open` has a real worktree to resolve. Returns the worktree path.
    fn commit_and_worktree(project: &Path, branch: &str) -> PathBuf {
        git(project, &["config", "user.email", "t@example.com"]);
        git(project, &["config", "user.name", "Test"]);
        std::fs::write(project.join("README"), "hi\n").unwrap();
        git(project, &["add", "-A"]);
        git(project, &["commit", "-q", "-m", "init"]);
        let wt = project.parent().unwrap().join(format!("wt-{branch}"));
        git(
            project,
            &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
        );
        wt
    }

    #[test]
    fn open_runs_the_viewer_in_the_tasks_worktree_and_falls_back_to_the_checkout() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // a [viewer] whose command drops a marker at the substituted {path}
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/opened.marker\"\n",
        )
        .unwrap();
        let wt = commit_and_worktree(&project, "feat");

        // A task on `feat` has a live worktree, so the viewer opens there.
        let with_wt = review_task(&mut store, &project);
        store.set_branch(with_wt, Some("feat")).unwrap();
        open(&mut store, &ctx, with_wt, None).unwrap();
        let wt_marker = wt.join("opened.marker");
        for _ in 0..50 {
            if wt_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            wt_marker.exists(),
            "the viewer should have run in the worktree, not the checkout"
        );

        // A task on a branch with no worktree falls back to the checkout (a
        // second task in the same project, since the project name is unique).
        let project_id = store.task(with_wt).unwrap().project_id;
        let no_wt = store
            .create_task(NewTask {
                project_id,
                repo_id: None,
                title: "No worktree".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap()
            .id;
        store.apply(no_wt, voro_core::Action::Start).unwrap();
        store
            .apply(no_wt, voro_core::Action::Complete(None))
            .unwrap();
        store.set_branch(no_wt, Some("ghost")).unwrap();
        open(&mut store, &ctx, no_wt, None).unwrap();
        let checkout_marker = project.join("opened.marker");
        for _ in 0..50 {
            if checkout_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            checkout_marker.exists(),
            "a branch with no worktree must fall back to the checkout"
        );
    }

    #[test]
    fn open_substitutes_the_branch_and_base_placeholders() {
        // A viewer template using all three placeholders to spell a diff range;
        // {base} has no origin here, so it falls back to `main`.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"echo {base}...{branch} > {path}/range.txt\"\n",
        )
        .unwrap();
        let wt = commit_and_worktree(&project, "feat");
        let id = review_task(&mut store, &project);
        store.set_branch(id, Some("feat")).unwrap();

        open(&mut store, &ctx, id, None).unwrap();

        // {path} resolved to the worktree, {base}...{branch} to `main...feat`.
        let range = wt.join("range.txt");
        for _ in 0..50 {
            if range.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&range).unwrap().trim(),
            "main...feat"
        );
    }

    /// Wait for a detached viewer to have written its marker file, and return
    /// what it wrote.
    fn viewer_output(path: &Path) -> String {
        for _ in 0..50 {
            if path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(path).unwrap().trim().to_string()
    }

    /// A checkout with a viewer template spelling a diff range, a worktree on
    /// `feat`, and a review task on that branch. Returns the pieces plus the
    /// revision at the point the operator would have reviewed.
    fn range_fixture() -> (Store, DispatchCtx, PathBuf, PathBuf, i64, String) {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"echo {base}...{branch} > {path}/range.txt\"\n",
        )
        .unwrap();
        let wt = commit_and_worktree(&project, "feat");
        // The commit the first review looked at, then a rework commit on top.
        std::fs::write(wt.join("first"), "one\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "reviewed"]);
        let reviewed = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::fs::write(wt.join("second"), "two\n").unwrap();
        git(&wt, &["add", "-A"]);
        git(&wt, &["commit", "-q", "-m", "the rework"]);

        let id = review_task(&mut store, &project);
        store.set_branch(id, Some("feat")).unwrap();
        (store, ctx, project, wt, id, reviewed)
    }

    /// The whole point of delta re-review on the viewer medium (DESIGN.md §8):
    /// `{base}` binds to the revision the operator rejected, so the viewer opens
    /// the rework alone rather than the branch.
    #[test]
    fn open_narrows_the_range_to_the_reviewed_revision() {
        let (mut store, ctx, _project, wt, id, reviewed) = range_fixture();
        store.record_reviewed(id, &reviewed).unwrap();

        let summary = open(&mut store, &ctx, id, None).unwrap();
        assert!(
            summary.contains("since your last review"),
            "the summary must say what it narrowed to: {summary}"
        );
        assert_eq!(
            viewer_output(&wt.join("range.txt")),
            format!("{reviewed}...feat")
        );
    }

    /// A first review is untouched: no reviewed revision has been recorded, so
    /// `{base}` is still the checkout's default branch and nothing is said.
    #[test]
    fn open_leaves_a_first_review_diffing_against_the_base() {
        let (mut store, ctx, _project, wt, id, _reviewed) = range_fixture();

        let summary = open(&mut store, &ctx, id, None).unwrap();
        assert!(!summary.contains("review"), "{summary}");
        assert_eq!(viewer_output(&wt.join("range.txt")), "main...feat");
    }

    /// The force-push case: the recorded revision is no longer on the branch,
    /// so the range cannot be built. Degrade to the full diff with a notice —
    /// never an error.
    #[test]
    fn open_falls_back_to_the_base_when_the_reviewed_revision_is_gone() {
        let (mut store, ctx, _project, wt, id, _reviewed) = range_fixture();
        store
            .record_reviewed(id, "0123456789abcdef0123456789abcdef01234567")
            .unwrap();

        let summary = open(&mut store, &ctx, id, None).unwrap();
        assert!(
            summary.contains("no longer on the branch"),
            "the fallback must explain itself: {summary}"
        );
        assert_eq!(viewer_output(&wt.join("range.txt")), "main...feat");
    }

    /// Recording is what a reject does, and a task with a branch but no PR
    /// records the local tip of it (DESIGN.md §8).
    #[test]
    fn rejecting_records_the_branch_head_that_was_reviewed() {
        let (mut store, _ctx, _project, wt, id, _reviewed) = range_fixture();
        assert_eq!(store.last_reviewed(id).unwrap(), None);

        crate::pr::record_reviewed(&mut store, id);

        let head = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        assert_eq!(store.last_reviewed(id).unwrap().as_deref(), Some(&head[..]));
    }

    /// A task reviewed with neither a PR nor a branch has no revision to read,
    /// so the TUI starts no capture for it at all (DESIGN.md §8).
    #[test]
    fn a_task_with_no_pr_and_no_branch_has_nothing_to_capture() {
        let (mut store, _ctx, project) = fixture("cat {prompt_file}");
        let id = review_task(&mut store, &project);

        assert_eq!(crate::pr::reviewed_source(&store, id), None);
    }

    #[test]
    fn open_ignores_new_placeholders_a_template_does_not_use() {
        // A template that mentions neither {branch} nor {base} is substituted
        // exactly as before — only {path} changes, and it lands unquoted-content
        // in the checkout since the task carries no branch.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"echo plain > {path}/plain.txt\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project); // no branch set

        open(&mut store, &ctx, id, None).unwrap();

        let plain = project.join("plain.txt");
        for _ in 0..50 {
            if plain.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&plain).unwrap().trim(), "plain");
    }

    #[test]
    fn open_records_a_failing_viewers_exit_status_in_the_launch_log() {
        // a [viewer] whose command exits non-zero after spawning: the failure
        // is silent on the (null-less) detached child, so it must be findable
        // in the launch log, and the summary must say where that log is.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"exit 3\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let summary = open(&mut store, &ctx, id, None).unwrap();
        let launch_log = ctx.launch_log_path();
        assert!(
            summary.contains(&launch_log.display().to_string()),
            "{summary}"
        );

        // the reap thread records the exit status asynchronously
        let mut contents = String::new();
        for _ in 0..50 {
            contents = std::fs::read_to_string(&launch_log).unwrap_or_default();
            if contents.contains("exited with") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(contents.contains("viewer:"), "{contents}");
        assert!(contents.contains("exit status: 3"), "{contents}");
    }

    #[test]
    fn open_without_a_viewer_reports_what_to_configure() {
        // the fixture's voro.toml defines no viewer at all
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = review_task(&mut store, &project);

        let err = open(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("[viewers.<name>]"), "{err}");
        assert!(err.contains(ctx.agents_path.to_str().unwrap()), "{err}");
    }

    #[test]
    fn open_refuses_a_task_that_is_not_review_or_running() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project); // still ready

        let err = open(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("review or running"), "{err}");
    }

    /// The two named-viewer selection paths (DESIGN.md §8/§11a): an explicit
    /// override picks its `[viewers.<name>]` entry over the default, and with
    /// no override the project's `viewer:<name>` review action picks one.
    #[test]
    fn open_picks_the_named_viewer_from_override_or_project_action() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/default.marker\"\n\n\
             [viewers.special]\ncmd = \"touch {path}/special.marker\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        open(&mut store, &ctx, id, Some("special")).unwrap();
        let marker = project.join("special.marker");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "the override must pick [viewers.special]");
        std::fs::remove_file(&marker).unwrap();

        let project_id = store.task(id).unwrap().project_id;
        store
            .set_review_action(
                project_id,
                &voro_core::ReviewAction::Viewer(Some("special".into())),
            )
            .unwrap();
        open(&mut store, &ctx, id, None).unwrap();
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "the project's viewer:special action must pick [viewers.special]"
        );
        assert!(
            !project.join("default.marker").exists(),
            "the default [viewer] must not have run"
        );
    }

    /// An unknown viewer name errors naming the known ones rather than
    /// silently falling back to the default.
    #[test]
    fn open_reports_an_unknown_viewer_name() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewers.zed]\ncmd = \"true\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let err = open(&mut store, &ctx, id, Some("emacs")).unwrap_err();
        assert!(err.contains("emacs"), "{err}");
        assert!(err.contains("zed"), "{err}");
    }

    // --- refine (task #314) ---

    /// Read a prompt the stub agent copied out with `cat {prompt_file} > file`,
    /// waiting for the copy to *complete* rather than merely to start: the shell
    /// creates the redirect target before `cat` runs, so polling for existence
    /// races the write and can read an empty file on a loaded runner. The
    /// template's closing sentence is the marker that the whole prompt landed.
    fn read_captured_prompt(path: &Path) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.contains("Rewrite the body and stop.")
            {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {} to be written", path.display())
    }

    /// A proposal in the fixture project, optionally discovered from a parent
    /// task whose body and completion summary are the context a refine seeds
    /// its agent with.
    fn proposal(store: &mut Store, project_path: &Path, with_parent: bool) -> i64 {
        let p = store
            .create_project("proj", project_path.to_str().unwrap())
            .unwrap();
        let new = |title: &str, body: &str, state| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: body.into(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
        };
        let id = store
            .create_task(new(
                "Sloppy proposal",
                "make it better",
                TaskState::Proposed,
            ))
            .unwrap()
            .id;
        if with_parent {
            let parent = store
                .create_task(new(
                    "The parent task",
                    "parent body about the scheduler",
                    TaskState::Ready,
                ))
                .unwrap()
                .id;
            store.apply(parent, voro_core::Action::Start).unwrap();
            store
                .apply(
                    parent,
                    voro_core::Action::Complete(Some("what the parent actually did".into())),
                )
                .unwrap();
            store
                .add_dep(id, parent, voro_core::DepKind::DiscoveredFrom)
                .unwrap();
        }
        id
    }

    #[test]
    fn refine_spawns_the_agent_seeds_it_and_opens_the_round() {
        // The stub copies the prompt it was handed, so the file it writes is
        // exactly what reached the agent.
        let (mut store, ctx, project) = fixture("cat {prompt_file} > refine-prompt.txt");
        let id = proposal(&mut store, &project, true);

        let summary = refine(&mut store, &ctx, id, "  name the files it touches  ").unwrap();
        assert!(summary.contains("stub"), "{summary}");
        assert!(summary.contains(&format!("refine-{id}")), "{summary}");

        let prompt = read_captured_prompt(&project.join("refine-prompt.txt"));
        // the operator's note, the body as it stands, and the discovered-from
        // context the sloppy proposal is missing
        assert!(prompt.contains("name the files it touches"), "{prompt}");
        assert!(prompt.contains("Sloppy proposal"), "{prompt}");
        assert!(prompt.contains("make it better"), "{prompt}");
        assert!(
            prompt.contains("parent body about the scheduler"),
            "{prompt}"
        );
        assert!(prompt.contains("what the parent actually did"), "{prompt}");
        // the one write it is told to make, with the id and database literal,
        // and the retitle that may ride along on it (task #391)
        assert!(
            prompt.contains(&format!(
                "voro set {id} --db {} --body-file <path> [--title",
                shell_quote(&ctx.db_path)
            )),
            "{prompt}"
        );
        assert!(prompt.contains("a title-only `voro set`"), "{prompt}");
        // and the writes it is told not to make
        assert!(
            prompt.contains("do not change its state, priority, or"),
            "{prompt}"
        );
        assert!(prompt.contains("no `voro triage`"), "{prompt}");

        // Voro's own half opened the round: the task is out of the triage queue
        // with the body still the one the agent was handed, and the round has a
        // session carrying the pid the reconciler probes (DESIGN.md §6).
        let task = store.task(id).unwrap();
        assert_eq!(task.state, TaskState::Refining);
        assert_eq!(task.body, "make it better");
        assert_eq!(
            store.latest_refine_note(id).unwrap().as_deref(),
            Some("name the files it touches")
        );
        // and neither marker yet — the rewrite has not landed
        assert!(!store.refined_flag(id).unwrap());
        assert!(!store.refine_failed_flag(id).unwrap());

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, "stub");
        assert!(sessions[0].pid.is_some());
        assert!(sessions[0].ended_at.is_none());
        // This stub defines no `sessions` verb, so there is no listing to find
        // the round in and the pid is the only thing to probe it by.
        assert!(sessions[0].session_ref.is_none());
        assert!(
            sessions[0]
                .log_path
                .as_deref()
                .is_some_and(|p| p.contains(&format!("refine-{id}"))),
            "{:?}",
            sessions[0].log_path
        );
    }

    /// A round captures its session ref exactly as a dispatch does (task #379).
    /// It has to: the round renders the agent's *dispatch* template, so under a
    /// `--bg` launcher the pid on the row belongs to a launcher that exits at
    /// birth, and only the agent's own listing can say the round is still
    /// working. Without this reconcile read the dead launcher as a dead round
    /// and mismarked the rewrite that then landed (DESIGN.md §8).
    #[test]
    fn refine_captures_the_session_ref_from_the_sessions_listing() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"true\"\nsessions = \"true\"\n",
        );
        let root = project.parent().unwrap().to_path_buf();
        let listing = write_sessions_json(&root, &project, "refine-uuid");
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}} >/dev/null\"\nsessions = \"cat '{}'\"\n",
                listing.display()
            ),
        )
        .unwrap();
        let id = proposal(&mut store, &project, false);

        let summary = refine(&mut store, &ctx, id, "name the files").unwrap();
        assert!(summary.contains("ref refine-uuid"), "{summary}");
        assert_eq!(
            store.sessions_for(id).unwrap()[0].session_ref.as_deref(),
            Some("refine-uuid")
        );
        assert_eq!(store.task(id).unwrap().state, TaskState::Refining);
    }

    /// Capture is best-effort here as it is for a dispatch: a listing that
    /// matches nothing leaves the ref NULL, says so, and the round runs on.
    #[test]
    fn refine_survives_a_ref_it_cannot_capture() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file} >/dev/null\"\nsessions = \"echo []\"\n",
        );
        let id = proposal(&mut store, &project, false);

        let summary = refine(&mut store, &ctx, id, "name the files").unwrap();
        assert!(summary.contains("session ref not captured"), "{summary}");
        assert!(store.sessions_for(id).unwrap()[0].session_ref.is_none());
        assert_eq!(store.task(id).unwrap().state, TaskState::Refining);
    }

    /// A proposal linked to a plan document is refined against that plan — the
    /// same reason a dispatch preamble names it, and sharper here, since the
    /// rewritten body is what later dispatches read.
    #[test]
    fn refine_seeds_the_agent_with_the_tasks_linked_documents() {
        let (mut store, ctx, project) = fixture("cat {prompt_file} > refine-prompt.txt");
        let id = proposal(&mut store, &project, false);
        let project_id = store.task(id).unwrap().project_id;
        let doc = store
            .create_doc(project_id, None, "docs/PLAN.md", Some("The plan"))
            .unwrap();
        store.link_doc(id, doc.id).unwrap();

        refine(&mut store, &ctx, id, "follow the plan").unwrap();

        let prompt = read_captured_prompt(&project.join("refine-prompt.txt"));
        assert!(prompt.contains("Linked documents"), "{prompt}");
        assert!(prompt.contains("docs/PLAN.md"), "{prompt}");
    }

    // --- launch identity (task #326) ---

    /// Wait for a file the stub agent wrote to hold `until`, so an assertion
    /// about a detached launch races neither the spawn nor the write: the shell
    /// creates a redirect target before the command behind it produces
    /// anything, so mere existence is not enough.
    fn read_marker(path: &Path, until: &str) -> String {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.contains(until)
            {
                return text.trim().to_string();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {} to be written", path.display())
    }

    /// The defect this task fixes: a refine borrowed the dispatch template and
    /// left `{task_id}` unsubstituted, so every refine of every task launched a
    /// session called, literally, `voro-{task_id}`. Now each launch composes its
    /// own name, and the refine of a task cannot collide with its dispatch.
    #[test]
    fn a_refine_and_a_dispatch_of_one_task_carry_distinct_rendered_names() {
        // Each launch writes a marker named for its own session, so the two
        // cannot race over one file and the filenames themselves prove the
        // names differ.
        let (mut store, ctx, project) = fixture(
            "echo --name {session_name} --for {task_id} > {session_name}.txt; : {prompt_file}",
        );
        let id = proposal(&mut store, &project, false);

        refine(&mut store, &ctx, id, "sharpen it").unwrap();
        let refine_marker = read_marker(&project.join(format!("voro-{id}-refine.txt")), "--for");
        assert_eq!(refine_marker, format!("--name voro-{id}-refine --for {id}"));

        // The round has to end before a verdict is legal — a refining task is
        // out of the queue by construction (DESIGN.md §6).
        store
            .conclude_refine(id, voro_core::RefineOutcome::Applied)
            .unwrap();
        store
            .apply(id, voro_core::Action::Triage(voro_core::Triage::Ready))
            .unwrap();
        dispatch(&mut store, &ctx, id, None).unwrap();
        let dispatch_marker = read_marker(&project.join(format!("voro-{id}.txt")), "--for");
        assert_eq!(dispatch_marker, format!("--name voro-{id} --for {id}"));

        assert_ne!(refine_marker, dispatch_marker);
        for rendered in [&refine_marker, &dispatch_marker] {
            assert!(!rendered.contains('{'), "unsubstituted: {rendered}");
        }

        // The refine's whole command line is in the launch log, named for the
        // session the operator can attach to.
        let log = std::fs::read_to_string(ctx.launch_log_path()).unwrap();
        let launched = log
            .lines()
            .find(|l| l.contains(&format!("refine-{id}: echo")))
            .unwrap_or_else(|| panic!("no launch line in {log}"));
        assert!(
            launched.contains(&format!("voro-{id}-refine")),
            "{launched}"
        );
        assert!(!launched.contains('{'), "unsubstituted: {launched}");
    }

    /// Defect 3: the refine prompt used to substitute into the seed, so a task
    /// body discussing a command template was rewritten before the agent read
    /// it — corrupting the very subject of the rewrite.
    #[test]
    fn a_refine_hands_the_agent_a_body_full_of_placeholders_unchanged() {
        let (mut store, ctx, project) = fixture("cat {prompt_file} > refine-prompt.txt");
        let id = proposal(&mut store, &project, false);
        let body = "the template says voro-{task_id} and the flag is {db}, verbatim";
        let task = store.task(id).unwrap();
        store
            .update_task(
                id,
                voro_core::TaskEdit {
                    title: task.title,
                    body: body.into(),
                    priority: task.priority,
                    agent: task.agent,
                    human: task.human,
                    deep: task.deep,
                },
            )
            .unwrap();

        refine(&mut store, &ctx, id, "keep the {task_id} in the body").unwrap();

        let prompt = read_captured_prompt(&project.join("refine-prompt.txt"));
        assert!(prompt.contains(body), "{prompt}");
        assert!(
            prompt.contains("keep the {task_id} in the body"),
            "{prompt}"
        );
        // ...while the template's own placeholders still resolved around it.
        assert!(prompt.contains(&format!("voro set {id} --db")), "{prompt}");
    }

    /// The same guarantee on the dispatch preamble, whose untrusted values are
    /// the branch name and the linked documents' titles.
    #[test]
    fn a_dispatch_hands_the_agent_branch_and_doc_names_unchanged() {
        let (mut store, ctx, project) = fixture("cat {prompt_file} > dispatch-prompt.txt");
        let id = ready_task(&mut store, &project);
        let project_id = store.task(id).unwrap().project_id;
        store.set_branch(id, Some("feat/{task_id}")).unwrap();
        let doc = store
            .create_doc(project_id, None, "docs/{db}.md", Some("Plan for {task_id}"))
            .unwrap();
        store.link_doc(id, doc.id).unwrap();

        dispatch(&mut store, &ctx, id, None).unwrap();

        let prompt = read_marker(&project.join("dispatch-prompt.txt"), "Detailed prompt.");
        assert!(prompt.contains("git branch `feat/{task_id}`"), "{prompt}");
        assert!(prompt.contains("Plan for {task_id}"), "{prompt}");
        assert!(prompt.contains("docs/{db}.md"), "{prompt}");
        // and the preamble's own placeholders still resolved
        assert!(prompt.contains(&format!("voro ask {id} --db")), "{prompt}");
        assert!(prompt.contains("--branch feat/{task_id}"), "{prompt}");
    }

    #[test]
    fn refine_is_refused_off_the_brief_states_and_without_a_note() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = proposal(&mut store, &project, false);

        let err = refine(&mut store, &ctx, id, "  ").unwrap_err();
        assert!(err.contains("note"), "{err}");

        store
            .apply(id, voro_core::Action::Triage(voro_core::Triage::Parked))
            .unwrap();
        let err = refine(&mut store, &ctx, id, "too late").unwrap_err();
        assert!(err.contains("proposed or ready"), "{err}");
        // refused before anything spawned, so no prompt was even written
        assert!(!ctx.runtime_dir.exists());
        assert!(!store.refined_flag(id).unwrap());
    }

    /// The widening of task #352: a triaged task whose body the operator now
    /// wants rewritten refines like a proposal, and the round takes it out of
    /// the dispatchable queue while it runs (DESIGN.md §6).
    #[test]
    fn a_ready_task_refines_like_a_proposal() {
        let (mut store, ctx, project) = fixture("cat {prompt_file} > refine-prompt.txt");
        let id = proposal(&mut store, &project, false);
        store
            .apply(id, voro_core::Action::Triage(voro_core::Triage::Ready))
            .unwrap();

        refine(&mut store, &ctx, id, "name the files it touches").unwrap();
        assert_eq!(store.task(id).unwrap().state, TaskState::Refining);
        assert!(
            !store.candidates().unwrap().iter().any(|c| c.task.id == id),
            "a refining task is out of the scheduler's input"
        );

        store
            .conclude_refine(id, voro_core::RefineOutcome::Applied)
            .unwrap();
        assert_eq!(store.task(id).unwrap().state, TaskState::Proposed);
        assert!(store.refined_flag(id).unwrap());
    }

    #[test]
    fn interactive_refine_seeds_the_plan_session_with_the_task() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let id = proposal(&mut store, &project, false);

        let launch = plan_session(&store, &ctx, PlanTarget::Refine { task_id: id }).unwrap();
        assert_eq!(launch.label, "refine");
        assert_eq!(launch.cwd, project.to_str().unwrap());

        let prompt_path = prompt_files(&ctx).pop().unwrap();
        assert_eq!(
            launch.command,
            format!("stub --interactive {}", shell_quote(&prompt_path))
        );
        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        // the existing body is seeded, and the session ends in `set`, not `add`
        assert!(prompt.contains("make it better"), "{prompt}");
        assert!(
            prompt.contains(&format!(
                "voro set {id} --db {} --body-file <path> [--title",
                shell_quote(&ctx.db_path)
            )),
            "{prompt}"
        );
        assert!(prompt.contains("a title-only `voro set`"), "{prompt}");
        assert!(!prompt.contains("voro add"), "{prompt}");
        assert!(prompt.contains("interactive"), "{prompt}");

        // Assembling the launch opens no round: the round-trip does that once
        // the child has a pid, so a launch that never runs leaves the proposal
        // exactly where it was (DESIGN.md §6).
        assert_eq!(store.task(id).unwrap().state, TaskState::Proposed);
        assert!(store.sessions_for(id).unwrap().is_empty());
        // ...and it carries what the round-trip needs to open one.
        let refine = launch.refine.expect("a refine launch names its round");
        assert_eq!(refine.task_id, id);
        assert_eq!(refine.agent, "stub");
    }

    /// A planning session has no task to transition, so it earns no session row
    /// and names no round (DESIGN.md §8).
    #[test]
    fn a_planning_session_names_no_refine_round() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        let launch = plan_session(&store, &ctx, PlanTarget::Create { project_id: p.id }).unwrap();
        assert!(launch.refine.is_none());
    }

    #[test]
    fn interactive_refine_is_refused_off_the_brief_states() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let id = proposal(&mut store, &project, false);
        store
            .apply(id, voro_core::Action::Triage(voro_core::Triage::Parked))
            .unwrap();

        let err = plan_session(&store, &ctx, PlanTarget::Refine { task_id: id }).unwrap_err();
        assert!(err.contains("proposed or ready"), "{err}");
    }

    /// A round already in flight is not refinable either — the second launch is
    /// refused before it can supersede the first (DESIGN.md §6).
    #[test]
    fn refine_is_refused_on_a_task_already_refining() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let id = proposal(&mut store, &project, false);
        store
            .record_refine_launch(id, "first round", "stub", None, None)
            .unwrap();

        let err = refine(&mut store, &ctx, id, "second round").unwrap_err();
        assert!(err.contains("already being refined"), "{err}");
        let err = plan_session(&store, &ctx, PlanTarget::Refine { task_id: id }).unwrap_err();
        assert!(err.contains("already being refined"), "{err}");
        // one round, one session
        assert_eq!(store.sessions_for(id).unwrap().len(), 1);
    }

    // --- planning sessions (task #112) ---

    #[test]
    fn plan_session_assembles_the_launch_and_writes_the_prompt() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        // a dirty checkout is fine for planning
        std::fs::write(project.join("scratch.txt"), "uncommitted").unwrap();

        let launch = plan_session(&store, &ctx, PlanTarget::Create { project_id: p.id }).unwrap();
        assert_eq!(launch.cwd, project.to_str().unwrap());

        // the plan template ran through the same {prompt_file} substitution as
        // dispatch, pointing at a prompt written outside the checkout
        let prompt_path = prompt_files(&ctx).pop().unwrap();
        assert_eq!(
            launch.command,
            format!("stub --interactive {}", shell_quote(&prompt_path))
        );
        // the file is stemmed for the project by name, so the number in a
        // Voro-named session or file is always a task id
        let stem = prompt_path.file_name().unwrap().to_str().unwrap();
        assert!(stem.starts_with("plan-proj-"), "{stem}");

        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(prompt.contains("plan a new task"), "{prompt}");
        assert!(prompt.contains("`proj`"), "{prompt}");
        assert!(prompt.contains("voro add 'proj' \"<title>\""), "{prompt}");
        assert!(prompt.contains("--body-file"), "{prompt}");
        assert!(prompt.contains("acceptance criteria"), "{prompt}");
        assert!(prompt.contains("`proposed`"), "{prompt}");
        // the fixture database is not the default, so every command carries it
        assert!(
            prompt.contains(&format!("--db {}", shell_quote(&ctx.db_path))),
            "{prompt}"
        );

        // no dispatch happened: no session row, no state change, no task at all
        assert!(store.tasks().unwrap().is_empty());
    }

    #[test]
    fn planning_prompt_renders_the_db_flag_only_for_a_non_default_database() {
        let rendered = render_planning_prompt("proj", &Store::production_db_path());
        assert!(
            rendered.contains("voro add 'proj' \"<title>\" --body-file"),
            "{rendered}"
        );
        assert!(!rendered.contains("--db"), "{rendered}");
    }

    #[test]
    fn plan_without_a_plan_verb_reports_what_to_configure() {
        // the fixture's stub agent defines only dispatch
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();

        let err = plan_session(&store, &ctx, PlanTarget::Create { project_id: p.id }).unwrap_err();
        assert!(err.contains("stub"), "{err}");
        assert!(err.contains("plan"), "{err}");
        assert!(err.contains("{prompt_file}"), "{err}");
        assert!(err.contains(ctx.agents_path.to_str().unwrap()), "{err}");
    }

    // --- the deep flag (task #241) ---

    /// The stub command writes the model it was rendered with to a per-task
    /// marker file, so a successful run proves which model reached the spawned
    /// command line.
    fn model_marker(
        store: &mut Store,
        ctx: &DispatchCtx,
        project: &Path,
        project_id: i64,
        deep: bool,
    ) -> String {
        let id = store
            .create_task(NewTask {
                project_id,
                repo_id: None,
                title: "T".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep,
            })
            .unwrap()
            .id;
        dispatch(store, ctx, id, None).unwrap();
        let marker = project.join(format!("model-{id}.txt"));
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(&marker).unwrap().trim().to_string()
    }

    #[test]
    fn a_deep_task_dispatches_with_the_deeper_model() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file} && echo {model} > model-{task_id}.txt\"\n\
             model = \"workhorse\"\nmodel_deep = \"strongest\"\n",
        );
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap()
            .id;
        assert_eq!(
            model_marker(&mut store, &ctx, &project, p, false),
            "workhorse"
        );
        assert_eq!(
            model_marker(&mut store, &ctx, &project, p, true),
            "strongest"
        );
    }

    /// An agent whose templates carry no `{model}` — the built-in `codex`
    /// shape — takes no model direction, so a deep task dispatches with the
    /// same command as any other.
    #[test]
    fn an_agent_without_the_placeholder_dispatches_a_deep_task_identically() {
        let (mut store, ctx, project) =
            fixture("cat {prompt_file} && echo none > model-{task_id}.txt");
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap()
            .id;
        assert_eq!(model_marker(&mut store, &ctx, &project, p, false), "none");
        assert_eq!(model_marker(&mut store, &ctx, &project, p, true), "none");
    }

    /// Planning takes `model_plan` whatever the queue holds — a planning
    /// session belongs to no task, so there is no depth to read.
    #[test]
    fn plan_renders_its_own_model() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\n\
             plan = \"stub -i --model {model} {prompt_file}\"\n\
             model = \"workhorse\"\nmodel_deep = \"strongest\"\nmodel_plan = \"thinker\"\n",
        );
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        let launch = plan_session(&store, &ctx, PlanTarget::Create { project_id: p.id }).unwrap();
        assert!(
            launch.command.contains("--model thinker"),
            "{}",
            launch.command
        );
    }

    #[test]
    fn task_override_selects_the_agent() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // add a second agent and set it as the task's override
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [agents.special]\ncmd = \"cat {prompt_file}\"\n",
        )
        .unwrap();
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        let id = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "T".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: Some("special".into()),
                human: false,
                deep: false,
            })
            .unwrap()
            .id;

        dispatch(&mut store, &ctx, id, None).unwrap();
        assert_eq!(store.sessions_for(id).unwrap()[0].agent, "special");
    }
}
