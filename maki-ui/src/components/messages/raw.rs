//! Raw terminal sequences emitted by the block renderer.
//!
//! A raw object is a constrained final-writer primitive, not arbitrary
//! terminal text. It owns an explicit `width x height` rectangle; Rust
//! paints a placeholder there and writes the sequence only after the
//! frame draw. Rust, not Lua, decides absolute placement and clipping.
//!
//! A sequence must be cursor-local: it may set graphic rendition, move the
//! cursor, save or restore it, and carry an APC payload (kitty graphics).
//! It must not change modes, scroll, touch the alternate screen, or use
//! OSC, so a plugin cannot hijack terminal state. Anything else is
//! refused and the block falls back to its Rust rendering.

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
}

/// A raw sequence and the rectangle it occupies.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placement {
    pub rect: Rect,
    pub seq: String,
}

/// Rects the final writer must repaint because a placement moved,
/// changed, or disappeared. Repainting is required: a cell diff would
/// believe the placeholder cells are still on screen and leave the old
/// sequence there.
#[derive(Default)]
pub(crate) struct RawOverlay {
    current: Vec<Placement>,
    previous: Vec<Placement>,
}

impl RawOverlay {
    /// Starts a frame, rotating last frame's placements into the set the
    /// next [`Self::invalidate`] compares against.
    pub(crate) fn begin(&mut self) {
        self.previous = std::mem::take(&mut self.current);
    }

    /// Records a placement clipped to `viewport`.
    pub(crate) fn place(
        &mut self,
        rect: Rect,
        seq: &str,
        viewport: Rect,
    ) -> Result<(), RawViolation> {
        validate(seq)?;
        if let Some(rect) = clip(rect, viewport) {
            self.current.push(Placement {
                rect,
                seq: seq.to_owned(),
            });
        }
        Ok(())
    }

    /// Rects to force-repaint this frame: every placement that left, moved,
    /// changed, or scrolled away.
    pub(crate) fn invalidate(&self) -> Vec<Rect> {
        let mut rects: Vec<Rect> = self
            .previous
            .iter()
            .filter(|old| !self.current.contains(old))
            .map(|old| old.rect)
            .collect();
        rects.extend(
            self.current
                .iter()
                .filter(|new| !self.previous.contains(new))
                .map(|new| new.rect),
        );
        rects
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
        // APC, the introducer kitty graphics use.
        b'_' => apc(bytes, i + 2),
        b']' => Err(RawViolation::Osc),
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

/// Walks an APC payload to its String Terminator.
fn apc(bytes: &[u8], mut i: usize) -> Result<usize, RawViolation> {
    while let Some(&byte) = bytes.get(i) {
        match byte {
            0x1b => {
                return match bytes.get(i + 1) {
                    Some(b'\\') => Ok(i + 2),
                    _ => Err(RawViolation::Unfinished),
                };
            }
            byte if byte < 0x20 => return Err(RawViolation::Control),
            _ => i += 1,
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
    #[test_case("\u{1b}_Ga=T,f=100;\u{1b}\\" ; "kitty_apc")]
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
    #[test_case("\u{1b}_Ga=T" ; "unterminated_apc")]
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

    #[test]
    fn unchanged_placement_needs_no_invalidation() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        let rect = Rect::new(1, 1, 2, 1);
        overlay.begin();
        overlay.place(rect, "\u{1b}[31m", viewport).unwrap();
        overlay.begin();
        overlay.place(rect, "\u{1b}[31m", viewport).unwrap();
        assert!(overlay.invalidate().is_empty());
    }

    #[test]
    fn moving_or_changing_placement_invalidates_both_rects() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        overlay.begin();
        overlay.place(Rect::new(1, 1, 2, 1), "\u{1b}[31m", viewport)
            .unwrap();
        overlay.begin();
        overlay.place(Rect::new(1, 5, 2, 1), "\u{1b}[32m", viewport)
            .unwrap();
        let rects = overlay.invalidate();
        assert!(rects.contains(&Rect::new(1, 1, 2, 1)));
        assert!(rects.contains(&Rect::new(1, 5, 2, 1)));
    }

    #[test]
    fn vanished_placement_invalidates_its_rect() {
        let mut overlay = RawOverlay::default();
        let viewport = Rect::new(0, 0, 80, 24);
        overlay.begin();
        overlay.place(Rect::new(1, 1, 2, 1), "\u{1b}[31m", viewport)
            .unwrap();
        overlay.begin();
        assert_eq!(overlay.invalidate(), vec![Rect::new(1, 1, 2, 1)]);
    }

    #[test]
    fn scrolling_away_clips_the_placement_out() {
        let mut overlay = RawOverlay::default();
        overlay.begin();
        overlay
            .place(Rect::new(1, 1, 2, 1), "\u{1b}[31m", Rect::new(0, 0, 80, 24))
            .unwrap();
        overlay.begin();
        overlay
            .place(Rect::new(1, 1, 2, 1), "\u{1b}[31m", Rect::new(0, 5, 80, 24))
            .unwrap();
        assert_eq!(overlay.invalidate(), vec![Rect::new(1, 1, 2, 1)]);
    }

    #[test]
    fn refused_sequence_leaves_no_placement() {
        let mut overlay = RawOverlay::default();
        assert_eq!(
            overlay.place(Rect::new(0, 0, 1, 1), "\u{1b}]0;x\u{7}", Rect::new(0, 0, 80, 24)),
            Err(RawViolation::Osc)
        );
        assert!(overlay.invalidate().is_empty());
    }
}
