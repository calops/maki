//! The single writer for raw terminal sequences.
//!
//! The transcript lays out a [`Placement`] per raw block and Rust paints
//! placeholder cells there. This runs after the frame draw. Each placement
//! establishes and restores terminal state itself: it saves the cursor, moves
//! to the Rust-computed rectangle, emits the sequence, resets the graphic
//! rendition, and restores the cursor, so a sequence can never leak attributes
//! or leave the cursor somewhere else. The frame is flushed once at the end.

use std::io::{Result, Write};

use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::queue;
use crossterm::style::{Attribute, SetAttribute};

use crate::components::messages::Placement;

pub(crate) fn write_sequences(out: &mut impl Write, placements: &[Placement]) -> Result<()> {
    if placements.is_empty() {
        return Ok(());
    }
    for placement in placements {
        queue!(
            out,
            SavePosition,
            MoveTo(placement.rect.x, placement.rect.y)
        )?;
        out.write_all(placement.seq.as_bytes())?;
        queue!(out, SetAttribute(Attribute::Reset), RestorePosition)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    const SEQ: &str = "\u{1b}[31m";
    const RESET: &str = "\u{1b}[0m";
    const X: u16 = 3;
    const Y: u16 = 4;
    // `MoveTo` is one-based, so its target cell is `(Y + 1, X + 1)`.
    const MOVE_TO: &str = "\u{1b}[5;4H";

    fn placement() -> Placement {
        Placement {
            rect: Rect::new(X, Y, 1, 1),
            seq: SEQ.to_owned(),
        }
    }

    #[test]
    fn establishes_and_restores_state_around_the_sequence() {
        let mut out = Vec::new();
        write_sequences(&mut out, &[placement()]).unwrap();
        let text = String::from_utf8(out).unwrap();
        let move_at = text.find(MOVE_TO).expect("cursor move");
        let seq_at = text.find(SEQ).expect("sequence");
        let reset_at = text.find(RESET).expect("rendition reset");
        assert!(move_at < seq_at && seq_at < reset_at);
    }

    #[test]
    fn empty_placements_write_nothing() {
        let mut out = Vec::new();
        write_sequences(&mut out, &[]).unwrap();
        assert!(out.is_empty());
    }
}
