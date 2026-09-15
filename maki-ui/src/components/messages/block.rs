//! Projection of transcript messages into the structured blocks the Lua
//! renderer receives, and the conversion of its render objects back into
//! ratatui lines.

use std::sync::Arc;

use ratatui::text::{Line, Span};

use maki_agent::types::InlineStyle;
use maki_agent::{SnapshotLine, SnapshotSpan, SpanColor, SpanStyle, ToolOutput, diff::DiffLine};
use maki_lua::{Decoration, RenderObject};

use super::raw;
use crate::components::tool_display::{
    HighlightRequest, SpinnerLine, ToolLines, resolve_span_style, spinner_span, spinner_style_name,
    spinner_token,
};
use crate::components::{DisplayMessage, DisplayRole, ToolStatus};
use crate::{
    components::{
        code_view::SectionFlags,
        tool_display::{INSTRUCTIONS_TOOL, instructions_header},
    },
    markdown::should_truncate,
};

/// Bounds a raw object's rectangle so a plugin cannot ask for an unbounded
/// run of placeholder cells.
const MAX_RAW_WIDTH: u16 = 256;
const MAX_RAW_HEIGHT: u16 = 128;
/// Painted where a raw sequence lands. The final writer overwrites it with
/// the sequence once the frame is drawn, so it must be a plain cell.
const PLACEHOLDER_CELL: &str = " ";

/// Lifecycle phases a projected block reports. They describe what Rust knows
/// about the block, not how a renderer should draw it.
const PHASE_COMPLETE: &str = "complete";
const PHASE_IN_PROGRESS: &str = "in_progress";
const PHASE_SUCCESS: &str = "success";
const PHASE_ERROR: &str = "error";

/// A raw sequence and where its rectangle starts inside the block's lines.
pub(crate) struct RawSpan {
    /// Source line index the span begins on, used to find its display row.
    pub row: usize,
    pub seq: String,
    pub width: u16,
    pub height: u16,
}

/// The kind name a renderer switches on.
pub(crate) fn kind(role: &DisplayRole) -> &'static str {
    match role {
        DisplayRole::User => "user",
        DisplayRole::Assistant => "assistant",
        DisplayRole::Thinking => "thinking",
        DisplayRole::Tool(_) => "tool",
        DisplayRole::Error => "error",
        DisplayRole::Done => "done",
    }
}

/// Blocks the Lua renderer may own. A tool block's render places the host's
/// native body via `maki.ui.transcript_tool()`, so its interactions and
/// metadata stay Rust-owned. Images do not disqualify a block:
/// `Segment::set_images` fills them in regardless of which side produced the
/// lines.
pub(crate) fn renderable(message: &DisplayMessage) -> bool {
    matches!(
        message.role,
        DisplayRole::User
            | DisplayRole::Assistant
            | DisplayRole::Thinking
            | DisplayRole::Tool(_)
            | DisplayRole::Error
            | DisplayRole::Done
    )
}

/// The block's lifecycle phase as Rust knows it, not a rendering hint.
fn phase(role: &DisplayRole) -> &'static str {
    match role {
        DisplayRole::Tool(tool) => match tool.status {
            ToolStatus::InProgress => PHASE_IN_PROGRESS,
            ToolStatus::Success => PHASE_SUCCESS,
            ToolStatus::Error => PHASE_ERROR,
        },
        _ => PHASE_COMPLETE,
    }
}

/// Structured content plus metadata, never default-rendered lines.
#[allow(dead_code)]
pub(crate) fn project(message: &DisplayMessage) -> serde_json::Value {
    project_with_tool_display(message, 0, false)
}

pub(crate) fn project_with_tool_display(
    message: &DisplayMessage,
    output_limit: usize,
    output_expanded: bool,
) -> serde_json::Value {
    project_with_tool_sections(
        message,
        output_limit,
        SectionFlags {
            script: output_expanded,
            output: output_expanded,
        },
    )
}

pub(crate) fn project_with_tool_sections(
    message: &DisplayMessage,
    output_limit: usize,
    expanded: SectionFlags,
) -> serde_json::Value {
    let mut block = serde_json::Map::new();
    block.insert("kind".into(), kind(&message.role).into());
    block.insert("text".into(), message.text.clone().into());
    block.insert("phase".into(), phase(&message.role).into());
    // Descriptors only: the encoded payload must never reach a renderer.
    block.insert(
        "images".into(),
        message
            .images
            .iter()
            .map(|image| {
                serde_json::json!({
                    "media_type": image.media_type.mime(),
                    "bytes": image.data.len(),
                })
            })
            .collect::<Vec<_>>()
            .into(),
    );
    block.insert(
        "thinking_collapsed".into(),
        message.thinking_collapsed.into(),
    );
    for (key, value) in [
        ("annotation", &message.annotation),
        ("timestamp", &message.timestamp),
        ("turn_usage", &message.turn_usage),
        ("plan_path", &message.plan_path),
    ] {
        if let Some(value) = value {
            block.insert(key.into(), value.clone().into());
        }
    }
    if let DisplayRole::Tool(tool) = &message.role {
        block.insert(
            "tool".into(),
            tool_block(message, &tool.name, output_limit, expanded),
        );
    }
    serde_json::Value::Object(block)
}

/// The structured `tool` object: identity, lifecycle, and the header content
/// a renderer needs, never a pre-rendered string.
fn tool_block(
    message: &DisplayMessage,
    name: &str,
    output_limit: usize,
    expanded: SectionFlags,
) -> serde_json::Value {
    let header_text = message
        .text
        .split_once('\n')
        .map_or(message.text.as_str(), |(head, _)| head);
    let header = match &message.render_header {
        Some(snapshot) => serde_json::json!({
            "kind": "snapshot",
            "lines": snapshot.lines.iter().map(snapshot_line_json).collect::<Vec<_>>(),
        }),
        None => {
            let mut header = serde_json::Map::new();
            header.insert("kind".into(), "text".into());
            header.insert("text".into(), header_text.into());
            if let Some(annotation) = &message.annotation {
                header.insert("annotation".into(), annotation.clone().into());
            }
            serde_json::Value::Object(header)
        }
    };
    let mut tool = serde_json::Map::new();
    tool.insert("name".into(), name.into());
    tool.insert("status".into(), phase(&message.role).into());
    for (key, value) in [
        ("usage", &message.turn_usage),
        ("timestamp", &message.timestamp),
    ] {
        if let Some(value) = value {
            tool.insert(key.into(), value.clone().into());
        }
    }
    tool.insert("header".into(), header);
    tool.insert("body_authority".into(), body_authority(message).into());
    if let Some(code) = code_projection(message, output_limit, expanded) {
        tool.insert("code".into(), code);
    }
    if let Some(output) = &message.tool_output {
        if let Some(body) = tool_body_projection(output, output_limit, expanded.output) {
            tool.insert("body".into(), body);
        }
        match output.as_ref() {
            ToolOutput::Diff {
                path,
                before,
                after,
                summary,
            } => {
                tool.insert("diff".into(), diff_projection(path, before, after, summary));
            }
            ToolOutput::GrepResult { entries } => {
                tool.insert(
                    "grep".into(),
                    grep_projection(entries, output_limit, expanded.output),
                );
            }
            _ => {}
        }
    }
    serde_json::Value::Object(tool)
}

/// Who owns the tool body. A streamed plugin body outranks anything Lua can
/// compose from the projection, so Lua must keep the native marker whenever
/// the host holds one.
fn body_authority(message: &DisplayMessage) -> &'static str {
    if message.render_snapshot.is_some() {
        "snapshot"
    } else if message.live_output.is_some() {
        "live"
    } else {
        "none"
    }
}

/// The synthetic block for an instruction segment. Instructions are their own
/// transcript element with their own header and expand state, so they are not
/// a tool block and carry no tool body authority.
pub(crate) fn project_instructions(
    id: &str,
    parent_id: &str,
    blocks: &[maki_agent::InstructionBlock],
    limit: usize,
    expanded: bool,
) -> serde_json::Value {
    let effective = if expanded { usize::MAX } else { limit };
    let plan = crate::components::code_view::plan_instructions_layout(blocks, effective);
    let total = crate::components::code_view::plan_instructions_layout(blocks, usize::MAX)
        .rows
        .len();
    let (text, annotation) = instructions_header(blocks);
    let mut block = serde_json::Map::new();
    block.insert("kind".into(), "instructions".into());
    block.insert("id".into(), id.into());
    block.insert("parent_id".into(), parent_id.into());
    block.insert("phase".into(), PHASE_COMPLETE.into());
    block.insert(
        "header".into(),
        serde_json::json!({ "name": INSTRUCTIONS_TOOL, "text": text, "annotation": annotation }),
    );
    block.insert(
        "blocks".into(),
        blocks
            .iter()
            .map(|b| serde_json::json!({ "path": b.path, "content": b.content }))
            .collect::<Vec<_>>()
            .into(),
    );
    block.insert(
        "display".into(),
        serde_json::json!({
            "expanded": expanded,
            "limit": {
                "configured": limit,
                "visible_lines": plan.rows.len(),
                "total_lines": total,
            },
            "truncation": plan.truncated.then(|| serde_json::json!({ "hidden_lines": total - plan.rows.len() })),
        }),
    );
    serde_json::Value::Object(block)
}

fn code_projection(
    message: &DisplayMessage,
    limit: usize,
    expanded: SectionFlags,
) -> Option<serde_json::Value> {
    let input = message.tool_input.as_deref().map(|input| match input {
        maki_agent::ToolInput::Code { language, code }
        | maki_agent::ToolInput::Script { language, code } => {
            let total_lines = logical_line_count(code.trim_end_matches('\n'));
            serde_json::json!({
                "language": language,
                "code": code,
                "display": body_display(
                    total_lines,
                    visible_code_lines(total_lines, limit, expanded.script),
                    limit,
                    expanded.script,
                ),
            })
        }
    });
    let output = message
        .tool_output
        .as_deref()
        .and_then(|output| match output {
            ToolOutput::ReadCode {
                path,
                start_line,
                lines,
                total_lines,
                instructions,
            } => Some(serde_json::json!({
                "kind": "read_code",
                "path": path,
                "start_line": start_line,
                "lines": lines,
                "total_lines": total_lines,
                "instructions": instructions,
                "display": body_display(
                    lines.len(),
                    visible_code_lines(lines.len(), limit, expanded.output),
                    limit,
                    expanded.output,
                ),
            })),
            _ => None,
        });
    if input.is_none() && output.is_none() {
        return None;
    }
    // A static read is the only output this slice covers; anything else
    // (writes, diffs) keeps the native body until its own migration.
    if message.tool_output.is_some() && output.is_none() {
        return None;
    }
    Some(serde_json::json!({ "input": input, "output": output }))
}

fn visible_code_lines(total_lines: usize, limit: usize, expanded: bool) -> usize {
    let visible_lines = if expanded || limit == 0 {
        total_lines
    } else {
        total_lines.min(limit)
    };
    if should_truncate(total_lines.saturating_sub(visible_lines)) {
        visible_lines
    } else {
        total_lines
    }
}

/// Who composes a tool or instruction block. Lua owns the states it can compose
/// exactly; the host keeps the states whose interaction metadata it must supply,
/// such as a clickable truncation window or a streamed body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyOwner {
    Lua,
    Host,
}

/// The single authority for who owns a tool block's lines.
pub(crate) fn tool_body_owner(
    message: &DisplayMessage,
    limit: usize,
    expanded: SectionFlags,
) -> BodyOwner {
    let DisplayRole::Tool(tool) = &message.role else {
        return BodyOwner::Lua;
    };
    if tool.status == ToolStatus::InProgress || body_authority(message) != "none" {
        return BodyOwner::Host;
    }
    let Some(output) = message.tool_output.as_deref() else {
        return BodyOwner::Host;
    };
    match output {
        ToolOutput::Diff { .. } => BodyOwner::Lua,
        ToolOutput::GrepResult { entries } => {
            if entries.is_empty() {
                return BodyOwner::Host;
            }
            let effective = if expanded.output { usize::MAX } else { limit };
            if crate::components::code_view::plan_grep_layout(entries, effective).truncated {
                BodyOwner::Host
            } else {
                BodyOwner::Lua
            }
        }
        ToolOutput::TodoList(items) => one_section(items.len(), limit, expanded.output),
        ToolOutput::Plain(text) | ToolOutput::Markdown(text) | ToolOutput::ReadDir(text) => {
            one_section(logical_line_count(&text.text), limit, expanded.output)
        }
        ToolOutput::Batch { text } => one_section(logical_line_count(text), limit, expanded.output),
        ToolOutput::ReadCode { lines, .. } => {
            let output = visible_code_lines(lines.len(), limit, expanded.output) == lines.len();
            match message.tool_input.as_deref() {
                Some(maki_agent::ToolInput::Code { code, .. })
                | Some(maki_agent::ToolInput::Script { code, .. }) => {
                    let total = logical_line_count(code.trim_end_matches('\n'));
                    if visible_code_lines(total, limit, expanded.script) == total && output {
                        BodyOwner::Lua
                    } else {
                        BodyOwner::Host
                    }
                }
                None => BodyOwner::Host,
            }
        }
        // Images carry no body lines, and legacy inline instruction bodies have
        // no bridge: both stay on the host path permanently.
        _ => BodyOwner::Host,
    }
}

/// One body section fits when the budget hides nothing.
fn one_section(total: usize, limit: usize, expanded: bool) -> BodyOwner {
    if visible_code_lines(total, limit, expanded) == total {
        BodyOwner::Lua
    } else {
        BodyOwner::Host
    }
}

/// Who composes an instruction segment's lines. The host keeps every collapsed
/// or truncated state, because the expansion click lives in its metadata.
pub(crate) fn instructions_body_owner(
    blocks: &[maki_agent::InstructionBlock],
    expanded: bool,
) -> BodyOwner {
    if !expanded || blocks.is_empty() {
        return BodyOwner::Host;
    }
    let limit = crate::components::code_view::instruction_limit(true);
    if crate::components::code_view::plan_instructions_layout(blocks, limit).truncated {
        BodyOwner::Host
    } else {
        BodyOwner::Lua
    }
}

fn tool_body_projection(
    output: &ToolOutput,
    limit: usize,
    expanded: bool,
) -> Option<serde_json::Value> {
    let (kind, text, legacy) = match output {
        ToolOutput::Plain(text) => ("plain", text.text.as_str(), false),
        ToolOutput::Markdown(text) => ("markdown", text.text.as_str(), false),
        ToolOutput::ReadDir(text) => ("read_dir", text.text.as_str(), false),
        ToolOutput::Batch { text } => ("batch", text.as_str(), true),
        ToolOutput::TodoList(items) => {
            let total_lines = items.len();
            let visible_lines = if expanded || limit == 0 {
                total_lines
            } else {
                total_lines.min(limit)
            };
            return Some(serde_json::json!({
                "kind": "todo_list",
                "items": items.iter().map(todo_item_projection).collect::<Vec<_>>(),
                "empty": items.is_empty().then(|| serde_json::json!({
                    "label": "No todos.",
                    "group": "todo.empty",
                })),
                "display": body_display(total_lines, visible_lines, limit, expanded),
            }));
        }
        _ => return None,
    };
    let total_lines = logical_line_count(text);
    let visible_lines = if expanded || limit == 0 {
        total_lines
    } else {
        total_lines.min(limit)
    };
    Some(serde_json::json!({
        "kind": kind,
        "text": text,
        "legacy": legacy.then_some(true),
        "display": body_display(total_lines, visible_lines, limit, expanded),
    }))
}

fn todo_item_projection(item: &maki_agent::types::TodoItem) -> serde_json::Value {
    serde_json::json!({
        "content": item.content,
        "status": item.status,
        "priority": item.priority,
        "marker": item.status,
    })
}

fn logical_line_count(text: &str) -> usize {
    if !text.is_empty() {
        text.lines().count()
    } else {
        0
    }
}

fn body_display(
    total_lines: usize,
    visible_lines: usize,
    configured: usize,
    expanded: bool,
) -> serde_json::Value {
    let hidden = total_lines.saturating_sub(visible_lines);
    let truncation = (hidden > 0).then(|| {
        serde_json::json!({
            "head_hidden": 0,
            "tail_hidden": hidden,
            "visible_start": if visible_lines == 0 { 0 } else { 1 },
            "visible_end": visible_lines,
        })
    });
    serde_json::json!({
        "expanded": expanded,
        "limit": {
            "configured": configured,
            "visible_lines": visible_lines,
            "total_lines": total_lines,
        },
        "truncation": truncation,
    })
}

fn diff_projection(path: &str, before: &str, after: &str, summary: &str) -> serde_json::Value {
    let hunks = maki_agent::diff::compute_hunks(before, after)
        .into_iter()
        .map(|hunk| {
            let has_old = hunk
                .lines
                .iter()
                .any(|line| !matches!(line, DiffLine::Added(_)));
            let has_new = hunk
                .lines
                .iter()
                .any(|line| !matches!(line, DiffLine::Removed(_)));
            let old_start = if has_old { hunk.before_start } else { 0 };
            let new_start = if has_new { hunk.after_start } else { 0 };
            let mut old_line = old_start;
            let mut new_line = new_start;
            let last_line = hunk.lines.len().saturating_sub(1);
            let old_count = hunk
                .lines
                .iter()
                .filter(|line| !matches!(line, DiffLine::Added(_)))
                .count();
            let new_count = hunk
                .lines
                .iter()
                .filter(|line| !matches!(line, DiffLine::Removed(_)))
                .count();
            let lines = hunk
                .lines
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let no_newline_at_eof = index == last_line
                        && ((has_old && !before.ends_with('\n'))
                            || (has_new && !after.ends_with('\n')));
                    match line {
                        DiffLine::Unchanged(text) => {
                            let line = serde_json::json!({
                                "kind": "context",
                                "text": text,
                                "old_line": old_line,
                                "new_line": new_line,
                                "no_newline_at_eof": no_newline_at_eof,
                            });
                            old_line += 1;
                            new_line += 1;
                            line
                        }
                        DiffLine::Removed(spans) => {
                            let line = serde_json::json!({
                                "kind": "remove",
                                "text": spans.iter().map(|span| span.text.as_str()).collect::<String>(),
                                "old_line": old_line,
                                "emphasis": emphasis_ranges(&spans),
                                "no_newline_at_eof": no_newline_at_eof,
                            });
                            old_line += 1;
                            line
                        }
                        DiffLine::Added(spans) => {
                            let line = serde_json::json!({
                                "kind": "add",
                                "text": spans.iter().map(|span| span.text.as_str()).collect::<String>(),
                                "new_line": new_line,
                                "emphasis": emphasis_ranges(&spans),
                                "no_newline_at_eof": no_newline_at_eof,
                            });
                            new_line += 1;
                            line
                        }
                    }
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "old_start": old_start,
                "old_count": old_count,
                "new_start": new_start,
                "new_count": new_count,
                "lines": lines,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "path": path,
        "summary": summary,
        "mode": if before.is_empty() { "new" } else if after.is_empty() { "delete" } else { "edit" },
        "hunks": hunks,
        "sources": { "before": before, "after": after },
    })
}
fn emphasis_ranges(spans: &[maki_agent::diff::DiffSpan]) -> Vec<serde_json::Value> {
    let mut offset = 0;
    spans
        .iter()
        .filter_map(|span| {
            let start_byte = offset;
            offset += span.text.len();
            span.emphasized.then(|| {
                serde_json::json!({
                    "start_byte": start_byte,
                    "end_byte": offset,
                    "kind": "changed",
                })
            })
        })
        .collect()
}

fn valid_match_ranges(line: &maki_agent::GrepLine) -> Option<&[std::ops::Range<usize>]> {
    let valid = line.match_ranges.iter().all(|range| {
        range.start <= range.end
            && range.end <= line.text.len()
            && line.text.is_char_boundary(range.start)
            && line.text.is_char_boundary(range.end)
    });
    valid.then_some(&line.match_ranges)
}

fn grep_projection(
    entries: &[maki_agent::GrepFileEntry],
    limit: usize,
    expanded: bool,
) -> serde_json::Value {
    let effective = if expanded { usize::MAX } else { limit };
    let plan = crate::components::code_view::plan_grep_layout(entries, effective);
    let truncation = plan.truncated.then(|| {
        serde_json::json!({
            "hidden_matches": plan.hidden_matches,
        })
    });
    let empty = entries.is_empty().then(|| {
        serde_json::json!({
            "label": maki_agent::NO_FILES_FOUND,
            "group": "grep.empty",
        })
    });
    serde_json::json!({
        "entries": entries.iter().map(|entry| serde_json::json!({
            "path": entry.path,
            "display": entry.path,
            "groups": entry.groups.iter().map(|group| serde_json::json!({
                "lines": group.lines.iter().map(|line| serde_json::json!({
                    "line": line.line_nr,
                    "text": line.text,
                    "is_match": line.is_match,
                    "ranges": valid_match_ranges(line).map(|ranges| ranges.iter().map(|range| serde_json::json!({
                        "start_byte": range.start,
                        "end_byte": range.end,
                    })).collect::<Vec<_>>()).unwrap_or_default(),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "display": {
            "expanded": expanded,
            "limit": {
                "configured": limit,
                "visible_lines": plan.rows.len(),
                "total_lines": plan.total_rows,
            },
            "truncation": truncation,
        },
        "empty": empty,
    })
}

/// A snapshot line in the same `{text, style?}` span shape the Lua span
/// grammar takes. Legacy dynamic-spinner styles become the semantic spinner
/// form here, so Lua never learns the internal string convention.
fn snapshot_line_json(line: &SnapshotLine) -> serde_json::Value {
    line.spans
        .iter()
        .map(snapshot_span_json)
        .collect::<Vec<_>>()
        .into()
}

fn snapshot_span_json(span: &SnapshotSpan) -> serde_json::Value {
    let text = serde_json::Value::String(span.text.clone());
    match &span.style {
        SpanStyle::Named(name) => match spinner_token(name) {
            Some(token) => serde_json::json!([text, { "spinner": token }]),
            None => serde_json::json!([text, name]),
        },
        SpanStyle::Default => serde_json::json!([text]),
        SpanStyle::Inline(inline) => serde_json::json!([text, inline_json(inline)]),
    }
}

fn inline_json(inline: &InlineStyle) -> serde_json::Value {
    let mut style = serde_json::Map::new();
    for (key, color) in [
        ("fg", inline.fg),
        ("bg", inline.bg),
        ("underline_color", inline.underline_color),
    ] {
        if let Some(color) = color {
            style.insert(key.into(), span_color_json(color));
        }
    }
    for (key, on) in [
        ("bold", inline.bold),
        ("italic", inline.italic),
        ("underline", inline.underline),
        ("dim", inline.dim),
        ("strikethrough", inline.strikethrough),
        ("reversed", inline.reversed),
        ("hidden", inline.hidden),
        ("slow_blink", inline.slow_blink),
        ("rapid_blink", inline.rapid_blink),
    ] {
        if on {
            style.insert(key.into(), true.into());
        }
    }
    serde_json::Value::Object(style)
}

/// A color in the `#rrggbb | index | default` grammar the span parser accepts.
fn span_color_json(color: SpanColor) -> serde_json::Value {
    match color {
        SpanColor::Rgb((r, g, b)) => serde_json::json!(format!("#{r:02x}{g:02x}{b:02x}")),
        SpanColor::Ansi(index) => serde_json::json!(index.to_string()),
        SpanColor::Default(_) => serde_json::json!("default"),
    }
}

/// A block render that asked for the native tool body, with its Rust-owned
/// metadata shifted to where Lua composed it. Temporary, through the tool
/// migration only.
#[allow(dead_code)]
pub(crate) struct ToolBody {
    /// Index of the body's first line in the rendered block.
    pub offset: usize,
    /// Highlight request with its range already shifted. The host owns the
    /// async highlight; Lua never sees or sets it.
    pub highlight: Option<HighlightRequest>,
    pub spinner_lines: Vec<SpinnerLine>,
    pub snapshot_base: Option<usize>,
    pub content_indent: &'static str,
    pub truncation: SectionFlags,
}

/// The lines a block render produced, plus the bodies it placed, if any, and
/// the spinner spans the renderer's own lines carry.
pub(crate) struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub raw: Vec<RawSpan>,
    pub tool: Option<ToolBody>,
    pub instructions: Option<ToolBody>,
    pub spinner_lines: Vec<SpinnerLine>,
}

/// Render objects as ratatui lines plus the raw spans they place, with no
/// native body available. See [`render_with_bodies`].
pub(crate) fn render(objects: &[RenderObject]) -> Option<Rendered> {
    render_with_bodies(objects, None, None)
}

/// [`render`] with the host's native tool body available.
pub(crate) fn render_with_tools(
    objects: &[RenderObject],
    tool_lines: Option<ToolLines>,
) -> Option<Rendered> {
    render_with_bodies(objects, tool_lines, None)
}

/// [`render`] with the host's native bodies available. At most one marker of
/// each kind is allowed, and a marker must have a body to place: anything else
/// refuses the block, so Lua can position a body but never supply its content
/// or metadata.
pub(crate) fn render_with_bodies(
    objects: &[RenderObject],
    tool_lines: Option<ToolLines>,
    instruction_lines: Option<ToolLines>,
) -> Option<Rendered> {
    let mut lines = Vec::new();
    let mut raw = Vec::new();
    let mut spinner_lines = Vec::new();
    let mut tool = tool_lines;
    let mut instructions = instruction_lines;
    let mut body = None;
    let mut instructions_body = None;
    for object in objects {
        match object {
            RenderObject::Lines {
                lines: source,
                decorations,
            } => {
                let decorated = render_lines(source, decorations)?;
                let offset = lines.len();
                for (line_idx, mut rendered) in decorated.into_iter().enumerate() {
                    for placement in &mut rendered.1 {
                        placement.line += offset + line_idx;
                    }
                    spinner_lines.extend(rendered.1);
                    lines.push(rendered.0);
                }
            }
            RenderObject::Raw { seq, width, height } => {
                raw::validate(seq).ok()?;
                let width = (*width).min(MAX_RAW_WIDTH);
                let height = (*height).min(MAX_RAW_HEIGHT);
                raw.push(RawSpan {
                    row: lines.len(),
                    seq: seq.clone(),
                    width,
                    height,
                });
                let placeholder = Line::from(Span::raw(PLACEHOLDER_CELL.repeat(width as usize)));
                lines.extend(std::iter::repeat_n(placeholder, height as usize));
            }
            RenderObject::InstructionsBody => {
                let mut tl = instructions.take()?;
                let offset = lines.len();
                lines.append(&mut tl.lines);
                instructions_body = Some(place_tool_body(tl, offset));
            }
            RenderObject::ToolBody => {
                let mut tl = tool.take()?;
                let offset = lines.len();
                lines.append(&mut tl.lines);
                body = Some(place_tool_body(tl, offset));
            }
        }
    }
    Some(Rendered {
        lines,
        raw,
        tool: body,
        instructions: instructions_body,
        spinner_lines,
    })
}

fn render_lines(
    source: &[SnapshotLine],
    decorations: &[Decoration],
) -> Option<Vec<(Line<'static>, Vec<SpinnerLine>)>> {
    let mut by_line = vec![Vec::new(); source.len()];
    for decoration in decorations {
        let line = source.get(decoration.line)?;
        let text = line
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        if decoration.bytes.end > text.len()
            || !text.is_char_boundary(decoration.bytes.start)
            || !text.is_char_boundary(decoration.bytes.end)
        {
            return None;
        }
        by_line[decoration.line].push(decoration);
    }
    source
        .iter()
        .enumerate()
        .map(|(line_idx, line)| render_line(line, &by_line[line_idx]))
        .collect()
}

fn render_line(
    source: &SnapshotLine,
    decorations: &[&Decoration],
) -> Option<(Line<'static>, Vec<SpinnerLine>)> {
    let mut spans = Vec::new();
    let mut spinners = Vec::new();
    let mut start = 0;
    for source_span in &source.spans {
        let end = start + source_span.text.len();
        if let Some(style) = spinner_style_name(&source_span.style) {
            if decorations
                .iter()
                .any(|decoration| decoration.bytes.start < end && start < decoration.bytes.end)
            {
                return None;
            }
            spinners.push(SpinnerLine {
                line: 0,
                span: spans.len(),
                style: style.map(Arc::from),
            });
            spans.push(spinner_span(style));
            start = end;
            continue;
        }
        if start == end {
            spans.push(Span::styled(
                String::new(),
                resolve_span_style(&source_span.style),
            ));
            continue;
        }
        let mut boundaries = vec![start, end];
        for decoration in decorations {
            if decoration.bytes.start > start && decoration.bytes.start < end {
                boundaries.push(decoration.bytes.start);
            }
            if decoration.bytes.end > start && decoration.bytes.end < end {
                boundaries.push(decoration.bytes.end);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        for chunk in boundaries.windows(2) {
            let range = chunk[0]..chunk[1];
            let mut style = resolve_span_style(&source_span.style);
            for decoration in decorations {
                if decoration.bytes.start <= range.start && range.end <= decoration.bytes.end {
                    style = style.patch(resolve_span_style(&decoration.style));
                }
            }
            let local = range.start - start..range.end - start;
            spans.push(Span::styled(source_span.text[local].to_owned(), style));
        }
        start = end;
    }
    Some((Line::from(spans), spinners))
}

fn place_tool_body(mut tl: ToolLines, offset: usize) -> ToolBody {
    let mut highlight = tl.highlight.take();
    if let Some(request) = &mut highlight {
        request.range = (request.range.0 + offset, request.range.1 + offset);
    }
    let spinner_lines = tl
        .spinner_lines
        .iter()
        .map(|placement| SpinnerLine {
            line: placement.line + offset,
            ..placement.clone()
        })
        .collect();
    ToolBody {
        offset,
        highlight,
        spinner_lines,
        snapshot_base: tl.snapshot_base.map(|base| base + offset),
        content_indent: tl.content_indent,
        truncation: tl.truncation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::code_view::RenderLimits;
    use crate::{
        components::{IMAGE_PLACEHOLDER, ToolRole, ToolStatus},
        theme,
    };
    use maki_agent::{BufferSnapshot, SnapshotLine};
    use maki_providers::{ImageMediaType, ImageSource};
    use std::sync::Arc;
    use test_case::test_case;

    const PNG_PAYLOAD: &str = "base64-payload-that-must-not-leak";

    #[test]
    fn project_carries_kind_text_and_present_metadata() {
        let mut message = DisplayMessage::new(DisplayRole::User, "hi".to_owned());
        message.timestamp = Some("12:00".to_owned());
        let block = project(&message);
        assert_eq!(block["kind"], "user");
        assert_eq!(block["text"], "hi");
        assert_eq!(block["phase"], PHASE_COMPLETE);
        assert_eq!(block["timestamp"], "12:00");
        assert_eq!(block["thinking_collapsed"], false);
        assert_eq!(block["images"], serde_json::json!([]));
        assert!(block.get("annotation").is_none());
    }

    #[test]
    fn project_lists_image_descriptors_without_the_payload() {
        let mut message = DisplayMessage::new(DisplayRole::User, IMAGE_PLACEHOLDER.to_owned());
        message.images = vec![ImageSource::new(
            ImageMediaType::Png,
            Arc::from(PNG_PAYLOAD),
        )];
        let block = project(&message);
        let images = block["images"].as_array().expect("images array");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["media_type"], ImageMediaType::Png.mime());
        assert_eq!(images[0]["bytes"], PNG_PAYLOAD.len());
        assert!(!block.to_string().contains(PNG_PAYLOAD));
    }

    #[test]
    fn body_authority_names_the_host_body_owner() {
        let mut message = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: "t".to_owned(),
                status: ToolStatus::Success,
                name: Arc::from("bash"),
            })),
            "bash> cmd".to_owned(),
        );
        let authority = |message: &DisplayMessage| {
            project_with_tool_display(message, 0, false)["tool"]["body_authority"].clone()
        };
        assert_eq!(authority(&message), "none");
        message.live_output = Some("streaming".to_owned());
        assert_eq!(authority(&message), "live");
        message.render_snapshot = Some(BufferSnapshot::plain_text("streamed".into()));
        assert_eq!(authority(&message), "snapshot");
    }

    #[test]
    fn code_projection_normalizes_input_and_tracks_sections() {
        let mut message = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: "t".to_owned(),
                status: ToolStatus::Success,
                name: Arc::from("read"),
            })),
            "read> src/lib.rs".to_owned(),
        );
        message.tool_input = Some(Arc::new(maki_agent::ToolInput::Script {
            language: "rust".to_owned(),
            code: "one\ntwo\nthree\nfour\n".to_owned(),
        }));
        message.tool_output = Some(Arc::new(ToolOutput::ReadCode {
            path: "src/lib.rs".to_owned(),
            start_line: 10,
            lines: vec!["four".to_owned(), "five".to_owned(), "six".to_owned()],
            total_lines: 20,
            instructions: None,
        }));
        let block = project_with_tool_sections(
            &message,
            2,
            SectionFlags {
                script: false,
                output: true,
            },
        );
        let code = &block["tool"]["code"];
        assert_eq!(code["input"]["language"], "rust");
        assert_eq!(code["input"]["code"], "one\ntwo\nthree\nfour\n");
        assert_eq!(code["input"]["display"]["truncation"]["tail_hidden"], 2);
        assert_eq!(code["output"]["kind"], "read_code");
        assert_eq!(code["output"]["path"], "src/lib.rs");
        assert_eq!(code["output"]["start_line"], 10);
        assert_eq!(
            code["output"]["display"]["truncation"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn code_projection_omits_absent_output_section() {
        let mut message = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: "t".to_owned(),
                status: ToolStatus::Success,
                name: Arc::from("bash"),
            })),
            "bash> echo".to_owned(),
        );
        message.tool_input = Some(Arc::new(maki_agent::ToolInput::Code {
            language: "bash".to_owned(),
            code: "echo ok".to_owned(),
        }));
        let block = project_with_tool_sections(&message, 20, SectionFlags::default());
        let code = &block["tool"]["code"];
        assert!(code["input"].is_object());
        assert!(code["output"].is_null());
    }

    #[test]
    fn todo_body_projection_uses_semantic_tokens() {
        let output = ToolOutput::TodoList(vec![maki_agent::types::TodoItem {
            content: "write tests".to_owned(),
            status: maki_agent::types::TodoStatus::Completed,
            priority: maki_agent::types::TodoPriority::High,
        }]);
        let body = tool_body_projection(&output, 0, true).expect("body");
        assert_eq!(body["items"][0]["status"], "completed");
        assert_eq!(body["items"][0]["marker"], "completed");
        assert_eq!(body["items"][0]["priority"], "high");
        assert!(body["items"][0].get("glyph").is_none());
    }

    #[test]
    fn todo_body_projection_has_semantic_empty_state() {
        let body = tool_body_projection(&ToolOutput::TodoList(Vec::new()), 0, true).expect("body");
        assert_eq!(body["empty"]["label"], "No todos.");
        assert_eq!(body["empty"]["group"], "todo.empty");
    }

    #[test_case(ToolOutput::Plain("one\ntwo\nthree".into()), "plain" ; "plain")]
    #[test_case(ToolOutput::Markdown("one\ntwo\nthree".into()), "markdown" ; "markdown")]
    fn text_body_projection_keeps_source_and_display_state(output: ToolOutput, kind: &str) {
        let text = output.as_text();
        let mut message = DisplayMessage::new(
            DisplayRole::Tool(Box::new(ToolRole {
                id: "t".to_owned(),
                status: ToolStatus::Success,
                name: Arc::from("bash"),
            })),
            "bash> cmd".to_owned(),
        );
        message.tool_output = Some(Arc::new(output));
        let block = project_with_tool_display(&message, 2, false);
        let body = &block["tool"]["body"];
        assert_eq!(body["kind"], kind);
        assert_eq!(body["text"], text);
        assert_eq!(body["display"]["limit"]["configured"], 2);
        assert_eq!(body["display"]["limit"]["visible_lines"], 2);
        assert_eq!(body["display"]["truncation"]["tail_hidden"], 1);
        let expanded = project_with_tool_display(&message, 2, true);
        assert!(expanded["tool"]["body"]["display"]["truncation"].is_null());
    }

    #[test]
    fn diff_projection_uses_unified_line_conventions() {
        let projection = diff_projection("src/lib.rs", "old\n", "new\n", "changed");
        assert_eq!(projection["path"], "src/lib.rs");
        let hunk = &projection["hunks"][0];
        assert_eq!(projection["mode"], "edit");
        assert_eq!(hunk["old_start"], 1);
        assert_eq!(hunk["old_count"], 1);
        assert_eq!(hunk["new_start"], 1);
        assert_eq!(hunk["new_count"], 1);
        assert_eq!(hunk["lines"][0]["kind"], "remove");
        assert_eq!(hunk["lines"][0]["old_line"], 1);
        assert!(hunk["lines"][0].get("new_line").is_none());
        assert_eq!(hunk["lines"][1]["kind"], "add");
        assert_eq!(hunk["lines"][1]["new_line"], 1);
        assert!(hunk["lines"][1].get("old_line").is_none());
    }

    /// The diff bridge highlights with one parser per side, so a line's colours
    /// depend on every line above it. The projection has to carry both full
    /// sources, or a hunk that starts mid-file cannot be coloured like the
    /// transcript.
    #[test]
    fn diff_projection_carries_the_sources_the_highlighter_walks() {
        let before = "fn a() {\n    let x = 1;\n}\n\nfn b() {}\n";
        let after = "fn a() {\n    let x = 2;\n}\n\nfn b() {}\n";
        let projection = diff_projection("src/lib.rs", before, after, "changed");

        assert_eq!(projection["sources"]["before"], before);
        assert_eq!(projection["sources"]["after"], after);
        assert!(projection.get("display").is_none());
    }

    /// Grep truncation is real budget accounting, so the projection reads the
    /// same plan the renderer paints.
    #[test]
    fn grep_projection_reports_the_native_budget() {
        let entries = vec![maki_agent::GrepFileEntry {
            path: "src/lib.rs".to_owned(),
            groups: vec![maki_agent::GrepMatchGroup {
                lines: (1..=4)
                    .map(|line_nr| maki_agent::GrepLine {
                        line_nr,
                        text: format!("line {line_nr}"),
                        is_match: true,
                        match_ranges: Vec::new(),
                    })
                    .collect(),
            }],
        }];
        let expanded = grep_projection(&entries, 2, true);
        assert_eq!(expanded["display"]["limit"]["visible_lines"], 4);
        assert_eq!(expanded["display"]["limit"]["total_lines"], 4);
        assert!(expanded["display"]["truncation"].is_null());

        let collapsed = grep_projection(&entries, 2, false);
        assert_eq!(collapsed["display"]["limit"]["configured"], 2);
        assert_eq!(collapsed["display"]["limit"]["visible_lines"], 2);
        assert_eq!(collapsed["display"]["limit"]["total_lines"], 4);
        assert_eq!(collapsed["display"]["truncation"]["hidden_matches"], 2);
    }

    #[test]
    fn grep_projection_has_a_semantic_empty_state() {
        let projection = grep_projection(&[], usize::MAX, true);
        assert_eq!(projection["empty"]["label"], maki_agent::NO_FILES_FOUND);
        assert_eq!(projection["empty"]["group"], "grep.empty");
        assert!(projection["display"]["truncation"].is_null());
    }

    #[test]
    fn grep_projection_preserves_source_match_ranges() {
        let projection = grep_projection(
            &[maki_agent::GrepFileEntry {
                path: "src/lib.rs".to_owned(),
                groups: vec![maki_agent::GrepMatchGroup {
                    lines: vec![maki_agent::GrepLine {
                        line_nr: 2,
                        text: "héllo".to_owned(),
                        is_match: true,
                        match_ranges: std::iter::once(1..3).collect(),
                    }],
                }],
            }],
            usize::MAX,
            true,
        );
        let line = &projection["entries"][0]["groups"][0]["lines"][0];
        assert_eq!(line["line"], 2);
        assert_eq!(line["text"], "héllo");
        assert_eq!(
            line["ranges"],
            serde_json::json!([{"start_byte": 1, "end_byte": 3}])
        );
    }

    #[test]
    fn diff_projection_marks_empty_sides_and_missing_newline() {
        let new_file = diff_projection("new", "", "é", "created");
        let hunk = &new_file["hunks"][0];
        assert_eq!(new_file["mode"], "new");
        assert_eq!(hunk["old_start"], 0);
        assert_eq!(hunk["old_count"], 0);
        assert_eq!(hunk["new_start"], 1);
        assert_eq!(hunk["new_count"], 1);
        assert!(
            hunk["lines"][0]["no_newline_at_eof"]
                .as_bool()
                .expect("no newline flag")
        );
    }

    #[test]
    fn grep_projection_drops_invalid_match_ranges() {
        let line = maki_agent::GrepLine {
            line_nr: 1,
            text: "é".to_owned(),
            is_match: true,
            match_ranges: std::iter::once(1..2).collect(),
        };
        assert!(valid_match_ranges(&line).is_none());
        let projection = grep_projection(
            &[maki_agent::GrepFileEntry {
                path: "a".to_owned(),
                groups: vec![maki_agent::GrepMatchGroup { lines: vec![line] }],
            }],
            usize::MAX,
            true,
        );
        assert_eq!(
            projection["entries"][0]["groups"][0]["lines"][0]["ranges"],
            serde_json::json!([])
        );
    }

    #[test_case(ToolStatus::InProgress => PHASE_IN_PROGRESS ; "in_progress")]
    #[test_case(ToolStatus::Success => PHASE_SUCCESS ; "success")]
    #[test_case(ToolStatus::Error => PHASE_ERROR ; "error")]
    fn project_reports_tool_status_as_phase(status: ToolStatus) -> String {
        let role = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".to_owned(),
            status,
            name: Arc::from("bash"),
        }));
        let block = project(&DisplayMessage::new(role, String::new()));
        block["phase"].as_str().expect("phase string").to_owned()
    }

    #[test]
    fn renderable_includes_thinking_and_tools() {
        assert!(renderable(&DisplayMessage::new(
            DisplayRole::Thinking,
            String::new()
        )));
        let tool = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".to_owned(),
            status: ToolStatus::Success,
            name: Arc::from("bash"),
        }));
        assert!(renderable(&DisplayMessage::new(tool, String::new())));
    }

    #[test]
    fn image_bearing_user_block_is_renderable() {
        let mut message = DisplayMessage::new(DisplayRole::User, IMAGE_PLACEHOLDER.to_owned());
        message.images = vec![ImageSource::new(
            ImageMediaType::Png,
            Arc::from(PNG_PAYLOAD),
        )];
        assert!(renderable(&message));
        let block = project(&message);
        assert_eq!(block["images"].as_array().expect("images array").len(), 1);
        assert!(!block.to_string().contains(PNG_PAYLOAD));
    }

    const SGR_RED: &str = "\u{1b}[31m";
    const RAW_WIDTH: u16 = 3;
    const RAW_HEIGHT: u16 = 2;

    #[test]
    fn render_passes_lines_through_without_raw() {
        let objects = vec![RenderObject::Lines {
            lines: vec![SnapshotLine::plain("a".to_owned())],
            decorations: Vec::new(),
        }];
        let rendered = render(&objects).unwrap();
        let (lines, raw) = (rendered.lines, rendered.raw);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "a");
        assert!(raw.is_empty());
    }

    #[test]
    fn render_applies_overlapping_decorations_in_order() {
        let objects = vec![RenderObject::Lines {
            lines: vec![SnapshotLine::plain("a界b".to_owned())],
            decorations: vec![
                Decoration {
                    line: 0,
                    bytes: 1..4,
                    group: "tool".to_owned(),
                    style: SpanStyle::Named("tool".to_owned()),
                },
                Decoration {
                    line: 0,
                    bytes: 1..5,
                    group: "error".to_owned(),
                    style: SpanStyle::Named("error".to_owned()),
                },
            ],
        }];
        let rendered = render(&objects).expect("rendered");
        assert_eq!(
            rendered.lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["a", "界", "b"]
        );
        assert_eq!(
            rendered.lines[0].spans[1].style,
            theme::style_by_name("tool").patch(theme::style_by_name("error"))
        );
        assert_eq!(
            rendered.lines[0].spans[2].style,
            theme::style_by_name("tool").patch(theme::style_by_name("error"))
        );
    }

    #[test]
    fn utf8_wide_decoration_preserves_the_styled_substring_when_wrapped() {
        let objects = vec![RenderObject::Lines {
            lines: vec![SnapshotLine::plain("a界b".to_owned())],
            decorations: vec![Decoration {
                line: 0,
                bytes: 1..4,
                group: "tool".to_owned(),
                style: SpanStyle::Named("tool".to_owned()),
            }],
        }];
        let rendered = render(&objects).expect("rendered");
        let mut segment = super::super::segment::Segment::with_lines(rendered.lines, None);
        segment.set_lua_render(Some(segment.lines().to_vec()), Vec::new(), Vec::new());
        let mut rows = segment.rows_from(0, 2);
        let (lines, _) = rows.next_chunk(1).expect("wrapped line");
        assert_eq!(lines[0].spans[1].content.as_ref(), "界");
        assert_eq!(lines[0].spans[1].style, theme::style_by_name("tool"));
    }

    #[test]
    fn render_refuses_invalid_decoration_boundaries() {
        let objects = vec![RenderObject::Lines {
            lines: vec![SnapshotLine::plain("界".to_owned())],
            decorations: vec![Decoration {
                line: 0,
                bytes: 1..3,
                group: "tool".to_owned(),
                style: SpanStyle::Named("tool".to_owned()),
            }],
        }];
        assert!(render(&objects).is_none());
    }

    #[test]
    fn render_places_raw_after_the_lines_it_follows() {
        let objects = vec![
            RenderObject::Lines {
                lines: vec![SnapshotLine::plain("a".to_owned())],
                decorations: Vec::new(),
            },
            RenderObject::Raw {
                seq: SGR_RED.to_owned(),
                width: RAW_WIDTH,
                height: RAW_HEIGHT,
            },
        ];
        let rendered = render(&objects).unwrap();
        let (lines, raw) = (rendered.lines, rendered.raw);
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].row, 1);
        assert_eq!(raw[0].seq, SGR_RED);
        assert_eq!(raw[0].width, RAW_WIDTH);
        assert_eq!(raw[0].height, RAW_HEIGHT);
        assert_eq!(lines.len(), 1 + usize::from(RAW_HEIGHT));
        let placeholder = PLACEHOLDER_CELL.repeat(RAW_WIDTH as usize);
        for line in &lines[1..] {
            assert_eq!(line.spans[0].content.as_ref(), placeholder.as_str());
        }
    }

    #[test]
    fn render_clamps_raw_dimensions() {
        let objects = vec![RenderObject::Raw {
            seq: SGR_RED.to_owned(),
            width: u16::MAX,
            height: u16::MAX,
        }];
        let rendered = render(&objects).unwrap();
        let (lines, raw) = (rendered.lines, rendered.raw);
        assert_eq!(raw[0].width, MAX_RAW_WIDTH);
        assert_eq!(raw[0].height, MAX_RAW_HEIGHT);
        assert_eq!(lines.len(), usize::from(MAX_RAW_HEIGHT));
    }

    #[test]
    fn render_refuses_an_invalid_raw_sequence() {
        let objects = vec![RenderObject::Raw {
            seq: "\u{1b}]0;title\u{7}".to_owned(),
            width: 1,
            height: 1,
        }];
        assert!(render(&objects).is_none());
    }

    fn tool_lines() -> ToolLines {
        ToolLines {
            lines: vec![
                Line::raw("bash> ls"),
                Line::raw("  a.txt"),
                Line::raw("  b.txt"),
            ],
            highlight: Some(HighlightRequest {
                range: (1, 3),
                input: None,
                output: None,
                limits: RenderLimits {
                    script: 1,
                    output: 2,
                },
            }),
            spinner_lines: vec![SpinnerLine::new(1, 0)],
            snapshot_base: Some(1),
            content_indent: "  ",
            truncation: SectionFlags {
                script: false,
                output: true,
            },
        }
    }

    #[test]
    fn tool_body_alone_keeps_the_native_lines_and_metadata() {
        let objects = vec![RenderObject::ToolBody];
        let rendered = render_with_tools(&objects, Some(tool_lines())).expect("body placed");
        assert_eq!(
            rendered.lines,
            vec![
                Line::raw("bash> ls"),
                Line::raw("  a.txt"),
                Line::raw("  b.txt")
            ]
        );
        let body = rendered.tool.expect("tool body");
        assert_eq!(body.offset, 0);
        assert_eq!(body.highlight.expect("highlight").range, (1, 3));
        assert_eq!(body.spinner_lines, vec![SpinnerLine::new(1, 0)]);
        assert_eq!(body.snapshot_base, Some(1));
        assert!(body.truncation.output);
    }

    #[test]
    fn tool_body_metadata_is_shifted_by_the_lines_lua_puts_above_it() {
        let objects = vec![
            RenderObject::Lines {
                lines: vec![SnapshotLine::plain("above".to_owned())],
                decorations: Vec::new(),
            },
            RenderObject::ToolBody,
            RenderObject::Lines {
                lines: vec![SnapshotLine::plain("below".to_owned())],
                decorations: Vec::new(),
            },
        ];
        let rendered = render_with_tools(&objects, Some(tool_lines())).expect("body placed");
        assert_eq!(rendered.lines.len(), 5);
        let body = rendered.tool.expect("tool body");
        assert_eq!(body.offset, 1);
        assert_eq!(body.highlight.expect("highlight").range, (2, 4));
        assert_eq!(body.spinner_lines, vec![SpinnerLine::new(2, 0)]);
        assert_eq!(body.snapshot_base, Some(2));
    }

    #[test]
    fn a_second_tool_body_refuses_the_block() {
        let objects = vec![RenderObject::ToolBody, RenderObject::ToolBody];
        assert!(render_with_tools(&objects, Some(tool_lines())).is_none());
    }

    #[test]
    fn a_tool_body_without_a_native_body_refuses_the_block() {
        assert!(render(&[RenderObject::ToolBody]).is_none());
    }
}
