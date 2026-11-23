//! What a task's agent sessions consumed (DESIGN.md §8), read from the
//! transcripts Claude Code keeps under `~/.claude/projects/` and recorded as a
//! `cost` event each time the task leaves `running`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::store::{Store, log_event};

/// Anthropic's per-token price ratios, in input-token equivalents. Prices
/// change; these ratios have not, which is why `weighted` is the unit.
pub const WEIGHT_INPUT: f64 = 1.0;
pub const WEIGHT_OUTPUT: f64 = 5.0;
pub const WEIGHT_CACHE_WRITE: f64 = 1.25;
pub const WEIGHT_CACHE_READ: f64 = 0.1;

/// The event kind a [`Cost`] is logged under, its detail the JSON form.
pub const COST_EVENT: &str = "cost";

/// One transition's worth of consumption, or the record that none could be
/// read. A task's total is the sum of its `cost` events: each one counts only
/// the transcript lines stamped after the previous event's `until` for the
/// same session, so a task that goes round twice is not counted twice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    #[serde(default)]
    pub messages: u64,
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub wall_secs: u64,
    #[serde(default)]
    pub weighted: u64,
    #[serde(default)]
    pub transcript_bytes: u64,
    /// The last timestamp counted, per session reference.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub until: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub missing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub looked_for: Option<String>,
}

impl Cost {
    pub fn parse(detail: &str) -> Option<Cost> {
        serde_json::from_str(detail).ok()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a Cost always serialises")
    }

    /// Fold another record into this one, for totals. Watermarks are not
    /// carried: a total is a reading, not a record to append.
    pub fn add(&mut self, other: &Cost) {
        self.messages += other.messages;
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.wall_secs += other.wall_secs;
        self.weighted += other.weighted;
        self.transcript_bytes += other.transcript_bytes;
    }

    /// `334 msgs  out 372k  cache-read 93.1M  weighted 12.3M  1h42m`, or the
    /// path that was looked for when there was no transcript.
    pub fn summary_line(&self) -> String {
        if self.missing {
            return format!(
                "no transcript — looked for {}",
                self.looked_for.as_deref().unwrap_or("?")
            );
        }
        format!(
            "{} msgs  out {}  cache-read {}  weighted {}  {}",
            self.messages,
            human_count(self.output),
            human_count(self.cache_read),
            human_count(self.weighted),
            human_duration(self.wall_secs)
        )
    }
}

pub fn weighted(input: u64, output: u64, cache_write: u64, cache_read: u64) -> u64 {
    (input as f64 * WEIGHT_INPUT
        + output as f64 * WEIGHT_OUTPUT
        + cache_write as f64 * WEIGHT_CACHE_WRITE
        + cache_read as f64 * WEIGHT_CACHE_READ)
        .round() as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

/// The assistant messages of one session's transcripts, keyed by API message
/// id. Claude Code writes one line per content block and repeats the whole
/// response's `usage` on each, so summing lines would count a response once
/// per block; the last line for an id wins.
#[derive(Debug, Default)]
pub struct Tally {
    messages: HashMap<String, Usage>,
    first: Option<String>,
    last: Option<String>,
    pub bytes: u64,
}

impl Tally {
    /// Fold one `.jsonl` transcript in, skipping lines stamped at or before
    /// `after`. Lines that do not parse, are not assistant turns, or carry no
    /// `usage` are skipped.
    pub fn add_transcript(&mut self, text: &str, after: Option<&str>) {
        self.bytes += text.len() as u64;
        for (n, line) in text.lines().enumerate() {
            let Ok(value) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if value.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(usage) = value.pointer("/message/usage") else {
                continue;
            };
            let timestamp = value.get("timestamp").and_then(Value::as_str);
            if let (Some(at), Some(after)) = (timestamp, after)
                && at <= after
            {
                continue;
            }
            let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
            let key = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("line:{}:{n}", self.bytes));
            self.messages.insert(
                key,
                Usage {
                    input: field("input_tokens"),
                    output: field("output_tokens"),
                    cache_read: field("cache_read_input_tokens"),
                    cache_write: field("cache_creation_input_tokens"),
                },
            );
            if let Some(at) = timestamp {
                if self.first.as_deref().is_none_or(|f| at < f) {
                    self.first = Some(at.to_string());
                }
                if self.last.as_deref().is_none_or(|l| at > l) {
                    self.last = Some(at.to_string());
                }
            }
        }
    }

    /// The last timestamp counted, the next event's watermark.
    pub fn last(&self) -> Option<&str> {
        self.last.as_deref()
    }

    pub fn into_cost(self) -> Cost {
        let mut sum = Usage::default();
        for u in self.messages.values() {
            sum.input += u.input;
            sum.output += u.output;
            sum.cache_read += u.cache_read;
            sum.cache_write += u.cache_write;
        }
        let wall_secs = match (
            self.first.as_deref().and_then(epoch_secs),
            self.last.as_deref().and_then(epoch_secs),
        ) {
            (Some(a), Some(b)) if b > a => (b - a) as u64,
            _ => 0,
        };
        Cost {
            messages: self.messages.len() as u64,
            input: sum.input,
            output: sum.output,
            cache_read: sum.cache_read,
            cache_write: sum.cache_write,
            wall_secs,
            weighted: weighted(sum.input, sum.output, sum.cache_write, sum.cache_read),
            transcript_bytes: self.bytes,
            ..Cost::default()
        }
    }
}

/// Every transcript file a Claude Code session left under `root`
/// (`~/.claude/projects`): `<dir>/<ref>.jsonl` in whichever project directory
/// holds it, and its subagents' `<dir>/<ref>/subagents/*.jsonl`. The directory
/// is searched rather than derived from a cwd because a session that enters a
/// worktree moves its transcript to the worktree's escaped path, which Voro
/// never learns.
pub fn transcripts(root: &Path, session_ref: &str) -> Vec<PathBuf> {
    if session_ref.is_empty() || session_ref.contains(['/', '\\']) || session_ref.starts_with('.') {
        return Vec::new();
    }
    let Ok(dirs) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for dir in dirs.flatten().map(|e| e.path()) {
        let main = dir.join(format!("{session_ref}.jsonl"));
        if main.is_file() {
            found.push(main);
        }
        if let Ok(subs) = std::fs::read_dir(dir.join(session_ref).join("subagents")) {
            found.extend(
                subs.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "jsonl")),
            );
        }
    }
    found
}

/// Seconds since the epoch for `YYYY-MM-DD[T ]HH:MM:SS[.fff][Z]`, the shapes
/// transcripts and the event log write. Both are UTC.
pub fn epoch_secs(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| ts.get(r)?.parse::<i64>().ok();
    let days = days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?);
    Some(days * 86_400 + num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?)
}

/// The Monday starting the ISO week `ts` falls in, as `YYYY-MM-DD`.
pub fn week_of(ts: &str) -> Option<String> {
    let days = epoch_secs(ts)?.div_euclid(86_400);
    // 1970-01-01 was a Thursday, three days after a Monday.
    let monday = days - (days + 3).rem_euclid(7);
    let (y, m, d) = civil_from_days(monday);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

// Howard Hinnant's civil-calendar algorithms.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `412`, `372k`, `93.1M`.
pub fn human_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// `1h42m`, `7m`, `40s`.
pub fn human_duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// One `cost` event with what the reports group it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEvent {
    pub task_id: i64,
    pub project_id: i64,
    pub title: String,
    pub at: String,
    pub cost: Cost,
}

impl Store {
    /// Read agent transcripts from `root` (normally `~/.claude/projects`) when
    /// a task leaves `running`. Unset, no `cost` event is logged.
    pub fn set_transcripts_root(&mut self, root: Option<PathBuf>) {
        self.transcripts_root = root;
    }

    /// Sum what the task's sessions consumed since its last `cost` event and
    /// log it as a new one (DESIGN.md §8). Every session the task has had is
    /// read, refine rounds and redispatches included, each from the watermark
    /// its previous event left. No transcript found is itself recorded, as a
    /// `missing` event naming where it looked. `None` when no root is set.
    pub fn record_cost(&mut self, task_id: i64) -> Result<Option<Cost>> {
        let Some(root) = self.transcripts_root.clone() else {
            return Ok(None);
        };
        let mut watermarks: BTreeMap<String, String> = BTreeMap::new();
        for event in self.events_for(task_id)? {
            if event.kind == COST_EVENT
                && let Some(prior) = event.detail.as_deref().and_then(Cost::parse)
            {
                watermarks.extend(prior.until);
            }
        }
        let mut refs: Vec<String> = self
            .sessions_for(task_id)?
            .into_iter()
            .filter_map(|s| s.session_ref)
            .collect();
        refs.sort();
        refs.dedup();

        let mut total = Cost::default();
        let mut found = false;
        for session_ref in &refs {
            let files = transcripts(&root, session_ref);
            if files.is_empty() {
                continue;
            }
            found = true;
            let mut tally = Tally::default();
            let after = watermarks.get(session_ref).map(String::as_str);
            for file in &files {
                if let Ok(text) = std::fs::read_to_string(file) {
                    tally.add_transcript(&text, after);
                }
            }
            let until = tally.last().or(after).map(str::to_string);
            total.add(&tally.into_cost());
            if let Some(until) = until {
                total.until.insert(session_ref.clone(), until);
            }
        }
        if !found {
            total.missing = true;
            total.looked_for = Some(match refs.as_slice() {
                [] => "no session reference recorded for this task".to_string(),
                _ => refs
                    .iter()
                    .map(|r| {
                        root.join("*")
                            .join(format!("{r}.jsonl"))
                            .display()
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
        log_event(&self.conn, task_id, COST_EVENT, Some(&total.to_json()))?;
        Ok(Some(total))
    }

    /// Every `cost` event, oldest first, optionally only those logged within
    /// the last `since_days` days.
    pub fn cost_events(&self, since_days: Option<u32>) -> Result<Vec<CostEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.task_id, t.project_id, t.title, e.at, e.detail
             FROM events e JOIN tasks t ON t.id = e.task_id
             WHERE e.kind = ?1
               AND (?2 IS NULL OR e.at >= datetime('now', '-' || ?2 || ' days'))
             ORDER BY e.id",
        )?;
        let rows = stmt.query_map(params![COST_EVENT, since_days], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (task_id, project_id, title, at, detail) = row?;
            if let Some(cost) = detail.as_deref().and_then(Cost::parse) {
                out.push(CostEvent {
                    task_id,
                    project_id,
                    title,
                    at,
                    cost,
                });
            }
        }
        Ok(out)
    }

    /// Each task's summed cost, for `list --cost`.
    pub fn cost_totals(&self) -> Result<HashMap<i64, Cost>> {
        let mut totals: HashMap<i64, Cost> = HashMap::new();
        for event in self.cost_events(None)? {
            totals.entry(event.task_id).or_default().add(&event.cost);
        }
        Ok(totals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(
        r#"{"type":"assistant","timestamp":"2026-11-23T10:00:00.000Z","message":{"id":"msg_a","usage":{"input_tokens":3,"output_tokens":100,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50}}}"#,
        "\n",
        r#"{"type":"user","timestamp":"2026-11-23T10:00:05.000Z","message":{"role":"user","content":"hi"}}"#,
        "\n",
        r#"{"type":"assistant","timestamp":"2026-11-23T10:00:10.000Z","message":{"id":"msg_a","usage":{"input_tokens":3,"output_tokens":100,"cache_read_input_tokens":1000,"cache_creation_input_tokens":50}}}"#,
        "\n",
        "{\"type\":\"assistant\",\"timestamp\":\"2026-11-23T10:00:20\n",
        r#"{"type":"assistant","timestamp":"2026-11-23T11:42:00.000Z","message":{"id":"msg_b","usage":{"input_tokens":2,"output_tokens":400,"cache_read_input_tokens":9000,"cache_creation_input_tokens":200}}}"#,
        "\n",
    );

    #[test]
    fn tally_sums_assistant_usage_once_per_message() {
        let mut tally = Tally::default();
        tally.add_transcript(FIXTURE, None);
        let cost = tally.into_cost();
        assert_eq!(cost.messages, 2, "the repeated msg_a line counts once");
        assert_eq!(cost.input, 5);
        assert_eq!(cost.output, 500);
        assert_eq!(cost.cache_read, 10_000);
        assert_eq!(cost.cache_write, 250);
        assert_eq!(cost.wall_secs, 6120);
        assert_eq!(cost.transcript_bytes, FIXTURE.len() as u64);
        // 5 + 5*500 + 1.25*250 + 0.1*10000
        assert_eq!(cost.weighted, 5 + 2500 + 313 + 1000);
        assert!(!cost.missing);
    }

    #[test]
    fn tally_skips_lines_up_to_the_watermark() {
        let mut tally = Tally::default();
        tally.add_transcript(FIXTURE, Some("2026-11-23T10:00:10.000Z"));
        assert_eq!(tally.last(), Some("2026-11-23T11:42:00.000Z"));
        let cost = tally.into_cost();
        assert_eq!(cost.messages, 1);
        assert_eq!(cost.output, 400);
        assert_eq!(cost.wall_secs, 0);
    }

    #[test]
    fn cost_round_trips_through_its_event_detail() {
        let mut cost = Cost {
            messages: 3,
            output: 9,
            ..Cost::default()
        };
        cost.until
            .insert("uuid".into(), "2026-11-23T10:00:00Z".into());
        assert_eq!(Cost::parse(&cost.to_json()), Some(cost));
        let missing = Cost {
            missing: true,
            looked_for: Some("/x/*/u.jsonl".into()),
            ..Cost::default()
        };
        assert_eq!(Cost::parse(&missing.to_json()), Some(missing));
    }

    #[test]
    fn transcripts_are_found_in_any_project_directory_with_subagents() {
        let root = tempfile::Builder::new()
            .prefix("voro-cost-")
            .tempdir()
            .unwrap()
            .keep();
        let wt = root.join("-home-x-proj--claude-worktrees-t");
        std::fs::create_dir_all(wt.join("uuid-1/subagents")).unwrap();
        std::fs::create_dir_all(root.join("-home-x-proj")).unwrap();
        std::fs::write(wt.join("uuid-1.jsonl"), "").unwrap();
        std::fs::write(wt.join("uuid-1/subagents/agent-a.jsonl"), "").unwrap();
        std::fs::write(root.join("-home-x-proj/other.jsonl"), "").unwrap();
        let found = transcripts(&root, "uuid-1");
        assert_eq!(
            found,
            vec![
                wt.join("uuid-1.jsonl"),
                wt.join("uuid-1/subagents/agent-a.jsonl")
            ]
        );
        assert!(transcripts(&root, "../uuid-1").is_empty());
    }

    fn line(id: &str, at: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","message":{{"id":"{id}","usage":{{"input_tokens":1,"output_tokens":{output},"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
        ) + "\n"
    }

    fn dispatched(root: Option<PathBuf>, session_ref: Option<&str>) -> (Store, i64) {
        use crate::{LivenessSource, NewTask, Priority, TaskState};
        let mut s = Store::open_in_memory().unwrap();
        s.set_transcripts_root(root);
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let id = s
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "t".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap()
            .id;
        let (_, sess) = s
            .record_dispatch(id, "claude", Some(1), LivenessSource::Pid, None)
            .unwrap();
        if let Some(r) = session_ref {
            s.set_session_ref(sess.id, r).unwrap();
        }
        (s, id)
    }

    fn costs(s: &Store, id: i64) -> Vec<Cost> {
        s.events_for(id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == COST_EVENT)
            .map(|e| Cost::parse(e.detail.as_deref().unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn each_exit_from_running_logs_only_the_work_since_the_last() {
        use crate::Action;
        let root = tempfile::Builder::new()
            .prefix("voro-cost-")
            .tempdir()
            .unwrap()
            .keep();
        let dir = root.join("-wt");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("uuid-9.jsonl");
        std::fs::write(&file, line("m1", "2026-11-23T10:00:00Z", 10)).unwrap();
        let (mut s, id) = dispatched(Some(root), Some("uuid-9"));

        s.apply(id, Action::Ask("A or B?".into())).unwrap();
        s.apply(id, Action::Resume).unwrap();
        let mut text = std::fs::read_to_string(&file).unwrap();
        text += &line("m2", "2026-11-23T10:05:00Z", 20);
        text += &line("m3", "2026-11-23T10:10:00Z", 30);
        std::fs::write(&file, text).unwrap();
        s.apply(id, Action::Complete(None)).unwrap();

        let costs = costs(&s, id);
        assert_eq!(costs.len(), 2);
        assert_eq!((costs[0].messages, costs[0].output), (1, 10));
        assert_eq!((costs[1].messages, costs[1].output), (2, 50));
        assert_eq!(costs[1].wall_secs, 300);
        assert_eq!(
            costs[1].until.get("uuid-9").map(String::as_str),
            Some("2026-11-23T10:10:00Z")
        );
        assert_eq!(s.cost_totals().unwrap()[&id].output, 60);
    }

    #[test]
    fn a_task_without_a_transcript_closes_with_a_missing_event() {
        use crate::{Action, TaskState};
        let root = tempfile::Builder::new()
            .prefix("voro-cost-")
            .tempdir()
            .unwrap()
            .keep();
        let (mut s, id) = dispatched(Some(root.clone()), Some("gone"));
        assert_eq!(
            s.apply(id, Action::Complete(None)).unwrap().state,
            TaskState::Review
        );
        let costs = costs(&s, id);
        assert_eq!(costs.len(), 1);
        assert!(costs[0].missing);
        assert_eq!(
            costs[0].looked_for.as_deref(),
            Some(root.join("*/gone.jsonl").to_str().unwrap())
        );
    }

    #[test]
    fn no_root_logs_no_cost() {
        let (mut s, id) = dispatched(None, Some("uuid"));
        s.apply(id, crate::Action::Abort).unwrap();
        assert!(costs(&s, id).is_empty());
    }

    #[test]
    fn calendar_helpers() {
        assert_eq!(epoch_secs("1970-01-02 00:00:01"), Some(86_401));
        assert_eq!(
            epoch_secs("2026-11-23T14:36:38.123Z"),
            epoch_secs("2026-11-23 14:36:38")
        );
        assert_eq!(
            week_of("2026-11-23 09:00:00").as_deref(),
            Some("2026-11-23")
        );
        assert_eq!(
            week_of("2026-11-29 23:59:59").as_deref(),
            Some("2026-11-23")
        );
        assert_eq!(
            week_of("2026-11-22 12:00:00").as_deref(),
            Some("2026-11-16")
        );
        assert_eq!(human_count(372_025), "372k");
        assert_eq!(human_count(93_087_006), "93.1M");
        assert_eq!(human_duration(6120), "1h42m");
    }
}
