//! A tiny, hand-rolled renderer for the markdown subset used in task bodies,
//! matching the `editor.rs` precedent of avoiding a parser dependency. It is
//! presentation only: `body_lines` turns a body string into styled ratatui
//! lines for the TUI popup and detail pane. Anything it does not understand
//! degrades to the literal text — content is never dropped or mangled — and it
//! is deliberately isolated behind one pure function so a real parser could be
//! swapped in later.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const BOLD: Modifier = Modifier::BOLD;

fn code_style() -> Style {
    Style::new().fg(Color::Cyan)
}

fn code_block_style() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// Render a task body into styled lines. Supported constructs: `**bold**`,
/// `` `inline code` ``, fenced code blocks (```), `#`–`###` headings, and
/// `-`/`*` bullets. Everything else — including malformed markup such as an
/// unclosed fence or an unmatched `**` — renders as its literal text.
pub fn body_lines(body: &str) -> Vec<Line<'static>> {
    let lines: Vec<&str> = body.lines().collect();

    // Fenced code blocks are matched in pairs of fence lines. An odd trailing
    // fence is unclosed, so it (and everything after it) stays normal text.
    let fence_idxs: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim_start().starts_with("```"))
        .map(|(i, _)| i)
        .collect();
    let mut is_fence = vec![false; lines.len()];
    let mut in_code = vec![false; lines.len()];
    let paired = fence_idxs.len() - fence_idxs.len() % 2;
    let mut k = 0;
    while k < paired {
        let (open, close) = (fence_idxs[k], fence_idxs[k + 1]);
        is_fence[open] = true;
        is_fence[close] = true;
        in_code[(open + 1)..close].fill(true);
        k += 2;
    }
    // An odd trailing fence is unclosed: emit its line verbatim (its backticks
    // must not be re-read as inline code) rather than opening a code block.
    let unclosed = (fence_idxs.len() % 2 == 1).then(|| *fence_idxs.last().unwrap());

    lines
        .iter()
        .enumerate()
        .map(|(i, &line)| {
            if is_fence[i] || in_code[i] {
                Line::from(Span::styled(line.to_string(), code_block_style()))
            } else if Some(i) == unclosed {
                Line::from(Span::raw(line.to_string()))
            } else {
                render_line(line)
            }
        })
        .collect()
}

/// Render a single non-code line: headings and bullets are block-level, the
/// rest is inline `**bold**` / `` `code` `` styling.
fn render_line(line: &str) -> Line<'static> {
    if let Some(text) = heading_text(line) {
        return Line::from(Span::styled(
            text.to_string(),
            Style::new().add_modifier(BOLD),
        ));
    }
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        let mut spans = vec![Span::raw("• ")];
        spans.extend(parse_inline(rest));
        return Line::from(spans);
    }
    Line::from(parse_inline(line))
}

/// The heading text for a `#`–`###` line, or `None` if it is not a heading.
/// The line may be just the hashes or `#`s followed by a space and text.
fn heading_text(line: &str) -> Option<&str> {
    for hashes in ["### ", "## ", "# "] {
        if let Some(rest) = line.strip_prefix(hashes) {
            return Some(rest);
        }
    }
    match line {
        "#" | "##" | "###" => Some(""),
        _ => None,
    }
}

/// Split a line into inline spans, styling `**bold**` and `` `inline code` ``.
/// An unmatched opener has no closer and is emitted as literal text.
fn parse_inline(text: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'`'
            && let Some(rel) = text[i + 1..].find('`')
        {
            let close = i + 1 + rel;
            flush(&mut spans, &mut plain);
            spans.push(Span::styled(text[i + 1..close].to_string(), code_style()));
            i = close + 1;
            continue;
        } else if bytes[i] == b'*'
            && bytes.get(i + 1) == Some(&b'*')
            && let Some(rel) = text[i + 2..].find("**")
        {
            let close = i + 2 + rel;
            flush(&mut spans, &mut plain);
            spans.push(Span::styled(
                text[i + 2..close].to_string(),
                Style::new().add_modifier(BOLD),
            ));
            i = close + 2;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut spans, &mut plain);
    spans
}

/// Emit any buffered plain text as a span and clear the buffer.
fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String) {
    if !plain.is_empty() {
        spans.push(Span::raw(std::mem::take(plain)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Flatten a line into `(content, is_bold, is_code, is_dim)` tuples so tests
    /// can assert on both text and styling.
    fn parts(line: &Line<'static>) -> Vec<(String, bool, bool, bool)> {
        line.spans
            .iter()
            .map(|s| {
                let bold = s.style.add_modifier.contains(Modifier::BOLD);
                let dim = s.style.add_modifier.contains(Modifier::DIM);
                let code = s.style.fg == Some(Color::Cyan);
                (s.content.to_string(), bold, code, dim)
            })
            .collect()
    }

    fn text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        let out = body_lines("just some plain text\nsecond line");
        assert_eq!(out.len(), 2);
        assert_eq!(
            parts(&out[0]),
            vec![("just some plain text".into(), false, false, false)]
        );
        assert_eq!(text(&out[1]), "second line");
    }

    #[test]
    fn bold_span() {
        let out = body_lines("a **strong** word");
        assert_eq!(
            parts(&out[0]),
            vec![
                ("a ".into(), false, false, false),
                ("strong".into(), true, false, false),
                (" word".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn inline_code_span() {
        let out = body_lines("run `cargo test` now");
        assert_eq!(
            parts(&out[0]),
            vec![
                ("run ".into(), false, false, false),
                ("cargo test".into(), false, true, false),
                (" now".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn mixed_bold_and_code_in_one_line() {
        let out = body_lines("**bold** and `code` mixed");
        assert_eq!(
            parts(&out[0]),
            vec![
                ("bold".into(), true, false, false),
                (" and ".into(), false, false, false),
                ("code".into(), false, true, false),
                (" mixed".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn headings_render_bold() {
        for line in ["# One", "## Two", "### Three"] {
            let out = body_lines(line);
            let p = parts(&out[0]);
            assert_eq!(p.len(), 1);
            assert!(p[0].1, "heading should be bold: {line}");
            assert!(!p[0].0.starts_with('#'), "hashes stripped: {line}");
        }
        // Four hashes is not a supported heading; it stays literal.
        let out = body_lines("#### Four");
        assert_eq!(text(&out[0]), "#### Four");
        assert!(!parts(&out[0])[0].1);
    }

    #[test]
    fn bullets_get_a_glyph() {
        for line in ["- item", "* item"] {
            let out = body_lines(line);
            assert_eq!(text(&out[0]), "• item");
        }
        // A bullet still gets inline styling on its content.
        let out = body_lines("- a **bold** point");
        assert_eq!(
            parts(&out[0]),
            vec![
                ("• ".into(), false, false, false),
                ("a ".into(), false, false, false),
                ("bold".into(), true, false, false),
                (" point".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn fenced_code_block_is_dimmed_and_inert() {
        let body = "before\n```\nlet **x** = `y`;\n```\nafter";
        let out = body_lines(body);
        assert_eq!(out.len(), 5);
        assert_eq!(text(&out[0]), "before");
        assert!(!parts(&out[0])[0].3);
        // Fence lines are dimmed.
        assert!(parts(&out[1])[0].3);
        // The enclosed line is dimmed and its markdown is inert (single span).
        let inner = parts(&out[2]);
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].0, "let **x** = `y`;");
        assert!(inner[0].3);
        assert!(!inner[0].1);
        assert!(parts(&out[3])[0].3);
        assert!(!parts(&out[4])[0].3);
    }

    #[test]
    fn unclosed_fence_renders_literally() {
        let body = "intro\n```\nlet x = **1**;";
        let out = body_lines(body);
        // No closing fence, so nothing enters code mode: the fence line is
        // literal and the following line keeps its inline styling.
        assert_eq!(text(&out[1]), "```");
        assert!(!parts(&out[1])[0].3, "unmatched fence line not dimmed");
        assert_eq!(
            parts(&out[2]),
            vec![
                ("let x = ".into(), false, false, false),
                ("1".into(), true, false, false),
                (";".into(), false, false, false),
            ]
        );
    }

    #[test]
    fn unmatched_bold_marker_is_literal() {
        let out = body_lines("this ** is not bold");
        assert_eq!(
            parts(&out[0]),
            vec![("this ** is not bold".into(), false, false, false)]
        );
    }

    #[test]
    fn unmatched_backtick_is_literal() {
        let out = body_lines("a lone ` backtick");
        assert_eq!(
            parts(&out[0]),
            vec![("a lone ` backtick".into(), false, false, false)]
        );
    }

    #[test]
    fn blank_lines_are_preserved() {
        let out = body_lines("one\n\ntwo");
        assert_eq!(out.len(), 3);
        assert_eq!(text(&out[1]), "");
    }
}
