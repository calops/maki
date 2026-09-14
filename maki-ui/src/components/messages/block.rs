//! Projection of transcript messages into the structured blocks the Lua
//! renderer receives, and the conversion of its render objects back into
//! ratatui lines.

use ratatui::text::{Line, Span};

use maki_lua::RenderObject;

use super::raw;
use crate::components::{DisplayMessage, DisplayRole, lua_float::snapshot_to_line};

/// Bounds a raw object's rectangle so a plugin cannot ask for an unbounded
/// run of placeholder cells.
const MAX_RAW_WIDTH: u16 = 256;
const MAX_RAW_HEIGHT: u16 = 128;
/// Painted where a raw sequence lands. The final writer overwrites it with
/// the sequence once the frame is drawn, so it must be a plain cell.
const PLACEHOLDER_CELL: &str = " ";

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

/// Blocks the Lua renderer may own. Tool and thinking blocks keep their
/// Rust rendering for now: clicks, collapsing and images hang off them.
pub(crate) fn renderable(message: &DisplayMessage) -> bool {
    matches!(
        message.role,
        DisplayRole::User | DisplayRole::Assistant | DisplayRole::Error | DisplayRole::Done
    ) && message.images.is_empty()
}

/// Structured content plus metadata, never default-rendered lines.
pub(crate) fn project(message: &DisplayMessage) -> serde_json::Value {
    let mut block = serde_json::Map::new();
    block.insert("kind".into(), kind(&message.role).into());
    block.insert("text".into(), message.text.clone().into());
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
    serde_json::Value::Object(block)
}

/// Render objects as ratatui lines plus the raw spans they place. Lines
/// append their spans; a raw object appends `height` placeholder rows and
/// records where its sequence goes. A raw sequence that fails validation
/// refuses the whole block (`None`), so the caller keeps the Rust rendering
/// and an unsafe sequence can never reach the terminal.
pub(crate) fn render(objects: &[RenderObject]) -> Option<(Vec<Line<'static>>, Vec<RawSpan>)> {
    let mut lines = Vec::new();
    let mut raw = Vec::new();
    for object in objects {
        match object {
            RenderObject::Lines(source) => {
                lines.extend(source.iter().map(snapshot_to_line));
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
        }
    }
    Some((lines, raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::SnapshotLine;
    use maki_providers::{ImageMediaType, ImageSource};
    use std::sync::Arc;

    #[test]
    fn project_carries_kind_text_and_present_metadata() {
        let mut message = DisplayMessage::new(DisplayRole::User, "hi".to_owned());
        message.timestamp = Some("12:00".to_owned());
        let block = project(&message);
        assert_eq!(block["kind"], "user");
        assert_eq!(block["text"], "hi");
        assert_eq!(block["timestamp"], "12:00");
        assert!(block.get("annotation").is_none());
    }

    #[test]
    fn renderable_excludes_tools_thinking_and_images() {
        assert!(renderable(&DisplayMessage::new(
            DisplayRole::Assistant,
            String::new()
        )));
        assert!(!renderable(&DisplayMessage::new(
            DisplayRole::Thinking,
            String::new()
        )));
        let mut with_image = DisplayMessage::new(DisplayRole::User, String::new());
        with_image.images = vec![ImageSource::new(ImageMediaType::Png, Arc::from("x"))];
        assert!(!renderable(&with_image));
    }

    const SGR_RED: &str = "\u{1b}[31m";
    const RAW_WIDTH: u16 = 3;
    const RAW_HEIGHT: u16 = 2;

    #[test]
    fn render_passes_lines_through_without_raw() {
        let objects = vec![RenderObject::Lines(vec![SnapshotLine::plain(
            "a".to_owned(),
        )])];
        let (lines, raw) = render(&objects).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "a");
        assert!(raw.is_empty());
    }

    #[test]
    fn render_places_raw_after_the_lines_it_follows() {
        let objects = vec![
            RenderObject::Lines(vec![SnapshotLine::plain("a".to_owned())]),
            RenderObject::Raw {
                seq: SGR_RED.to_owned(),
                width: RAW_WIDTH,
                height: RAW_HEIGHT,
            },
        ];
        let (lines, raw) = render(&objects).unwrap();
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
        let (lines, raw) = render(&objects).unwrap();
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
}
