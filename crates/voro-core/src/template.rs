//! The one substitution routine (DESIGN.md §8). Every template Voro fills —
//! agent command lines, dispatch preambles, planning and refine prompts — goes
//! through [`render`], because chained `String::replace` calls re-scan values
//! they have already emitted: whichever untrusted value goes in last, the ones
//! before it were searched for the placeholders that came after. A task body
//! discussing `{task_id}` is then silently rewritten before the agent sees it.
//!
//! Shell quoting lives here too, so command assembly owns it rather than
//! borrowing it from the TUI crate.

use std::path::Path;

/// Fill `template` from `bindings` in a single left-to-right pass: each bound
/// placeholder's value is emitted verbatim and never re-scanned, so a value
/// containing another placeholder survives whatever order the bindings are
/// given in. An unrecognised `{…}` is copied through untouched, which is what
/// makes composing nested blocks safe — render the inner block first, then bind
/// the finished text.
pub fn render(template: &str, bindings: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        // Longest match wins, so one placeholder that prefixes another cannot
        // shadow it whichever order the bindings arrive in.
        let matched = bindings
            .iter()
            .filter(|(name, _)| rest.starts_with(name))
            .max_by_key(|(name, _)| name.len());
        match matched {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Single-quote a path for safe substitution into an `sh -c` command line.
pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_placeholders_are_filled_and_unknown_ones_copied_through() {
        assert_eq!(
            render("run {a} then {b} and {c}", &[("{a}", "1"), ("{b}", "2")]),
            "run 1 then 2 and {c}"
        );
        assert_eq!(render("nothing to do", &[("{a}", "1")]), "nothing to do");
        assert_eq!(render("{a}{a}{a}", &[("{a}", "x")]), "xxx");
        assert_eq!(render("a { b } c", &[("{b}", "x")]), "a { b } c");
    }

    /// The defect this routine exists for: a value that itself contains a
    /// placeholder reaches the output as written, whichever order the bindings
    /// are given in. Chained `String::replace` cannot promise that.
    #[test]
    fn a_value_containing_another_placeholder_is_emitted_verbatim() {
        let body = "the note says {task_id} is wrong, and {db} too";
        for bindings in [
            vec![("{seed}", body), ("{task_id}", "42"), ("{db}", " --db x")],
            vec![("{task_id}", "42"), ("{db}", " --db x"), ("{seed}", body)],
        ] {
            assert_eq!(
                render("task {task_id}:\n{seed}\n{db}", &bindings),
                format!("task 42:\n{body}\n --db x")
            );
        }
    }

    #[test]
    fn the_longest_matching_placeholder_wins() {
        for bindings in [
            vec![("{project}", "p"), ("{project_arg}", "'p'")],
            vec![("{project_arg}", "'p'"), ("{project}", "p")],
        ] {
            assert_eq!(render("{project} {project_arg}", &bindings), "p 'p'");
        }
    }

    #[test]
    fn unicode_before_an_unbound_brace_is_not_split() {
        assert_eq!(render("— {x} — {y}", &[("{x}", "ok")]), "— ok — {y}");
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote(Path::new("/tmp/a b")), "'/tmp/a b'");
        assert_eq!(shell_quote(Path::new("/tmp/it's")), "'/tmp/it'\\''s'");
    }
}
