# TUI redesign — plan

This document is the deliverable of task #58: a reviewable plan for a visual and
structural pass over the cockpit, before any rendering code changes. It audits
what the TUI shows and binds today, proposes a screen structure and per-screen
key map, and mocks up each screen at 80 columns. The implementation is cut into
independently-landable tasks, filed as proposals linked `discovered-from` #58.

Two decisions were made upstream of this plan and are taken as given. First,
the box-drawing borders go: every region and popup is wrapped in a ratatui
`Borders::ALL` block, and the lines carry no information — structure should
come from whitespace, indentation, and type hierarchy instead. Second, key
overload is not solved by burying bindings in a help popup; instead, functions
that don't deserve a global key move to screens or views where they are local
and self-evident.

## 1. Audit

The TUI has two screens (`Screen::Cockpit`, `Screen::Tasks`, toggled with tab)
and nine modal popups (`Mode` in `app.rs`). All rendering is in
`crates/voro/src/ui.rs`.

The **cockpit** stacks five regions: a one-line header (`voro` plus each
project's `name:weight`), the queue in a full border titled "Next", the detail
pane in a full border titled "Detail", the running strip in a full border
titled "Running" (collapsed when nothing runs), and a one-line status/hint
line. On an 80×24 terminal the three borders spend six rows and four columns
of every line on chrome, and the title-in-border style means each region's
label interrupts a horizontal rule. The **tasks** screen is one full-border
list ("All tasks") plus the status line.

The nine popups: Weights, AddProject, PickProject, Transition, Prompt (the
generic one-line input used for answers and reject feedback), Detail (the task
view on the tasks screen), AgentPicker, Score, and History. All are bordered;
that is appropriate for floating overlays and is not the problem. The problems
are consistency: some titles embed their key help ("Weights — 0-5 weight,
r rename/path, d delete, esc to close"), others none (Score, PickProject);
close keys differ for no reason (Score closes on any key, History on
`h`/`esc`/`q`, Detail on `esc`/`q`, Weights on `esc`/`q`/`w`); and two popups
(Score, History) are read-only per-task views that arguably shouldn't be
popups at all.

The key map in `Mode::Normal` is global — every binding works on both screens
regardless of relevance: `q` quit, `tab` screen, `j`/`k` move, `r` refresh,
`⏎` contextual action, `n` new task, `e` edit, `s` state menu, `x` score,
`h` history, `d` dispatch, `D` dispatch-via-picker, `w` weights, `P`
add/edit project. The status line prints all of them at once — thirteen
items — so the most important context-sensitive hint (`⏎ answer`) drowns in
the middle of a line that never changes.

The style vocabulary is mostly coherent already and is worth writing down so
the redesign preserves it: yellow for scores, cyan for questions, magenta for
agents and the redispatch flag, red for status-line errors, dim for secondary
and terminal things, reversed for selection, bold for titles/emphasis.

## 2. Proposed structure

Three screens instead of two, no new popups, two popups retired.

**Cockpit** stays the home screen and keeps its shape — header, queue,
detail, running strip, status line — but loses the borders. Each region gets
a one-line section label in the established style (bold title, dim count,
e.g. `Next · 6`), regions separated by a single blank line. The empty queue
keeps its "nothing to do — press n" line. The detail pane absorbs the Score
popup: `x` toggles the score decomposition inline between the meta line and
the body (the queue is sorted by score; the natural question "why is this
first?" is answered in place, not in an overlay). History stays reachable
from the cockpit via `h` for now, rendered as today but restyled.

**Tasks** likewise keeps its content and loses the border. Its Detail popup
gains the same inline score toggle. History becomes a section at the bottom
of the Detail view (`h` toggles it) rather than a separate popup, since "what
happened to this task" is part of looking at the task.

**Projects** is new, replacing the Weights popup and the global `P` binding.
It is a screen because project administration is a real activity (weights
every morning, rename/re-path/delete occasionally) crammed today into a modal
with hidden sub-keys. Layout: one row per project — weight, name, path, open
task count — with the same direct manipulation the popup has now: `0`–`5`
sets the selected project's weight immediately (the morning ritual must stay
one keystroke per project), `r` renames/re-paths via the existing AddProject
form, `a` adds a project, `d` deletes (guarded, as now). Tab cycles
cockpit → tasks → projects; `1`/`2`/`3` jump directly.

**The status line becomes per-screen** and shows only what acts on the
current selection, styled as key-bold, label-dim pairs rather than one dim
string. Movement (`j`/`k`), `q`, and `tab` are printed on every screen —
nothing is hidden, the line just stops listing actions that don't apply.

- cockpit: `⏎ answer · d dispatch · D agent · s state · x score · h history · n new · e edit · tab tasks · q quit`
  (the `⏎ verb` reflects the selected row as today; `x`/`h` only shown when a
  task is selected)
- tasks: `⏎ view · s state · n new · e edit · tab projects · q quit`
- projects: `0-5 weight · r rename · a add · d delete · tab cockpit · q quit`

Global keys retired from `Mode::Normal`: `w` and `P` (moved to the projects
screen). `r` refresh stops being advertised (the TUI already auto-refreshes
on database change; the binding stays as a silent escape hatch). Every
function remains reachable; nothing moves into a help popup.

**Popup conventions**, applied to all that remain: single border kept (they
float over content, the border earns its place), title is the subject only
("Weights" → gone; "Transition #12", "Dispatch #12"), key help lives in a
one-line dim footer inside the popup instead of the title, and every popup
closes on `esc` plus its own opening key, nothing else. The Prompt input and
AddProject form keep their current behaviour restyled to the same convention.

## 3. Mockups

Cockpit, 80 columns. Borders replaced by section labels and blank lines;
selection shown by the reversed row (marked `▌` here):

```
voro  vore:3  mote:3
Next · 4
 12.4   #41 answer P1 vore: Collect answers via $EDITOR — one-line or editor?
▌11.2   #58 review P2 vore: Plan the TUI redesign: screen structure, key map…▐
  9.1   #43 start  P2 mote: Wire the importer into the nightly sync
  4.0   #57 triage P3 vore: Import open GitHub issues as proposed

#58 · vore · P2 · review · 11.2
Plan the TUI redesign: screen structure, key map, mockups, follow-up tasks

Voro's TUI works but reads as clunky next to modern terminal UIs like
Claude Code's. This task is a design pass, not an implementation: its
deliverable is a plan the operator can review, plus a set of proposed
follow-up tasks — one per coherent implementation chunk…

Running · 1
  #44 sonnet    running      3m07s  Reconcile dead-pid sessions

⏎ review · d dispatch · D agent · s state · x score · h history · n new · e edit · tab tasks · q quit
```

The same cockpit with the score toggle (`x`) open on the selected task — the
decomposition renders between the meta line and the body, dim, and `x` again
closes it:

```
#58 · vore · P2 · review · 11.2
Plan the TUI redesign: screen structure, key map, mockups, follow-up tasks
  weight 3 · P2 (value 2) · review (+1) · base w×(p+s) 9.0 · age 2.1d (+0.21)

Voro's TUI works but reads as clunky next to modern terminal UIs like…
```

Tasks screen:

```
All tasks · 58
  #58 review      P2 w3 vore           Plan the TUI redesign: screen structure…
▌ #57 proposed    P3 w3 vore           Import open GitHub issues as proposed  ▐
  #44 running     P1 w3 vore           Reconcile dead-pid sessions
   #5 parked      P2 w3 vore           Yank the focus task body  blocked by #3

⏎ view · s state · n new · e edit · tab projects · q quit
```

Projects screen:

```
Projects · 2
▌ 3  vore   ~/Projects/focus   12 open                                        ▐
  3  mote   ~/Projects/mote     4 open

0-5 weight · r rename · a add · d delete · tab cockpit · q quit
```

Detail popup on the tasks screen, with the history section toggled open —
representative of the popup convention (subject-only title, dim key footer):

```
┌ #57 ─────────────────────────────────────────────────────────────┐
│ Import open GitHub issues as proposed                            │
│ vore · P3 · proposed · w3                                        │
│                                                                  │
│ One-way capture: voro import <project> pulls open issues…        │
│                                                                  │
│ History                                                          │
│ 2026-07-08 09:12  created    imported from GH #21                │
│ 2026-07-08 09:12  proposed                                       │
│                                                                  │
│ ⏎ state · x score · h history · j/k scroll · esc close           │
└──────────────────────────────────────────────────────────────────┘
```

## 4. Implementation cut

Four tasks, filed as proposals linked to #58, ordered so each lands
independently and the diff stays reviewable. The first is the foundation the
others restyle against; two and three can land in either order; four goes
last because the final key line depends on which keys the middle two retire.

1. **Strip the borders and normalise the style vocabulary.** Remove
   `Borders::ALL` from the cockpit regions and the tasks list; introduce the
   section-label convention; apply the popup convention (subject-only titles,
   dim key footer, uniform esc-plus-toggle close) to all popups. Pure
   restyling, no keys change.
2. **Add the projects screen; retire the Weights popup and global `P`.**
   Three-screen tab cycle plus `1`/`2`/`3`; weight editing stays one
   keystroke per project.
3. **Fold score and history into the task views; retire the Score popup.**
   `x` toggles the decomposition inline in the cockpit detail pane and the
   tasks-screen Detail popup; history becomes a Detail-popup section.
4. **Contextual per-screen key line.** Key-bold label-dim spans, per-screen
   content as specified above; drop retired keys; keep the `⏎ verb` hint.

DESIGN.md §9 describes the cockpit's three regions and the interaction list
but does not prescribe borders or the key map, so it needs only a one-line
amendment when task 2 lands: the weights modal becomes a projects screen (the
"fast every morning" requirement carries over unchanged).
