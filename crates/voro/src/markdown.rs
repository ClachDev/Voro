//! A tiny, hand-rolled renderer for the markdown subset used in task bodies
//! and in the agent's own blocks on the detail card, matching the `editor.rs`
//! precedent of avoiding a parser dependency. It is presentation only:
//! `body_lines` turns a body string into styled ratatui lines for the TUI
//! popup and detail pane, and `wrap_lines` breaks those lines to a width for
//! callers that must prefix every visual line. Anything it does not understand
//! degrades to the literal text — content is never dropped or mangled — and
//! the parsing is deliberately isolated behind one pure function so a real
//! parser could be swapped in later.

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

/// Word-wrap already-styled lines to `width` columns, preserving each span's
/// style across the break. A word wider than the whole width is broken at the
/// width rather than overflowing. Callers that prefix every visual line — the
/// detail card's gutter blocks — need the breaks decided here, since the
/// `Paragraph` that renders the result repeats no prefix when it wraps; lines
/// returned from here fit by construction, so it has nothing left to wrap.
pub fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines;
    }
    lines
        .into_iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// The display width of one character, measured as ratatui measures it so
/// this wrapping and ratatui's rendering agree on widths.
fn char_width(ch: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*ch.encode_utf8(&mut buf)).width()
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line];
    }
    let cells: Vec<(char, Style, usize)> = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |ch| (ch, span.style, char_width(ch)))
        })
        .collect();

    let mut out = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let mut end = start;
        let mut used = 0;
        while end < cells.len() && used + cells[end].2 <= width {
            used += cells[end].2;
            end += 1;
        }
        // A single character wider than the whole width still has to advance.
        let end = end.max(start + 1);
        if end >= cells.len() {
            out.push(line_of(&cells[start..]));
            break;
        }
        // Break at the last space that fits, so a word is not split; the
        // whitespace at the break is consumed rather than starting a line.
        match cells[start..=end].iter().rposition(|c| c.0.is_whitespace()) {
            Some(rel) if rel > 0 => {
                let brk = start + rel;
                out.push(line_of(&cells[start..brk]));
                start = brk;
                while start < cells.len() && cells[start].0.is_whitespace() {
                    start += 1;
                }
            }
            _ => {
                out.push(line_of(&cells[start..end]));
                start = end;
            }
        }
    }
    out
}

/// Reassemble characters into a line, merging runs that share a style.
fn line_of(cells: &[(char, Style, usize)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for &(ch, style, _) in cells {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(ch),
            _ => spans.push(Span::styled(ch.to_string(), style)),
        }
    }
    Line::from(spans)
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
    fn wrapping_breaks_on_words_and_keeps_styles() {
        let out = wrap_lines(body_lines("a **strong** word here"), 10);
        assert_eq!(out.len(), 2);
        assert_eq!(text(&out[0]), "a strong");
        assert_eq!(text(&out[1]), "word here");
        // The bold survives the break it did not fall on.
        assert_eq!(
            parts(&out[0]),
            vec![
                ("a ".into(), false, false, false),
                ("strong".into(), true, false, false),
            ]
        );
        // A style spanning the break keeps both halves.
        let out = wrap_lines(body_lines("**one two three**"), 8);
        assert_eq!(out.len(), 2);
        assert!(parts(&out[0])[0].1 && parts(&out[1])[0].1);
        assert_eq!(text(&out[0]), "one two");
        assert_eq!(text(&out[1]), "three");
    }

    #[test]
    fn wrapping_leaves_short_lines_and_breaks_long_words() {
        let out = wrap_lines(body_lines("short\n\nalso short"), 20);
        assert_eq!(out.len(), 3);
        assert_eq!(text(&out[1]), "");
        // No whitespace to break on: the word is cut at the width.
        let out = wrap_lines(body_lines("supercalifragilistic"), 6);
        assert_eq!(out.len(), 4);
        assert_eq!(text(&out[0]), "superc");
        assert_eq!(
            out.iter().map(text).collect::<String>(),
            "supercalifragilistic"
        );
        // Every line fits the width it was given.
        assert!(out.iter().all(|l| l.width() <= 6));
    }

    #[test]
    fn blank_lines_are_preserved() {
        let out = body_lines("one\n\ntwo");
        assert_eq!(out.len(), 3);
        assert_eq!(text(&out[1]), "");
    }
}
