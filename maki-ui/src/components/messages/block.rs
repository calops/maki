//! Projection of transcript messages into the structured blocks the Lua
//! renderer receives, and the conversion of its render objects back into
//! ratatui lines.

use ratatui::text::{Line, Span};

use maki_lua::RenderObject;

use super::raw;
use crate::components::{DisplayMessage, DisplayRole, ToolStatus, lua_float::snapshot_to_line};

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

/// Blocks the Lua renderer may own. Tool blocks keep their Rust rendering for
/// now: clicks and collapsing hang off them. Thinking is static here; its
/// collapse click still runs through the native row mapping. Images do not
/// disqualify a block: `Segment::set_images` fills them in regardless of which
/// side produced the lines.
pub(crate) fn renderable(message: &DisplayMessage) -> bool {
    matches!(
        message.role,
        DisplayRole::User
            | DisplayRole::Assistant
            | DisplayRole::Thinking
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
pub(crate) fn project(message: &DisplayMessage) -> serde_json::Value {
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
    use crate::components::{IMAGE_PLACEHOLDER, ToolRole, ToolStatus};
    use maki_agent::SnapshotLine;
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
    fn renderable_includes_thinking_and_excludes_tools() {
        assert!(renderable(&DisplayMessage::new(
            DisplayRole::Thinking,
            String::new()
        )));
        let tool = DisplayRole::Tool(Box::new(ToolRole {
            id: "t1".to_owned(),
            status: ToolStatus::Success,
            name: Arc::from("bash"),
        }));
        assert!(!renderable(&DisplayMessage::new(tool, String::new())));
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
