// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The draw commands a script emits, and what bounds them.

use crate::material::MaterialId;
use crate::ui::Colour;

/// How tall the virtual canvas is, in virtual pixels.
///
/// Width is whatever the window's aspect ratio makes it — see the module docs
/// for why a fixed height is the half worth fixing.
pub const VIRTUAL_HEIGHT: u16 = 1080;

/// Where a command's coordinates are measured from.
///
/// Offsets always run **inward**: `x` grows right from a left anchor and left
/// from a right one, `y` grows down from a top anchor and up from a bottom one.
/// So `(8, 8)` is eight in from the corner whichever corner it is, and a script
/// that wants a badge in each corner writes the same numbers four times rather
/// than remembering which two need negating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Anchor {
    /// The top-left corner.
    TopLeft,
    /// The middle of the top edge.
    Top,
    /// The top-right corner.
    TopRight,
    /// The middle of the left edge.
    Left,
    /// The middle of the screen.
    Centre,
    /// The middle of the right edge.
    Right,
    /// The bottom-left corner.
    BottomLeft,
    /// The middle of the bottom edge.
    Bottom,
    /// The bottom-right corner.
    BottomRight,
}

impl Anchor {
    /// Resolves an anchored offset to virtual-canvas coordinates.
    ///
    /// `width` is the canvas width the window's aspect ratio produced; the
    /// height is always [`VIRTUAL_HEIGHT`].
    ///
    /// Returns `f32` because the caller is a renderer and rounding belongs
    /// there — not because anything here is fractional.
    #[must_use]
    pub fn resolve(self, width: f32, x: i16, y: i16) -> (f32, f32) {
        let height = f32::from(VIRTUAL_HEIGHT);
        let (x, y) = (f32::from(x), f32::from(y));
        let px = match self {
            Self::TopLeft | Self::Left | Self::BottomLeft => x,
            Self::Top | Self::Centre | Self::Bottom => width / 2.0 + x,
            Self::TopRight | Self::Right | Self::BottomRight => width - x,
        };
        let py = match self {
            Self::TopLeft | Self::Top | Self::TopRight => y,
            Self::Left | Self::Centre | Self::Right => height / 2.0 + y,
            Self::BottomLeft | Self::Bottom | Self::BottomRight => height - y,
        };
        (px, py)
    }
}

/// How full a bar is, in per-mille.
///
/// A fraction that cannot be a `NaN` and cannot be out of range: the
/// constructor clamps, so a script computing `health / max_health` with a
/// zero max gets an empty bar rather than a bar of infinite length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Fill(u16);

impl Fill {
    /// Completely empty.
    pub const EMPTY: Self = Self(0);
    /// Completely full.
    pub const FULL: Self = Self(1000);

    /// Clamps a per-mille value into range.
    #[must_use]
    pub const fn per_mille(value: i32) -> Self {
        if value <= 0 {
            Self::EMPTY
        } else if value >= 1000 {
            Self::FULL
        } else {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the branches above bound this to 1..1000"
            )]
            Self(value as u16)
        }
    }

    /// The fraction, for a renderer that wants to multiply a width by it.
    #[must_use]
    pub fn fraction(self) -> f32 {
        f32::from(self.0) / 1000.0
    }
}

/// One thing to draw.
///
/// **Not serialised, and that is the difference from [`crate::ui::Widget`].** A
/// widget tree crosses the wire, so it needs a stable postcard encoding and a
/// decoder that treats it as hostile. A HUD frame is produced by a script the
/// client is already running, in the client's own process, and is consumed by
/// the renderer a few microseconds later. Giving it a wire format would invite
/// somebody to send one.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A line of text.
    Text {
        /// Where it is measured from.
        anchor: Anchor,
        /// Inward offset from that anchor.
        x: i16,
        /// Inward offset from that anchor.
        y: i16,
        /// What it says. Bounded by [`Limits::text_bytes`].
        text: String,
        /// Height in virtual pixels.
        size: u16,
        /// Colour.
        colour: Colour,
    },
    /// A filled rectangle.
    Rect {
        /// Where it is measured from.
        anchor: Anchor,
        /// Inward offset from that anchor.
        x: i16,
        /// Inward offset from that anchor.
        y: i16,
        /// Width in virtual pixels.
        w: u16,
        /// Height in virtual pixels.
        h: u16,
        /// Fill colour.
        colour: Colour,
    },
    /// An image, by content hash, through the same pipeline as a block texture.
    Image {
        /// Where it is measured from.
        anchor: Anchor,
        /// Inward offset from that anchor.
        x: i16,
        /// Inward offset from that anchor.
        y: i16,
        /// Width in virtual pixels.
        w: u16,
        /// Height in virtual pixels.
        h: u16,
        /// The file's hash, as sent in the content table.
        hash: crate::proto::ContentHash,
    },
    /// A proportion of a rectangle, filled left to right.
    ///
    /// A rectangle and a fraction rather than two rectangles a script computes
    /// itself, because "compute the inner width" is where a health bar acquires
    /// its off-by-one and its division by zero.
    Bar {
        /// Where it is measured from.
        anchor: Anchor,
        /// Inward offset from that anchor.
        x: i16,
        /// Inward offset from that anchor.
        y: i16,
        /// Width in virtual pixels.
        w: u16,
        /// Height in virtual pixels.
        h: u16,
        /// How full.
        fill: Fill,
        /// Colour of the filled part.
        colour: Colour,
        /// Colour behind it.
        background: Colour,
    },
    /// A material, drawn as the game draws it.
    ///
    /// The one command a script could not write itself: it does not know what a
    /// block looks like, and should not have to — the client already has the
    /// atlas. This is what makes a hotbar expressible in tier 2 at all.
    Icon {
        /// Where it is measured from.
        anchor: Anchor,
        /// Inward offset from that anchor.
        x: i16,
        /// Inward offset from that anchor.
        y: i16,
        /// Both edges, in virtual pixels — icons are square.
        size: u16,
        /// What to draw.
        material: MaterialId,
    },
}

/// An engine HUD element a script may take over.
///
/// # Why this list is one long
///
/// It names what the engine actually draws, and the engine draws almost
/// nothing: a crosshair, chat, and the settings screen. That is Task 14's first
/// criterion stated as a type — the hotbar, the dig readout and anything else a
/// game wants are a MOD's, built on this API, and delete the mod and they are
/// gone. Naming a `Hotbar` here that the renderer does not have would be a
/// promise nothing could keep, and the first script to hide it would find that
/// out by nothing happening.
///
/// It grows when the engine grows an element, and only then.
///
/// **Chat is deliberately absent, and always will be** — see the module docs. A
/// script that could hide chat could hide a moderator's warning. Settings are
/// absent for the same reason: they must work with zero mods loaded, so they
/// cannot be at a mod's mercy either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Builtin {
    /// The crosshair in the middle of the screen.
    Crosshair,
}

impl Builtin {
    /// The name a script uses for this element.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Crosshair => "crosshair",
        }
    }

    /// Parses the name a script used, if it is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [Self::Crosshair]
            .into_iter()
            .find(|builtin| builtin.name() == name)
    }

    /// Every element there is, for a message that lists them.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Crosshair]
    }
}

/// What a frame may not exceed.
///
/// # Why a HUD needs its own limits when it is already budgeted
///
/// The instruction budget bounds how long a script may THINK. It does not bound
/// what it may ask the renderer to do: a hundred instructions can queue ten
/// thousand rectangles, and the cost of that lands on the frame after the
/// budget has already said yes. So the frame is bounded too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most commands in one frame.
    pub commands: usize,
    /// Longest single string, in bytes.
    pub text_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // Enough for a hotbar, a health bar, a compass and a debug readout
            // several times over; far short of anything that would cost a frame.
            commands: 512,
            // A HUD line that does not fit on the screen is not a HUD line.
            text_bytes: 512,
        }
    }
}

/// Why a draw command was refused.
///
/// Refusing is deliberately a fault the SCRIPT sees, not a silent drop: a
/// hotbar that quietly stops after 512 icons is a bug somebody debugs for an
/// hour, and a script that is told is a script whose author fixes it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HudError {
    /// The frame already holds as many commands as it may.
    #[error("this frame already has {limit} draw commands, which is all it may have")]
    TooManyCommands {
        /// The ceiling that was hit.
        limit: usize,
    },
    /// A string was longer than [`Limits::text_bytes`].
    #[error("that text is {len} bytes, over the {limit} a HUD string may be")]
    TextTooLong {
        /// How long it was.
        len: usize,
        /// The ceiling.
        limit: usize,
    },
}

/// Where a frame was, so it can be put back there.
///
/// Returned by [`Frame::checkpoint`] and handed to [`Frame::rewind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    commands: usize,
    hidden: usize,
}

/// What one script drew this frame.
///
/// Ordered: commands are drawn in the order they were issued, so a script can
/// put a label on top of a panel by saying the panel first. That is the whole
/// of the z-ordering model, and it is enough.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    commands: Vec<Command>,
    hidden: std::collections::BTreeSet<Builtin>,
    limits: Limits,
}

impl Frame {
    /// An empty frame with the given limits.
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            commands: Vec::new(),
            hidden: std::collections::BTreeSet::new(),
            limits,
        }
    }

    /// Adds a command, or says why it could not.
    ///
    /// # Errors
    ///
    /// [`HudError::TooManyCommands`] past [`Limits::commands`], and
    /// [`HudError::TextTooLong`] for an oversized string.
    pub fn push(&mut self, command: Command) -> Result<(), HudError> {
        if self.commands.len() >= self.limits.commands {
            return Err(HudError::TooManyCommands {
                limit: self.limits.commands,
            });
        }
        if let Command::Text { text, .. } = &command
            && text.len() > self.limits.text_bytes
        {
            return Err(HudError::TextTooLong {
                len: text.len(),
                limit: self.limits.text_bytes,
            });
        }
        self.commands.push(command);
        Ok(())
    }

    /// Marks an engine element as replaced by this script.
    pub fn hide(&mut self, builtin: Builtin) {
        self.hidden.insert(builtin);
    }

    /// What to draw, in order.
    #[must_use]
    pub fn commands(&self) -> &[Command] {
        &self.commands
    }

    /// Whether a script asked the engine not to draw this element.
    #[must_use]
    pub fn hides(&self, builtin: Builtin) -> bool {
        self.hidden.contains(&builtin)
    }

    /// Whether anything was drawn at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.hidden.is_empty()
    }

    /// Where the frame is now, for [`Frame::rewind`].
    #[must_use]
    pub fn checkpoint(&self) -> Mark {
        Mark {
            commands: self.commands.len(),
            hidden: self.hidden.len(),
        }
    }

    /// Discards everything added since a checkpoint.
    ///
    /// **Why one frame with rewind, rather than one frame per script.** A
    /// script that faults halfway through drawing has left a half-drawn HUD,
    /// and showing that is worse than showing nothing — a hotbar missing its
    /// last three slots reads as "the game lost my items". Rewinding takes that
    /// script's whole contribution away and leaves every other script's alone.
    ///
    /// A frame each would isolate faults just as well, but it would multiply
    /// [`Limits::commands`] by however many scripts a server pushed, which is a
    /// number the server chooses.
    ///
    /// Hidden builtins rewind on COUNT rather than by remembering which ones,
    /// which is exact because [`Frame::hide`] only ever adds.
    pub fn rewind(&mut self, mark: Mark) {
        self.commands.truncate(mark.commands);
        if self.hidden.len() > mark.hidden {
            // `BTreeSet` has no truncate, and there are at most a handful of
            // builtins, so rebuilding the prefix is cheaper than tracking
            // insertion order for a set that never exceeds four entries.
            let kept: std::collections::BTreeSet<Builtin> =
                self.hidden.iter().copied().take(mark.hidden).collect();
            self.hidden = kept;
        }
    }

    /// Empties the frame, keeping the allocation for the next one.
    ///
    /// A HUD is rebuilt sixty times a second and reusing the buffer is the
    /// difference between one allocation and sixty per second per script.
    pub fn clear(&mut self) {
        self.commands.clear();
        self.hidden.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: Colour = [255, 255, 255, 255];

    /// Resolved coordinates as whole virtual pixels.
    ///
    /// The values here are exact whole numbers, so truncating is lossless —
    /// and comparing integers keeps these assertions off float equality.
    /// (`round` is on the determinism deny-list for this crate; it is not
    /// needed, but reaching for it here is how one would find that out.)
    fn px(pair: (f32, f32)) -> (i32, i32) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "virtual-canvas coordinates are small integers"
        )]
        (pair.0 as i32, pair.1 as i32)
    }

    fn text(body: &str) -> Command {
        Command::Text {
            anchor: Anchor::TopLeft,
            x: 0,
            y: 0,
            text: body.to_owned(),
            size: 16,
            colour: WHITE,
        }
    }

    #[test]
    fn an_offset_runs_inward_from_whichever_corner_it_names() {
        // The property the docs promise: the same (8, 8) is eight pixels in
        // from the corner, four times, rather than two of them being outside
        // the screen.
        let width = 1920.0;
        let height = i32::from(VIRTUAL_HEIGHT);
        assert_eq!(px(Anchor::TopLeft.resolve(width, 8, 8)), (8, 8));
        assert_eq!(
            px(Anchor::TopRight.resolve(width, 8, 8)),
            (1912, 8),
            "x should run leftward from a right anchor"
        );
        assert_eq!(
            px(Anchor::BottomLeft.resolve(width, 8, 8)),
            (8, height - 8),
            "y should run upward from a bottom anchor"
        );
        assert_eq!(
            px(Anchor::BottomRight.resolve(width, 8, 8)),
            (1912, height - 8)
        );
    }

    #[test]
    fn a_centred_anchor_moves_with_the_window_width_and_a_left_one_does_not() {
        // This is the reason for a virtual HEIGHT rather than a virtual size:
        // the same script on a wider window puts a centred element in the
        // middle of THAT window, and a left-anchored one in the same place.
        assert_eq!(px(Anchor::Bottom.resolve(1440.0, 0, 16)).0, 720);
        assert_eq!(px(Anchor::Bottom.resolve(2560.0, 0, 16)).0, 1280);

        assert_eq!(
            px(Anchor::BottomLeft.resolve(1440.0, 0, 16)),
            px(Anchor::BottomLeft.resolve(2560.0, 0, 16)),
            "a left-anchored element should not care how wide the window is"
        );
    }

    #[test]
    fn a_fill_clamps_rather_than_letting_a_bar_run_off_the_screen() {
        assert_eq!(Fill::per_mille(-1), Fill::EMPTY);
        assert_eq!(Fill::per_mille(0), Fill::EMPTY);
        assert_eq!(Fill::per_mille(1000), Fill::FULL);
        assert_eq!(Fill::per_mille(9_000), Fill::FULL);
        assert!((Fill::per_mille(500).fraction() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn a_frame_refuses_the_command_past_its_limit_and_says_so() {
        let limits = Limits {
            commands: 2,
            ..Limits::default()
        };
        let mut frame = Frame::new(limits);
        frame.push(text("one")).expect("first");
        frame.push(text("two")).expect("second");
        assert_eq!(
            frame.push(text("three")),
            Err(HudError::TooManyCommands { limit: 2 }),
            "a frame past its limit should fault, not silently drop"
        );
        assert_eq!(frame.commands().len(), 2);
    }

    #[test]
    fn an_oversized_string_is_refused_without_being_stored() {
        let limits = Limits {
            text_bytes: 4,
            ..Limits::default()
        };
        let mut frame = Frame::new(limits);
        frame.push(text("ok")).expect("short");
        assert_eq!(
            frame.push(text("far too long")),
            Err(HudError::TextTooLong { len: 12, limit: 4 })
        );
        assert_eq!(frame.commands().len(), 1, "the refused string is not kept");
    }

    #[test]
    fn commands_keep_the_order_they_were_issued_in() {
        // The whole z-ordering model: a panel said first is behind the label
        // said second.
        let mut frame = Frame::new(Limits::default());
        frame.push(text("behind")).expect("first");
        frame.push(text("in front")).expect("second");
        let Command::Text { text: first, .. } = &frame.commands()[0] else {
            panic!("expected text");
        };
        assert_eq!(first, "behind");
    }

    #[test]
    fn chat_is_not_a_builtin_a_script_can_name() {
        // Moderation depends on chat being visible, so there is no spelling of
        // `hide_builtin` that takes it away.
        assert_eq!(Builtin::parse("crosshair"), Some(Builtin::Crosshair));
        assert_eq!(Builtin::parse("chat"), None);
        assert_eq!(Builtin::parse("settings"), None);
        assert_eq!(Builtin::parse("Crosshair"), None, "names are exact");
    }

    #[test]
    fn rewinding_takes_away_one_scripts_work_and_leaves_the_rest() {
        // A script that faults halfway through has left a half-drawn HUD, and
        // a hotbar missing its last three slots reads as lost items.
        let mut frame = Frame::new(Limits::default());
        frame.push(text("the first script")).expect("push");
        let mark = frame.checkpoint();
        frame.push(text("half a hotbar")).expect("push");
        frame.hide(Builtin::Crosshair);

        frame.rewind(mark);
        assert_eq!(frame.commands().len(), 1, "only the faulting script goes");
        let Command::Text { text: kept, .. } = &frame.commands()[0] else {
            panic!("expected text");
        };
        assert_eq!(kept, "the first script");
        assert!(
            !frame.hides(Builtin::Crosshair),
            "and the hide it asked for goes with it — the engine draws its own again"
        );
    }

    #[test]
    fn a_rewind_keeps_a_hide_that_was_already_there() {
        // The other half: an earlier script's hide is not collateral damage
        // when a later one faults.
        let mut frame = Frame::new(Limits::default());
        frame.hide(Builtin::Crosshair);
        let mark = frame.checkpoint();
        frame.push(text("the faulting script")).expect("push");
        frame.rewind(mark);
        assert!(frame.hides(Builtin::Crosshair));
        assert!(frame.commands().is_empty());
    }

    #[test]
    fn clearing_keeps_nothing_and_hiding_survives_only_the_frame_it_was_said_in() {
        // Immediate mode: a script that stops asking for the crosshair to be
        // hidden gets it back, without having to un-hide it.
        let mut frame = Frame::new(Limits::default());
        frame.hide(Builtin::Crosshair);
        frame.push(text("hi")).expect("push");
        assert!(frame.hides(Builtin::Crosshair));
        frame.clear();
        assert!(frame.is_empty());
        assert!(!frame.hides(Builtin::Crosshair));
    }
}
