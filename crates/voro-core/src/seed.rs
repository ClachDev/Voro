//! The dev store's fixture (DESIGN.md §5).
//!
//! Every row is written through the ordinary store and transition APIs, so the
//! fixture sits at the current schema and holds only states the machine of §6
//! can reach.
//!
//! The repo paths are fictional, under the data directory. Nothing here creates
//! them, so they are not git repositories and dispatch refuses them; `git init`
//! one by hand when a dev build needs to exercise dispatch.

use crate::error::Result;
use crate::model::{DepKind, LivenessSource, Priority, SessionOutcome, TaskState};
use crate::store::{NewTask, Store};
use crate::transition::{Action, Triage};

/// What a seed run wrote, for the CLI to report.
pub struct SeedSummary {
    pub projects: usize,
    pub tasks: usize,
}

/// True when a store has no projects — the condition for seeding a dev store
/// on first open, kept here so the caller does not have to know what "empty"
/// means.
pub fn is_empty(store: &Store) -> Result<bool> {
    Ok(store.projects()?.is_empty())
}

/// Fill an empty store with a board that exercises the interface: every task
/// state of §6, each dependency kind, live and dead sessions, documents, a
/// multi-repo project, an archived one, and a spread of ages so the queue's
/// ordering is something other than degenerate.
pub fn seed(store: &mut Store) -> Result<SeedSummary> {
    let mut tasks = 0;

    // --- voro: the busy project, and the only multi-repo one ---
    let voro = store.create_project("voro", &repo_path("voro"))?;
    store.set_weight(voro.id, 4)?;
    store.add_repo(voro.id, "voro-site", &repo_path("voro-site"))?;
    let design = store.create_doc(voro.id, None, "docs/DESIGN.md", Some("Design document"))?;
    store.create_doc(
        voro.id,
        None,
        "https://github.com/ClachDev/Voro/issues",
        Some("Issue tracker"),
    )?;

    let ready = ready_task(
        store,
        voro.id,
        "Cache the queue's score decomposition between draws",
        "The decomposition is recomputed for every row on every draw. Cache it against \
         the task's `state_since` and invalidate when the store's `data_version` moves.",
        Priority::P1,
    )?;
    store.link_doc(ready.id, design.id)?;
    age(store, ready.id, "-3 days")?;
    tasks += 1;

    let running = ready_task(
        store,
        voro.id,
        "Teach `voro import` to follow a transferred GitHub issue",
        "An issue moved between repositories imports as a fresh task, duplicating the one \
         already tracking it. Follow the transfer and reconcile against the existing task.",
        Priority::P1,
    )?;
    store.apply(running.id, Action::Start)?;
    store.set_branch(running.id, Some("import-follows-transfers"))?;
    // Fixture sessions record no pid: reconcile-on-read finalises a live
    // session whose process is gone, and an absent pid reads as liveness
    // unknown, which leaves the row standing.
    store.create_session(
        running.id,
        "claude",
        None,
        LivenessSource::Listing,
        Some("/tmp/voro-dev/import.log"),
    )?;
    age(store, running.id, "-40 minutes")?;
    tasks += 1;

    let asked = ready_task(
        store,
        voro.id,
        "Rename the cockpit's running strip",
        "The strip shows waiting rows too, so its name no longer describes it.",
        Priority::P2,
    )?;
    store.apply(asked.id, Action::Start)?;
    let asked_session =
        store.create_session(asked.id, "claude", None, LivenessSource::Listing, None)?;
    store.apply(
        asked.id,
        Action::Ask("Should the strip keep showing waiting rows once they have a PR?".into()),
    )?;
    store.set_session_ref(asked_session.id, "dev-fixture-asked")?;
    age(store, asked.id, "-5 hours")?;
    tasks += 1;

    let review = ready_task(
        store,
        voro.id,
        "Split the review keys: `g`/`pr` always GitHub, `o`/`open` always local",
        "One key that guesses the medium is a key you cannot predict. Split it.",
        Priority::P1,
    )?;
    store.apply(review.id, Action::Start)?;
    store.set_branch(review.id, Some("split-review-keys"))?;
    let review_session = store.create_session(
        review.id,
        "claude",
        None,
        LivenessSource::Listing,
        Some("/tmp/voro-dev/keys.log"),
    )?;
    store.apply(
        review.id,
        Action::Complete(Some(
            "Split the keys and updated §8. The `gh repo view` probe behind `auto` is gone.".into(),
        )),
    )?;
    store.set_pr(review.id, Some("https://github.com/ClachDev/Voro/pull/130"))?;
    store.record_reviewed(review.id, "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678")?;
    store.end_session(review_session.id, SessionOutcome::Completed)?;
    age(store, review.id, "-2 days")?;
    tasks += 1;

    let waiting = ready_task(
        store,
        voro.id,
        "Colour the cockpit queue's state column per state",
        "The state column is the one field read at a glance, and it is monochrome.",
        Priority::P2,
    )?;
    store.apply(waiting.id, Action::Start)?;
    store.set_branch(waiting.id, Some("colour-state-column"))?;
    store.apply(waiting.id, Action::Complete(None))?;
    store.set_pr(
        waiting.id,
        Some("https://github.com/ClachDev/Voro/pull/129"),
    )?;
    store.apply(waiting.id, Action::HandOff)?;
    age(store, waiting.id, "-6 days")?;
    tasks += 1;

    let stalled = ready_task(
        store,
        voro.id,
        "Backfill `reviewed_at` for tasks reviewed before #132",
        "Rows reviewed before the revision capture landed show as never reviewed.",
        Priority::P1,
    )?;
    store.apply(stalled.id, Action::Start)?;
    let dead = store.create_session(
        stalled.id,
        "claude",
        Some(0),
        LivenessSource::Listing,
        Some("/tmp/voro-dev/backfill.log"),
    )?;
    store.reconcile_session(dead.id, false, false)?;
    age(store, stalled.id, "-1 day")?;
    tasks += 1;

    let done = ready_task(
        store,
        voro.id,
        "Raise the running strip's height ceiling",
        "Four running rows and the strip starts clipping.",
        Priority::P2,
    )?;
    store.apply(done.id, Action::Start)?;
    store.apply(
        done.id,
        Action::Complete(Some("Ceiling raised to eight rows.".into())),
    )?;
    store.set_pr(done.id, Some("https://github.com/ClachDev/Voro/pull/134"))?;
    store.apply(done.id, Action::Accept)?;
    age(store, done.id, "-9 days")?;
    tasks += 1;

    let proposed = proposed_task(
        store,
        voro.id,
        "Inline diff pane for review rows",
        "Deferred in §11 behind the viewer baseline. Revisit once `open` has bedded in.",
        Priority::P2,
    )?;
    store.add_dep(proposed.id, done.id, DepKind::DiscoveredFrom)?;
    age(store, proposed.id, "-12 hours")?;
    tasks += 1;

    let refining = proposed_task(
        store,
        voro.id,
        "Session-log retention",
        "Logs are kept indefinitely. Decide whether that needs to change.",
        Priority::P3,
    )?;
    store.apply(
        refining.id,
        Action::Refine("Narrow this to a decision with a threshold, not an open question.".into()),
    )?;
    store.create_session(refining.id, "claude", None, LivenessSource::Listing, None)?;
    tasks += 1;

    let parked = ready_task(
        store,
        voro.id,
        "Replace rusqlite with sqlx",
        "Rejected in spirit by §5's boring-dependencies rule, kept parked as a reminder of why.",
        Priority::P3,
    )?;
    store.apply(parked.id, Action::Park)?;
    age(store, parked.id, "-21 days")?;
    tasks += 1;

    let rejected = proposed_task(
        store,
        voro.id,
        "Rewrite the TUI in egui",
        "A GUI is a different product, not a refinement of this one.",
        Priority::P3,
    )?;
    store.apply(rejected.id, Action::Triage(Triage::Reject))?;
    tasks += 1;

    let human = store.create_task(NewTask {
        project_id: voro.id,
        repo_id: None,
        title: "Decide the v0.2.0 release cut".into(),
        body: "Desk work: pick the cut line, write the changelog entry, tag it.".into(),
        priority: Priority::P1,
        state: TaskState::Ready,
        agent: None,
        human: true,
        deep: false,
    })?;
    store.link_doc(human.id, design.id)?;
    age(store, human.id, "-4 days")?;
    tasks += 1;

    // --- mote: exercises the blocker demotion of §6 ---
    let mote = store.create_project("mote", &repo_path("mote"))?;
    store.set_weight(mote.id, 3)?;

    let blocker = ready_task(
        store,
        mote.id,
        "Recover the fleet API's heartbeat after a network partition",
        "The heartbeat never resumes once the socket drops mid-flight.",
        Priority::P0,
    )?;
    age(store, blocker.id, "-2 days")?;
    tasks += 1;

    // An open blocker demotes this to `parked` (§6): the machine chooses this
    // row's state, the fixture does not.
    let blocked = ready_task(
        store,
        mote.id,
        "Ship mission-layer v0 over the fleet API",
        "Blocked until the heartbeat survives a partition.",
        Priority::P1,
    )?;
    store.add_dep(blocked.id, blocker.id, DepKind::Blocks)?;
    tasks += 1;

    let mote_done = ready_task(
        store,
        mote.id,
        "Pin the telemetry schema to a version header",
        "",
        Priority::P2,
    )?;
    store.apply(mote_done.id, Action::Start)?;
    store.apply(mote_done.id, Action::Complete(None))?;
    store.apply(mote_done.id, Action::Accept)?;
    age(store, mote_done.id, "-16 days")?;
    tasks += 1;

    // --- AugereAI: the triage queue, plus the layout's awkward cases ---
    let augere = store.create_project("AugereAI", &repo_path("AugereAI"))?;
    store.set_weight(augere.id, 2)?;

    let deep = store.create_task(NewTask {
        project_id: augere.id,
        repo_id: None,
        title: "MCP front door v0: discover plus dispatch over the Mote fleet API".into(),
        body: "Two verbs, one schema, no state of its own.".into(),
        priority: Priority::P1,
        state: TaskState::Ready,
        agent: Some("claude".into()),
        human: false,
        deep: true,
    })?;
    store.apply(deep.id, Action::Start)?;
    store.set_branch(deep.id, Some("mcp-front-door"))?;
    store.apply(
        deep.id,
        Action::Complete(Some("Both verbs land; schema in the spec repo.".into())),
    )?;
    store.set_pr(
        deep.id,
        Some("https://github.com/ClachDev/AugereAI/pull/193"),
    )?;
    age(store, deep.id, "-8 days")?;
    tasks += 1;

    let long = proposed_task(
        store,
        augere.id,
        "Capability schema v0 covering zone vocabulary, mission envelopes, and the \
         negotiated subset a robot advertises when it joins a fleet it has not seen before",
        "Long deliberately: the queue and detail pane both have to survive a title that \
         does not fit.",
        Priority::P2,
    )?;
    age(store, long.id, "-30 hours")?;
    tasks += 1;

    let unicode = proposed_task(
        store,
        augere.id,
        "Zone vocabulary — ünïcöde, emoji 🤖, and CJK 機械 in one title",
        "Also deliberate: column widths are computed in characters, not bytes.",
        Priority::P3,
    )?;
    store.add_dep(unicode.id, long.id, DepKind::Parent)?;
    tasks += 1;

    let related = proposed_task(
        store,
        augere.id,
        "Spec repo v0: capability schema and mission API",
        "",
        Priority::P2,
    )?;
    store.add_dep(related.id, long.id, DepKind::Related)?;
    age(store, related.id, "-3 days")?;
    tasks += 1;

    // --- ODM: a quiet project, low weight ---
    let odm = store.create_project("ODM", &repo_path("ODM"))?;
    store.set_weight(odm.id, 1)?;
    let odm_ready = ready_task(
        store,
        odm.id,
        "Compare dense cloud counts across two SRT-corrected runs",
        "One run proves nothing here; the pipeline is bimodal.",
        Priority::P2,
    )?;
    age(store, odm_ready.id, "-11 days")?;
    tasks += 1;

    // --- an archived project, to prove archived rows stay out of the queue ---
    let old = store.create_project("clachdev-site", &repo_path("clachdev-site"))?;
    store.set_weight(old.id, 0)?;
    let old_task = ready_task(store, old.id, "Publish the launch page", "", Priority::P3)?;
    store.apply(old_task.id, Action::Start)?;
    store.apply(old_task.id, Action::Complete(None))?;
    store.apply(old_task.id, Action::Accept)?;
    age(store, old_task.id, "-60 days")?;
    store.set_archived(old.id, true)?;
    tasks += 1;

    Ok(SeedSummary { projects: 5, tasks })
}

/// A fictional checkout under the data directory. Never created, so dispatch
/// refuses it — see the module note.
fn repo_path(name: &str) -> String {
    Store::data_dir()
        .join("dev-repos")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn ready_task(
    store: &mut Store,
    project_id: i64,
    title: &str,
    body: &str,
    priority: Priority,
) -> Result<crate::model::Task> {
    store.create_task(NewTask {
        project_id,
        repo_id: None,
        title: title.into(),
        body: body.into(),
        priority,
        state: TaskState::Ready,
        agent: None,
        human: false,
        deep: false,
    })
}

fn proposed_task(
    store: &mut Store,
    project_id: i64,
    title: &str,
    body: &str,
    priority: Priority,
) -> Result<crate::model::Task> {
    store.create_task(NewTask {
        project_id,
        repo_id: None,
        title: title.into(),
        body: body.into(),
        priority,
        state: TaskState::Proposed,
        agent: None,
        human: false,
        deep: false,
    })
}

/// Backdate a task so the fixture's queue has a spread of ages to sort by;
/// `state_since` is the age input to the score (§7). Fixture setup writes it
/// directly. Task *state* is never written this way.
fn age(store: &mut Store, task_id: i64, offset: &str) -> Result<()> {
    store.conn.execute(
        "UPDATE tasks SET state_since = datetime('now', ?1), created_at = datetime('now', ?1) \
         WHERE id = ?2",
        rusqlite::params![offset, task_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState;

    fn seeded() -> Store {
        let mut store = Store::open_in_memory().unwrap();
        seed(&mut store).unwrap();
        store
    }

    #[test]
    fn every_task_state_appears_on_the_board() {
        let store = seeded();
        let tasks = store.tasks().unwrap();
        for state in TaskState::ALL {
            assert!(
                tasks.iter().any(|t| t.state == state),
                "no task in {state} — the fixture exists to exercise every state"
            );
        }
    }

    #[test]
    fn no_live_session_records_a_pid() {
        // Reconcile-on-read finalises a live session whose process is gone, so
        // a fixture that recorded the seeding process's pid would decay its own
        // `running` and `refining` rows on the very next command.
        let store = seeded();
        for session in store.live_sessions().unwrap() {
            assert_eq!(session.pid, None, "session {} records a pid", session.id);
        }
    }

    #[test]
    fn the_board_covers_every_dependency_kind() {
        let store = seeded();
        let deps: Vec<_> = store
            .deps_by_task()
            .unwrap()
            .into_values()
            .flatten()
            .collect();
        for kind in DepKind::ALL {
            assert!(deps.iter().any(|d| d.kind == kind), "no {kind} dependency");
        }
    }

    #[test]
    fn an_open_blocker_leaves_its_dependent_parked() {
        // Not written as `parked`: the fixture creates it ready and the machine
        // demotes it, which is the behaviour worth having on the board.
        let store = seeded();
        let blocked = store
            .tasks()
            .unwrap()
            .into_iter()
            .find(|t| t.title.starts_with("Ship mission-layer"))
            .expect("the blocked task");
        assert_eq!(blocked.state, TaskState::Parked);
    }

    #[test]
    fn seeding_is_reproducible() {
        let first = seeded().tasks().unwrap();
        let second = seeded().tasks().unwrap();
        let titles = |ts: Vec<crate::model::Task>| {
            ts.into_iter()
                .map(|t| (t.title, t.state))
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(first), titles(second));
    }
}
