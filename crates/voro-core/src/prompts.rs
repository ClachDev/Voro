//! The prompts Voro writes for the agents it launches (DESIGN.md §8): the
//! return-path preamble prepended to a dispatched task, the rework message, and
//! the whole prompts of planning, quick-propose and refine sessions. Pure
//! rendering — the callers in the `voro` crate do the I/O.

use std::path::Path;

use crate::{Store, render, shell_quote};

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
task; drop it for a proposal that stands on its own. Finish with your work
committed on a branch and a `--summary` on `done`: that summary is the pull
request's description — `voro pr` opens the PR with it as the body — so write it
as one, a short account of what changed and why followed by how you verified it,
not a status line. Never modify the database with raw SQL, which would bypass
the state machine and event log.{branch}{docs}{milestones}{rework}

---

";

/// Shared by the rework message and the redispatch preamble so the
/// answer-the-feedback instruction cannot drift (DESIGN.md §8). A re-review is
/// narrowed to the diff since the rejected revision, so the summary is what
/// carries the operator from each of their points to the change that answers
/// it; without it they are back to rediscovering the rework from the diff.
const REWORK_SUMMARY_SENTENCE: &str = "Report with `voro done {task_id}{db} --summary \"...\"`. That summary is the \
     pull request's description, so write it as one — a PR body that answers \
     that feedback point by point, one item per point, saying what you changed \
     or why you did not. The operator re-reviews only the diff since the \
     revision they rejected, so that summary is what connects your changes to \
     their points.";

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

/// The `{milestones}` block for a task that counts toward a milestone
/// (DESIGN.md §3/§8): its nearest milestones downstream, so a follow-up the
/// agent files can join the milestone only when the milestone needs it.
const MILESTONES_TEMPLATE: &str = "\n\n\
This task counts toward the milestone(s) below:

{list}

A follow-up you propose takes `--blocks <id>` only if the milestone cannot pass without it.";

/// Shared by both branch blocks below so the early-registration instruction
/// cannot drift. `{name}` is the assigned branch or the `<name>` the
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

/// The `{branch}` block for a task that carries an intended git branch:
/// the agent is told the name, to do its work in a throwaway worktree on
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

/// The guidance every task-drafting prompt gives on what a task body must
/// carry, kept in one place so the four templates and the `voro-cli` skill
/// cannot drift apart (a test ties the skill to it). The list of what the agent
/// is given mirrors what the `voro` crate's `spawn_session` and [`render_preamble`] put in front
/// of the body; change it when they change.
macro_rules! body_guidance {
    () => {
        "\
Write the body as a dispatchable prompt. The agent starts in the project
checkout, so its harness loads the repo's agent instructions, and Voro also
gives it: how to report back, the branch and worktree rules, the linked
documents by location, the milestones the task counts toward, and a notice when
the task was sent back from review. It gets nothing from your session and
nothing from the tasks this one waits on.

The body states what is specific to this task: the outcome, the files, the
decisions already made, and concrete acceptance criteria. It does not restate
the repo's working rules (those belong in the repo's agent instructions), the
plan (link the document), or how to report back."
    };
}

/// [`body_guidance!`] as a value, for the test that checks the skill against it.
#[cfg(test)]
const BODY_GUIDANCE: &str = body_guidance!();

/// Rendered per planning session (DESIGN.md §8) and written as the whole
/// prompt — unlike dispatch there is no task body to prepend it to, because
/// producing one is the session's job. `{project}` is the project's name,
/// `{project_arg}` the same name shell-quoted for the `voro add` line, and
/// `{db}` the same conditional `--db` flag the return-path preamble renders,
/// for the same reason: the command must name its database literally.
const PLANNING_PROMPT_TEMPLATE: &str = "\
<!-- Voro planning session: the deliverable is a task, not a PR -->
You are an agent launched by Voro to help the operator plan a new {kind} for the
project `{project}`. This is an interactive planning conversation: ask the
operator what they want, and interview them until the {kind} is well defined —
scope, approach where it is settled, and what done looks like. Do not modify
the project's files; the checkout is there to read, so the {kind} can name real
files and code.

{shape}

When the operator confirms the draft, write the body to a file outside the
checkout and create the {kind} with:

    voro add {project_arg} \"<title>\"{db}{flag} --body-file <path>

Add `--priority <0-3>` if the operator wants something other than the default
P2. The task is created in `proposed` and the operator triages it from the
queue, so do not pass a state. If the operator decides against creating a
task, end the session without creating one — that is a no-op, not a failure.
Never modify Voro's database with raw SQL, which would bypass the state
machine and event log.
";

/// The body guidance of a planning session that drafts a task.
const PLANNING_TASK_SHAPE: &str = concat!(
    body_guidance!(),
    "

Say in the body where the task's evidence and outputs land — the pull request
description, a decision record, a gitignored results directory, whatever the
project's own conventions call for — so the agent does not default to
committing verification write-ups and work logs as new files. Ask the operator
where they belong if the project states no convention. Say too what the task
makes obsolete — existing files, docs, or tooling it replaces — and put
retiring them in the same task's scope."
);

/// The body guidance of a planning session that drafts a milestone (DESIGN.md
/// §3). `{db}` is rendered before the shape is.
const PLANNING_MILESTONE_SHAPE: &str = "\
A milestone is something the operator can watch the robot or the system do; a
fix or a refactor is a task, so say so if that is what the operator describes.
Run `voro milestones{db}` before drafting and tell the operator if one listed
already covers the outcome. Keep the title to four words or fewer, because it
is a label cut to 16 characters. Write the body as the acceptance statement:
what the operator will watch happen, in measurable terms. No agent executes a
milestone, so the body is not a prompt; the tasks that reach it are filed
later and block it.";

/// Rendered per quick propose (DESIGN.md §6/§8) and written as the whole
/// prompt: the operator's one line, expanded by a headless agent into a task it
/// files itself. Modelled on [`PLANNING_PROMPT_TEMPLATE`] and ending in the same
/// `voro add`, but with no operator to interview — `{intent}` is everything the
/// session gets. `{project}` is the project's name, `{project_arg}` the same
/// name shell-quoted for the `add` line, and `{db}` the conditional `--db` flag
/// every rendered verb carries, for the same reason: the command must name its
/// database literally.
const PROPOSE_PROMPT_TEMPLATE: &str = "\
<!-- Voro quick propose: the deliverable is a task, not a PR -->
You are an agent launched by Voro to turn the operator's one-line intent into a
proposed task for the project `{project}`. There is no conversation to be had —
this session is headless and the operator is elsewhere — so what they typed is
the whole brief:

    {intent}

{shape}

When the body is ready, write it to a file outside the checkout and create
exactly one task with:

    voro add {project_arg} \"<title>\"{db}{flag} --body-file <path>

File that task even when the intent is thin: infer what you can and say in the
body where you were guessing. A weak proposal is visible in the queue and the
operator can refine it, where a session that files nothing leaves no trace of
what they typed at all. Pass no state — the task is created in `proposed` and
the operator triages it from the queue. That one command is the only change you
may make: no second task, no `voro triage`, and never modify Voro's database
with raw SQL, which would bypass the state machine and event log.
";

/// The expansion guidance of a quick propose that files a task.
const PROPOSE_TASK_SHAPE: &str = concat!(
    "\
Expand it into a title and a body. The title names what the task achieves, in
one imperative line.

",
    body_guidance!(),
    "

The decisions already made are those the intent settles. The checkout you are
running in is there to read, so the body can name real files and code. Do not
modify it, and do not do the task itself — writing its brief is the job."
);

/// The expansion guidance of a quick propose that files a milestone (DESIGN.md
/// §3).
const PROPOSE_MILESTONE_SHAPE: &str = "\
Expand it into a milestone: something the operator can watch the robot or the
system do. Keep the title to four words or fewer, because it is a label cut to
16 characters. Write the body as the acceptance statement: what the
operator will watch happen, in measurable terms. No agent executes a milestone,
so the body is not a prompt; the tasks that reach it are filed later and block
it. The checkout you are running in is there to read, so the statement can name
real behaviour. Do not modify it, and do not do the work itself.";

/// What a create flow files (DESIGN.md §8/§9): an ordinary task, or a
/// milestone, which the agent files with `voro add --milestone`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filing {
    Task,
    Milestone,
}

impl Filing {
    pub fn kind(self) -> &'static str {
        match self {
            Filing::Task => "task",
            Filing::Milestone => "milestone",
        }
    }

    fn flag(self) -> &'static str {
        match self {
            Filing::Task => "",
            Filing::Milestone => " --milestone",
        }
    }
}

/// The note-driven refine (DESIGN.md §6): a headless agent rewrites a proposed
/// task's body to honour the operator's one-line note, and applies it through
/// `voro set --body-file` — the same "the CLI is the agent's interface" shape as
/// dispatch and planning. `{seed}` carries the task as it stands plus the
/// context of the task it was discovered from; `{note}` is the operator's brief.
const REFINE_PROMPT_TEMPLATE: &str = "\
<!-- Voro refine: the deliverable is a rewritten task body -->
You are an agent launched by Voro to rewrite the body of proposed task {task_id}
{shape}

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
{shape}

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

/// The body guidance of a note-driven refine of a task, continuing the
/// template's opening sentence.
const REFINE_TASK_SHAPE: &str = concat!(
    "\
as the operator's note below asks.

",
    body_guidance!(),
    "

The checkout you are running in is there to read, so the body can name real
files and code. Do not modify the project's files, and do not do the task
itself — writing its brief is the job."
);

/// The body guidance of a note-driven refine of a milestone (DESIGN.md §3),
/// continuing the template's opening sentence.
const REFINE_MILESTONE_SHAPE: &str = "\
so that it reads as the milestone's acceptance statement: what the operator will
watch the robot or the system do, in measurable terms. No agent executes a
milestone, so the body is not a prompt; the tasks that reach it are filed
separately and block it. Keep the title to four words or fewer, because it is
a label cut to 16 characters, and retitle a longer one. The checkout you are
running in is there to read, so the statement can name real behaviour. Do not
modify the project's files, and do not do the work itself.";

/// The body guidance of an interactive refine of a task.
const REFINE_PLAN_TASK_SHAPE: &str = body_guidance!();

/// The body guidance of an interactive refine of a milestone (DESIGN.md §3).
const REFINE_PLAN_MILESTONE_SHAPE: &str = "\
This task is a milestone: something the operator can watch the robot or the
system do. Keep the title to four words or fewer, because it is a label cut to
16 characters, and retitle a longer one. Write the body as the
acceptance statement: what the operator will watch happen, in measurable terms.
No agent executes a milestone, so the body is not a prompt; the tasks that
reach it are filed separately and block it.";

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
pub fn render_preamble(
    task_id: i64,
    db_path: &Path,
    branch: Option<&str>,
    docs: &[(String, String)],
    milestones: &[(i64, String)],
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
    // A task in no milestone renders no block, so its prompt is byte-for-byte
    // what it was before milestones existed.
    let milestones_block = if milestones.is_empty() {
        String::new()
    } else {
        let list = milestones
            .iter()
            .map(|(id, title)| format!("    #{id}  {title}"))
            .collect::<Vec<_>>()
            .join("\n");
        render(MILESTONES_TEMPLATE, &[("{list}", list.as_str())])
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
            ("{milestones}", milestones_block.as_str()),
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
pub fn render_planning_prompt(project: &str, db_path: &Path, filing: Filing) -> String {
    let db = db_flag(db_path);
    let shape = match filing {
        Filing::Task => PLANNING_TASK_SHAPE.to_string(),
        Filing::Milestone => render(PLANNING_MILESTONE_SHAPE, &[("{db}", db.as_str())]),
    };
    render(
        PLANNING_PROMPT_TEMPLATE,
        &[
            ("{shape}", shape.as_str()),
            ("{kind}", filing.kind()),
            ("{flag}", filing.flag()),
            ("{project_arg}", shell_quote(Path::new(project)).as_str()),
            ("{project}", project),
            ("{db}", db.as_str()),
        ],
    )
}

/// Fill [`PROPOSE_PROMPT_TEMPLATE`] for a concrete project and intent. The
/// intent is the operator's own text, so it goes through the single-pass
/// renderer for the same reason a refine's seed does: a line that mentions
/// `{db}` must reach the agent as it was typed.
pub fn render_propose_prompt(
    project: &str,
    db_path: &Path,
    intent: &str,
    filing: Filing,
) -> String {
    let shape = match filing {
        Filing::Task => PROPOSE_TASK_SHAPE,
        Filing::Milestone => PROPOSE_MILESTONE_SHAPE,
    };
    render(
        PROPOSE_PROMPT_TEMPLATE,
        &[
            ("{intent}", intent.trim()),
            ("{shape}", shape),
            ("{flag}", filing.flag()),
            ("{project_arg}", shell_quote(Path::new(project)).as_str()),
            ("{project}", project),
            ("{db}", db_flag(db_path).as_str()),
        ],
    )
}

/// The note-driven refine prompt for a task, or for a milestone when `milestone`
/// is set: [`REFINE_PROMPT_TEMPLATE`] filled through [`render_refine`].
pub fn render_refine_prompt(
    task_id: i64,
    db_path: &Path,
    seed: &str,
    note: &str,
    milestone: bool,
) -> String {
    let shape = if milestone {
        REFINE_MILESTONE_SHAPE
    } else {
        REFINE_TASK_SHAPE
    };
    render_refine(REFINE_PROMPT_TEMPLATE, shape, task_id, db_path, seed, note)
}

/// The interactive refine prompt for a task, or for a milestone when
/// `milestone` is set: [`REFINE_PLAN_PROMPT_TEMPLATE`] filled through
/// [`render_refine`] with no note.
pub fn render_refine_plan_prompt(
    task_id: i64,
    db_path: &Path,
    seed: &str,
    milestone: bool,
) -> String {
    let shape = if milestone {
        REFINE_PLAN_MILESTONE_SHAPE
    } else {
        REFINE_PLAN_TASK_SHAPE
    };
    render_refine(
        REFINE_PLAN_PROMPT_TEMPLATE,
        shape,
        task_id,
        db_path,
        seed,
        "",
    )
}

/// Fill a refine template for a concrete task: the literal task id in the `set`
/// command, the same conditional `--db` flag the other prompts render, the seed
/// context, and — for the note-driven flavour — the operator's note. The seed
/// carries the task's own body, so it goes through the single-pass renderer:
/// a body that discusses `{task_id}` or `{db}` must reach the agent as written,
/// and rewriting the subject of a rewrite is the one thing this must not do.
fn render_refine(
    template: &str,
    shape: &str,
    task_id: i64,
    db_path: &Path,
    seed: &str,
    note: &str,
) -> String {
    render(
        template,
        &[
            ("{shape}", shape),
            ("{seed}", seed),
            ("{note}", note.trim()),
            ("{task_id}", task_id.to_string().as_str()),
            ("{db}", db_flag(db_path).as_str()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The `voro-cli` skill states the same body guidance the prompt templates
    /// render, so an edit to one cannot leave the other behind. Wrapping is
    /// free to differ; the words are not.
    #[test]
    fn the_skill_carries_the_body_guidance_the_templates_render() {
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let skill = squash(include_str!(
            "../../../plugins/voro/skills/voro-cli/SKILL.md"
        ));
        let guidance = squash(BODY_GUIDANCE);
        assert!(
            skill.contains(&guidance),
            "SKILL.md must contain the body guidance verbatim:\n{guidance}"
        );
        for shape in [
            PLANNING_TASK_SHAPE,
            PROPOSE_TASK_SHAPE,
            REFINE_TASK_SHAPE,
            REFINE_PLAN_TASK_SHAPE,
        ] {
            assert!(squash(shape).contains(&guidance), "{shape}");
        }
    }

    #[test]
    fn preamble_renders_a_db_flag_only_for_a_non_default_database() {
        // a scratch (non-default) db renders --db on every verb, shell-quoted
        let db = PathBuf::from("/tmp/scratch/voro.db");
        let rendered = render_preamble(62, &db, None, &[], &[], false);
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
        let default = render_preamble(62, &Store::production_db_path(), None, &[], &[], false);
        assert!(default.contains("voro ask 62 --question"), "{default}");
        assert!(!default.contains("--db"), "{default}");
    }

    #[test]
    fn preamble_tells_both_cases_to_register_the_branch_early() {
        // no assigned branch: the agent is told to register the name it picks,
        // but branch-assignment wording and a completion `voro done --branch`
        // are absent, since no name is known.
        let plain = render_preamble(62, &Store::production_db_path(), None, &[], &[], false);
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
            let rendered =
                render_preamble(62, &Store::production_db_path(), branch, &[], &[], false);
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
        let linked = render_preamble(62, &db, None, &docs, &[], false);
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
        let plain = render_preamble(62, &db, None, &[], &[], false);
        assert!(!plain.contains("derives from the document"), "{plain}");
    }

    /// A redispatch of rejected work carries the answer-the-feedback
    /// instruction, and a task nobody rejected carries none (DESIGN.md §8).
    #[test]
    fn preamble_tells_a_redispatched_rework_to_answer_the_feedback() {
        let db = Store::production_db_path();
        let reworking = render_preamble(62, &db, None, &[], &[], true);
        assert!(
            reworking.contains("been through review once already"),
            "{reworking}"
        );
        assert!(reworking.contains("point by point"), "{reworking}");
        assert!(
            reworking.contains("pull request's description"),
            "a rework summary is a PR body too: {reworking}"
        );
        assert!(
            reworking.contains("voro done 62 --summary"),
            "the instruction must be copy-pasteable: {reworking}"
        );

        let first = render_preamble(62, &db, None, &[], &[], false);
        assert!(!first.contains("point by point"), "{first}");
    }

    /// Every dispatched prompt says what shape a completion summary takes: it
    /// is the pull request's description, since `voro pr` opens the PR with it
    /// as the body (DESIGN.md §8).
    #[test]
    fn preamble_says_the_summary_is_the_pr_description() {
        let plain = render_preamble(62, &Store::production_db_path(), None, &[], &[], false);
        // The template is hand-wrapped, so match the prose rather than where
        // the lines happen to break.
        let flowed = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flowed.contains("that summary is the pull request's description"),
            "{plain}"
        );
        assert!(flowed.contains("not a status line"), "{plain}");
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
        assert!(
            message.contains("pull request's description"),
            "the rework summary is the PR body: {message}"
        );
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
    fn planning_prompt_renders_the_db_flag_only_for_a_non_default_database() {
        let rendered = render_planning_prompt("proj", &Store::production_db_path(), Filing::Task);
        assert!(
            rendered.contains("voro add 'proj' \"<title>\" --body-file"),
            "{rendered}"
        );
        assert!(!rendered.contains("--db"), "{rendered}");
    }

    #[test]
    fn planning_prompt_asks_for_evidence_routing_and_supersession() {
        let rendered = render_planning_prompt("proj", &Store::default_db_path(), Filing::Task);
        assert!(
            rendered.contains("where the task's evidence and outputs land"),
            "{rendered}"
        );
        assert!(
            rendered.contains("gitignored results directory"),
            "{rendered}"
        );
        assert!(
            rendered.contains("if the project states no convention"),
            "{rendered}"
        );
        assert!(rendered.contains("makes obsolete"), "{rendered}");
        assert!(
            rendered.contains("retiring them in the same task's scope"),
            "{rendered}"
        );
    }
}
