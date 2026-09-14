//! Raw terminal sequences emitted by the block renderer.
//!
//! A raw object is a constrained final-writer primitive, not arbitrary
//! terminal text and not an escape hatch. The renderer only describes the
//! payload and the rectangle it wants; it never writes to the terminal. Rust
//! paints a placeholder there, decides absolute placement and clipping, and
//! writes the sequence only after the frame draw.
//!
//! A sequence must be cursor-local: it may set graphic rendition, move the
//! cursor, and save or restore it. It must not change modes, scroll, touch the
//! alternate screen, use OSC, or carry a graphics payload (APC/DCS). Persistent
//! graphics need a Rust-controlled teardown the cell diff cannot provide, so
//! they are refused until that protocol exists. Anything refused leaves the
//! block on its Rust rendering.

use ratatui::layout::Rect;

/// Why a raw sequence was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawViolation {
    Empty,
    Unfinished,
    Control,
    Osc,
    PrivateMode,
    ScreenControl,
    /// A graphics payload (APC/DCS), refused until there is a teardown.
    Graphics,
}

/// A raw sequence and the rectangle it occupies.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placement {
    pub rect: Rect,
    pub seq: String,
}

/// The last frame's placements, used to answer which rects the final writer
/// must repaint because a placement moved, changed, or disappeared.
/// Repainting is required: a cell diff would believe the placeholder cells
/// are still on screen and leave the old sequence there.
#[derive(Default)]
pub(crate) struct RawOverlay {
    placements: Vec<Placement>,
}

impl RawOverlay {
    /// Records this frame's placements and answers the rects that differ
    /// from the previous frame: old rects that left or changed, plus new
    /// rects that appeared or changed. The caller force-repaints them,
    /// because a cell diff would keep the stale sequence on screen.
    pub(crate) fn commit(&mut self, current: Vec<Placement>) -> Vec<Rect> {
        let mut rects: Vec<Rect> = self
            .placements
            .iter()
            .filter(|old| !current.contains(old))
            .map(|old| old.rect)
            .collect();
        rects.extend(
            current
                .iter()
                .filter(|new| !self.placements.contains(new))
                .map(|new| new.rect),
        );
        self.placements = current;
        rects
    }

    /// This frame's placements, in the order the panel recorded them.
    pub(crate) fn placements(&self) -> &[Placement] {
        &self.placements
    }
}

/// Clips `rect` to `viewport`, `None` when nothing of it is visible.
pub(crate) fn clip(rect: Rect, viewport: Rect) -> Option<Rect> {
    let x1 = rect.x.max(viewport.x);
    let y1 = rect.y.max(viewport.y);
    let x2 = (rect.x + rect.width).min(viewport.x + viewport.width);
    let y2 = (rect.y + rect.height).min(viewport.y + viewport.height);
    (x2 > x1 && y2 > y1).then(|| Rect::new(x1, y1, x2 - x1, y2 - y1))
}

/// Accepts only cursor-local sequences. See the module docs for the rule.
pub(crate) fn validate(seq: &str) -> Result<(), RawViolation> {
    let bytes = seq.as_bytes();
    if bytes.is_empty() {
        return Err(RawViolation::Empty);
    }
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => i = escape(bytes, i)?,
            byte if byte < 0x20 => return Err(RawViolation::Control),
            _ => i += 1,
        }
    }
    Ok(())
}

fn escape(bytes: &[u8], i: usize) -> Result<usize, RawViolation> {
    match *bytes.get(i + 1).ok_or(RawViolation::Unfinished)? {
        b'[' => csi(bytes, i + 2),
        // DECSC / DECRC.
        b'7' | b'8' => Ok(i + 2),
        b']' => Err(RawViolation::Osc),
        // APC and DCS carry graphics that outlive the frame.
        b'_' | b'P' => Err(RawViolation::Graphics),
        _ => Err(RawViolation::Unfinished),
    }
}

/// Walks a CSI to its final byte. Private parameters and every final but
/// the cursor and rendition ones are refused.
fn csi(bytes: &[u8], mut i: usize) -> Result<usize, RawViolation> {
    while let Some(&byte) = bytes.get(i) {
        match byte {
            0x30..=0x3f => {
                if matches!(byte, b'?' | b'>' | b'=' | b'<') {
                    return Err(RawViolation::PrivateMode);
                }
                i += 1;
            }
            0x20..=0x2f => i += 1,
            0x40..=0x7e => {
                return match byte {
                    // CUU CUD CUF CUB CNL CPL CHA CUP HVP SGR SCOSC SCORC.
                    b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'f' | b'm' | b's'
                    | b'u' => Ok(i + 1),
                    _ => Err(RawViolation::ScreenControl),
                };
            }
            _ => return Err(RawViolation::Unfinished),
        }
    }
    Err(RawViolation::Unfinished)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("\u{1b}[31m" ; "sgr")]
    #[test_case("\u{1b}[1;2H" ; "cursor_position")]
    #[test_case("\u{1b}[2A" ; "cursor_up")]
    #[test_case("\u{1b}7\u{1b}8" ; "save_restore")]
    #[test_case("\u{1b}[s\u{1b}[u" ; "scosc_scorc")]
    fn accepted(seq: &str) {
        assert_eq!(validate(seq), Ok(()));
    }

    #[test_case("" ; "empty")]
    #[test_case("\u{1b}]0;title\u{7}" ; "osc")]
    #[test_case("\u{1b}[?25l" ; "private_mode")]
    #[test_case("\u{1b}[?1049h" ; "alternate_screen")]
    #[test_case("\u{1b}[2J" ; "erase_display")]
    #[test_case("\u{1b}[3S" ; "scroll_up")]
    #[test_case("\u{1b}[r" ; "scroll_region")]
    #[test_case("\u{1b}c" ; "reset")]
    #[test_case("\u{1b}[31" ; "unfinished_csi")]
    #[test_case("\u{1b}" ; "lone_escape")]
    #[test_case("\u{1b}_Ga=T,f=100;\u{1b}\\" ; "kitty_graphics_deferred")]
    #[test_case("\u{1b}_Ga=T" ; "unterminated_apc")]
    #[test_case("\u{1b}Pq" ; "dcs_graphics_deferred")]
    #[test_case("\u{7}" ; "bare_control")]
    fn refused(seq: &str) {
        assert!(validate(seq).is_err());
    }

    #[test]
    fn clip_keeps_the_visible_part() {
        let viewport = Rect::new(0, 10, 80, 10);
        assert_eq!(
            clip(Rect::new(2, 12, 4, 2), viewport),
            Some(Rect::new(2, 12, 4, 2))
        );
        assert_eq!(
            clip(Rect::new(2, 8, 4, 4), viewport),
            Some(Rect::new(2, 10, 4, 2))
        );
        assert_eq!(clip(Rect::new(2, 0, 4, 4), viewport), None);
        assert_eq!(clip(Rect::new(200, 12, 4, 2), viewport), None);
    }

    const SGR_RED: &str = "\u{1b}[31m";
    const SGR_GREEN: &str = "\u{1b}[32m";

    fn placement(rect: Rect, seq: &str, viewport: Rect) -> Placement {
        Placement {
            rect: clip(rect, viewport).unwrap(),
            seq: seq.to_owned(),
        }
    }

    #[test]
    fn unchanged_placement_needs_no_invalidation() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        let rect = Rect::new(1, 1, 2, 1);
        overlay.commit(vec![placement(rect, SGR_RED, viewport)]);
        assert!(
            overlay
                .commit(vec![placement(rect, SGR_RED, viewport)])
                .is_empty()
        );
    }

    #[test]
    fn moving_or_changing_placement_invalidates_both_rects() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        overlay.commit(vec![placement(Rect::new(1, 1, 2, 1), SGR_RED, viewport)]);
        let rects = overlay.commit(vec![placement(Rect::new(1, 5, 2, 1), SGR_GREEN, viewport)]);
        assert!(rects.contains(&Rect::new(1, 1, 2, 1)));
        assert!(rects.contains(&Rect::new(1, 5, 2, 1)));
    }

    #[test]
    fn placements_expose_the_current_frame() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        let first = placement(Rect::new(1, 1, 2, 1), SGR_RED, viewport);
        overlay.commit(vec![first.clone()]);
        assert_eq!(overlay.placements(), std::slice::from_ref(&first));
    }

    #[test]
    fn vanished_placement_invalidates_its_rect() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        overlay.commit(vec![placement(Rect::new(1, 1, 2, 1), SGR_RED, viewport)]);
        assert_eq!(overlay.commit(Vec::new()), vec![Rect::new(1, 1, 2, 1)]);
    }
}
