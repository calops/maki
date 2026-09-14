//! Projection of transcript messages into the structured blocks the Lua
//! renderer receives, and the conversion of its render objects back into
//! ratatui lines.

use ratatui::text::Line;

use maki_lua::RenderObject;

use crate::components::{DisplayMessage, DisplayRole, lua_float::snapshot_to_line};

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

/// Render objects as ratatui lines. A raw object is not drawable as
/// lines, so a block containing one reports `None` and the caller keeps
/// the Rust rendering rather than dropping content.
pub(crate) fn lines_for(objects: &[RenderObject]) -> Option<Vec<Line<'static>>> {
    if objects
        .iter()
        .any(|object| matches!(object, RenderObject::Raw { .. }))
    {
        return None;
    }
    Some(
        objects
            .iter()
            .flat_map(|object| match object {
                RenderObject::Lines(lines) => lines.iter().map(snapshot_to_line).collect(),
                RenderObject::Raw { .. } => Vec::new(),
            })
            .collect(),
    )
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

    #[test]
    fn lines_for_refuses_raw_blocks() {
        let lines = vec![RenderObject::Lines(vec![SnapshotLine::plain("a".to_owned())])];
        assert!(lines_for(&lines).is_some());
        let mut mixed = lines;
        mixed.push(RenderObject::Raw {
            seq: "\u{1b}[31m".to_owned(),
            width: 1,
            height: 1,
        });
        assert!(lines_for(&mixed).is_none());
    }
}
