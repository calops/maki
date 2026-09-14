//! Greedy word wrap over the UI span shape.
//!
//! The Lua `maki.ui.wrap` glue in the parent module parses its argument into
//! this module's `SnapshotSpan`s and serializes the lines back. The rule is
//! documented on that function: `\n` is a hard break, otherwise the wrap is a
//! plain greedy fill, and cell widths come from `unicode-width`.

use maki_agent::{SnapshotLine, SnapshotSpan};
use unicode_width::UnicodeWidthChar;

/// One character of a line plus the input span it came from and its display
/// width in cells. Zero-width characters occupy no cell but stay in the text.
#[derive(Clone, Copy)]
struct Glyph {
    ch: char,
    span: usize,
    width: usize,
}

impl Glyph {
    fn new(ch: char, span: usize) -> Self {
        Self {
            ch,
            span,
            width: UnicodeWidthChar::width(ch).unwrap_or(0),
        }
    }

    fn is_space(&self) -> bool {
        self.ch.is_whitespace()
    }
}

/// Splits `spans` on `\n` into hard lines, then greedily wraps each one to
/// `width` display cells. Always yields at least one line, so an empty input
/// wraps to a single empty line.
pub(crate) fn wrap_lines(spans: &[SnapshotSpan], width: u16) -> Vec<SnapshotLine> {
    let max = usize::from(width);
    paragraphs(spans)
        .iter()
        .flat_map(|paragraph| wrap_paragraph(paragraph, max))
        .map(|glyphs| SnapshotLine {
            spans: to_spans(&glyphs, spans),
        })
        .collect()
}

/// Splits the text into hard paragraphs at every `\n`. An empty input and a
/// trailing `\n` both leave a trailing empty paragraph.
fn paragraphs(spans: &[SnapshotSpan]) -> Vec<Vec<Glyph>> {
    let mut out = vec![Vec::new()];
    for (span, s) in spans.iter().enumerate() {
        for ch in s.text.chars() {
            if ch == '\n' {
                out.push(Vec::new());
            } else if let Some(line) = out.last_mut() {
                line.push(Glyph::new(ch, span));
            }
        }
    }
    out
}

/// Greedy fill of one `\n`-free paragraph. A word that would overflow opens a
/// new line before it, and the whitespace that separated it is dropped. A word
/// wider than `max` is cut at `max`, and a single character wider than `max`
/// gets a line of its own so no text is lost.
fn wrap_paragraph(glyphs: &[Glyph], max: usize) -> Vec<Vec<Glyph>> {
    let mut lines: Vec<Vec<Glyph>> = Vec::new();
    let mut current: Vec<Glyph> = Vec::new();
    let mut width = 0usize;
    let mut pending: &[Glyph] = &[];
    let mut rest = glyphs;
    while let Some((run, tail)) = take_run(rest) {
        if run[0].is_space() {
            if current.is_empty() || tail.is_empty() {
                push_run(&mut current, &mut width, run, max, &mut lines);
            } else {
                pending = run;
            }
        } else {
            if !current.is_empty() && width + run_width(pending) + run_width(run) > max {
                lines.push(std::mem::take(&mut current));
                width = 0;
            } else if !current.is_empty() {
                push_run(&mut current, &mut width, pending, max, &mut lines);
            }
            pending = &[];
            push_run(&mut current, &mut width, run, max, &mut lines);
        }
        rest = tail;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Splits the leading run that is either all whitespace or all non-whitespace
/// from the rest.
fn take_run(glyphs: &[Glyph]) -> Option<(&[Glyph], &[Glyph])> {
    let space = glyphs.first()?.is_space();
    let end = glyphs
        .iter()
        .position(|glyph| glyph.is_space() != space)
        .unwrap_or(glyphs.len());
    Some(glyphs.split_at(end))
}

fn run_width(run: &[Glyph]) -> usize {
    run.iter().map(|glyph| glyph.width).sum()
}

/// Appends `run` to the open line, breaking whenever the next glyph would push
/// the line past `max`. A glyph wider than the whole line still lands on its
/// own line.
fn push_run(
    current: &mut Vec<Glyph>,
    width: &mut usize,
    run: &[Glyph],
    max: usize,
    lines: &mut Vec<Vec<Glyph>>,
) {
    for glyph in run {
        if *width > 0 && *width + glyph.width > max {
            lines.push(std::mem::take(current));
            *width = 0;
        }
        current.push(*glyph);
        *width += glyph.width;
    }
}

/// Rebuilds spans from a wrapped line, merging runs of glyphs that came from
/// the same input span and cloning each span's style once.
fn to_spans(glyphs: &[Glyph], source: &[SnapshotSpan]) -> Vec<SnapshotSpan> {
    let mut spans: Vec<SnapshotSpan> = Vec::new();
    let mut previous = None;
    for glyph in glyphs {
        if let Some(last) = spans.last_mut()
            && previous == Some(glyph.span)
        {
            last.text.push(glyph.ch);
            continue;
        }
        spans.push(SnapshotSpan {
            text: glyph.ch.to_string(),
            style: source[glyph.span].style.clone(),
        });
        previous = Some(glyph.span);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::SpanStyle;
    use test_case::test_case;

    const BOLD: &str = "bold";
    const ITALIC: &str = "italic";

    fn styled(text: &str, style: SpanStyle) -> SnapshotSpan {
        SnapshotSpan {
            text: text.into(),
            style,
        }
    }

    fn named(text: &str, style: &str) -> SnapshotSpan {
        styled(text, SpanStyle::Named(style.into()))
    }

    fn texts(lines: &[SnapshotLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect()
    }

    fn wrap(text: &str, width: u16) -> Vec<String> {
        texts(&wrap_lines(&[styled(text, SpanStyle::Default)], width))
    }

    #[test_case("hello world", 5, &["hello", "world"] ; "breaks_at_the_space")]
    #[test_case("hello world", 11, &["hello world"] ; "fits_on_one_line")]
    #[test_case("a  b", 10, &["a  b"] ; "interior_spacing_stays")]
    #[test_case("aaa   bb", 6, &["aaa", "bb"] ; "whitespace_at_the_break_is_dropped")]
    #[test_case("hi   ", 10, &["hi   "] ; "trailing_whitespace_stays")]
    fn ascii_wrap_cases(text: &str, width: u16, expected: &[&str]) {
        assert_eq!(wrap(text, width), expected);
    }

    #[test_case(4, &["abcd", "efgh", "ij"] ; "cuts_at_the_width")]
    #[test_case(1, &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] ; "width_one_breaks_every_character")]
    fn overlong_word_breaks_at_width(width: u16, expected: &[&str]) {
        assert_eq!(wrap("abcdefghij", width), expected);
    }

    #[test]
    fn explicit_newlines_are_hard_breaks() {
        assert_eq!(wrap("a\nbb\n\nccc", 80), ["a", "bb", "", "ccc"]);
        assert_eq!(wrap("a\n", 80), ["a", ""]);
        assert_eq!(wrap("", 80), [""]);
    }

    #[test_case(4, &["你好", "世界"] ; "two_per_line")]
    #[test_case(3, &["你", "好", "世", "界"] ; "one_per_line_when_only_two_fit")]
    fn wide_characters_count_two_cells(width: u16, expected: &[&str]) {
        assert_eq!(wrap("你好世界", width), expected);
    }

    #[test]
    fn span_style_survives_a_split() {
        let line = [named("abcdef", BOLD)];
        let lines = wrap_lines(&line, 3);
        assert_eq!(texts(&lines), ["abc", "def"]);
        for row in &lines {
            assert_eq!(row.spans.len(), 1);
            assert_eq!(row.spans[0].style, SpanStyle::Named(BOLD.into()));
        }
    }

    #[test]
    fn each_wrapped_span_keeps_its_own_style() {
        let line = [named("ab", BOLD), named("cd", ITALIC)];
        let lines = wrap_lines(&line, 1);
        assert_eq!(texts(&lines), ["a", "b", "c", "d"]);
        let styles: Vec<SpanStyle> = lines.iter().map(|row| row.spans[0].style.clone()).collect();
        assert_eq!(
            styles,
            [
                SpanStyle::Named(BOLD.into()),
                SpanStyle::Named(BOLD.into()),
                SpanStyle::Named(ITALIC.into()),
                SpanStyle::Named(ITALIC.into()),
            ]
        );
    }

    #[test]
    fn whitespace_across_a_span_boundary_wraps_like_plain_text() {
        let line = [named("hello ", BOLD), named("world", ITALIC)];
        let lines = wrap_lines(&line, 5);
        assert_eq!(texts(&lines), ["hello", "world"]);
    }
}
