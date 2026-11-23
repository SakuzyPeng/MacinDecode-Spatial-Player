//! The switch between the panner's box and the room view.
//!
//! The two views draw the same listener and the same objects in two different
//! spaces, so cutting from one to the other loses the one thing a reader needs
//! to carry across: which object in the room is which one in the box. The
//! switch therefore plays out in four overlapping stages, each moving one
//! thing so the eye can follow it:
//!
//! 1. **The box becomes the unit box.** The panner's whole room shrinks or
//!    grows onto the Scene unit it is in the modelled room, the floor moves to
//!    the room's, the listener takes their real size, every object travels
//!    from where the panner puts it to where the room makes it sound, and the
//!    LFE cabinet, which the room gives no place, shrinks away.
//! 2. **The room rises.** Its walls, treatment and edges grow up from the
//!    floor while the ruler fades in, and the box hands over to the dashed
//!    unit box it now is.
//! 3. **The seat grows** under the listener.
//! 4. **The listener sits**, into the riding pose, with their ears coming to
//!    rest on the listening position.
//!
//! Leaving the room plays the same timeline backwards. Everything here is a
//! function of one number, `progress`, from `0.0` (the panner's box) to `1.0`
//! (the room); [`RoomSwitch`] is the clock that drives it, on wall time, so a
//! window that is hidden halfway through comes back settled.

use std::time::Instant;

use crate::head_tracking::Quaternion;

use super::params;
use super::room::RoomView;

/// The room a frame is drawn in, and how far the switch into it has gone.
#[derive(Debug, Clone, Copy)]
pub struct InRoom<'a> {
    pub view: &'a RoomView,
    /// `0.0` is the panner's box, `1.0` the room.
    pub progress: f32,
}

#[cfg(test)]
impl<'a> InRoom<'a> {
    /// All the way in.
    #[must_use]
    pub const fn settled(view: &'a RoomView) -> Self {
        Self {
            view,
            progress: 1.0,
        }
    }
}

impl InRoom<'_> {
    #[must_use]
    pub fn stages(self) -> Stages {
        Stages::at(self.progress)
    }
}

/// How far each stage of the switch has gone, each eased from `0.0` to `1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stages {
    /// The box onto the unit box, the floor, the listener's size, the objects
    /// and the LFE cabinet.
    pub morph: f32,
    /// The room up from its floor, and the box over to the dashed unit box.
    pub rise: f32,
    /// The seat.
    pub seat: f32,
    /// The listener, standing to seated.
    pub sit: f32,
}

impl Stages {
    /// Where the stages are at `progress`. They overlap a little, so the
    /// switch reads as one movement rather than four, but each starts only
    /// once the one it depends on is mostly done: the room rises around the
    /// unit box, the seat grows on the room's floor, and the listener sits on
    /// the seat.
    #[must_use]
    pub fn at(progress: f32) -> Self {
        Self {
            morph: stage(progress, 0.0, 0.40),
            rise: stage(progress, 0.30, 0.70),
            seat: stage(progress, 0.55, 0.85),
            sit: stage(progress, 0.65, 1.0),
        }
    }
}

/// One stage's eased share of the timeline between `start` and `end`.
/// Exactly `0.0` before it and exactly `1.0` after it, so a settled view is
/// drawn from exactly the numbers it was drawn from before there was a switch.
fn stage(progress: f32, start: f32, end: f32) -> f32 {
    let t = ((progress - start) / (end - start)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `a` to `b` by `amount`, returning either end exactly rather than by a
/// rounding error.
#[must_use]
pub fn mix(a: f32, b: f32, amount: f32) -> f32 {
    if amount <= 0.0 {
        a
    } else if amount >= 1.0 {
        b
    } else {
        (b - a).mul_add(amount, a)
    }
}

#[must_use]
pub fn mix3(a: [f32; 3], b: [f32; 3], amount: f32) -> [f32; 3] {
    std::array::from_fn(|axis| mix(a[axis], b[axis], amount))
}

/// The switch's clock: the room last drawn, which way the switch is going,
/// and where it was when it last turned.
#[derive(Debug, Clone, Default)]
pub struct RoomSwitch {
    /// The room being entered, shown, or left. Kept after the renderer stops
    /// asking for it, so that leaving has a room to leave.
    room: Option<RoomView>,
    /// Progress when `since` was taken.
    from: f32,
    /// `None` until the first frame, which settles rather than animates:
    /// a player that opens in the room does not walk into it.
    since: Option<Instant>,
    entering: bool,
}

impl RoomSwitch {
    /// Follow the room the frame should be in, and return the room to draw
    /// and how far into it the switch is, or `None` once the switch is all the
    /// way out. Another room replacing the one shown is a cut, not a switch:
    /// the listener is already seated.
    ///
    /// `pose` keeps a room being left turned with the head, because its
    /// head-locked objects are drawn through it until they are back in the box.
    pub fn follow(
        &mut self,
        target: Option<RoomView>,
        pose: Quaternion,
        now: Instant,
    ) -> Option<(RoomView, f32)> {
        let entering = target.is_some();
        if self.since.is_none() {
            self.from = if entering { 1.0 } else { 0.0 };
            self.since = Some(now);
            self.entering = entering;
        } else if entering != self.entering {
            self.from = self.progress(now);
            self.since = Some(now);
            self.entering = entering;
        }
        match target {
            Some(room) => self.room = Some(room),
            None => {
                if let Some(room) = &mut self.room {
                    room.turn_with(pose);
                }
            }
        }
        let progress = self.progress(now);
        if !self.entering && progress <= 0.0 {
            self.room = None;
        }
        self.room.clone().map(|room| (room, progress))
    }

    /// Whether the switch is still under way, and the frame needs another
    /// one after it.
    #[must_use]
    pub fn moving(&self, now: Instant) -> bool {
        let progress = self.progress(now);
        self.since.is_some() && progress > 0.0 && progress < 1.0
    }

    fn progress(&self, now: Instant) -> f32 {
        let Some(since) = self.since else {
            return self.from;
        };
        let travelled =
            now.saturating_duration_since(since).as_secs_f32() / params::ROOM_SWITCH_SECONDS;
        if self.entering {
            (self.from + travelled).min(1.0)
        } else {
            (self.from - travelled).max(0.0)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "the switch's ends are exact by construction, and that is what is checked"
)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn room() -> RoomView {
        let room = crate::room_package::RoomGeometry::parse(include_str!(
            "../../tests/fixtures/room-reference-7x6x3.2-corrected-v1.json"
        ))
        .unwrap();
        RoomView::new(&room, Some(1.5))
    }

    fn after(start: Instant, seconds: f32) -> Instant {
        start + Duration::from_secs_f32(seconds)
    }

    #[test]
    fn the_stages_start_and_end_exactly_and_in_order() {
        assert_eq!(
            Stages::at(0.0),
            Stages {
                morph: 0.0,
                rise: 0.0,
                seat: 0.0,
                sit: 0.0
            }
        );
        assert_eq!(
            Stages::at(1.0),
            Stages {
                morph: 1.0,
                rise: 1.0,
                seat: 1.0,
                sit: 1.0
            }
        );
        // The room rises around a box that has mostly arrived, the seat is
        // mostly there before the listener starts to sit on it.
        let rising = Stages::at(0.30);
        assert_eq!(rising.rise, 0.0);
        assert!(rising.morph > 0.8);
        let sitting = Stages::at(0.65);
        assert_eq!(sitting.sit, 0.0);
        assert!(sitting.seat > 0.2 && sitting.rise > 0.9 && sitting.morph == 1.0);
        let mut last = Stages::at(0.0);
        for step in 1..=100_u8 {
            let next = Stages::at(f32::from(step) / 100.0);
            assert!(next.morph >= last.morph && next.rise >= last.rise);
            assert!(next.seat >= last.seat && next.sit >= last.sit);
            last = next;
        }
    }

    #[test]
    fn mixing_returns_either_end_exactly() {
        let (a, b) = (0.1_f32, 0.7_f32);
        assert_eq!(mix(a, b, 0.0).to_bits(), a.to_bits());
        assert_eq!(mix(a, b, 1.0).to_bits(), b.to_bits());
        assert!((mix(a, b, 0.5) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn the_first_frame_settles_where_it_is() {
        let start = Instant::now();
        let mut inside = RoomSwitch::default();
        let (_, progress) = inside
            .follow(Some(room()), Quaternion::default(), start)
            .expect("a room on the first frame is drawn");
        assert_eq!(progress, 1.0);
        assert!(!inside.moving(start));

        let mut outside = RoomSwitch::default();
        assert!(outside.follow(None, Quaternion::default(), start).is_none());
        assert!(!outside.moving(start));
    }

    #[test]
    fn entering_and_leaving_take_the_switch_time_and_reverse_from_where_they_are() {
        let start = Instant::now();
        let pose = Quaternion::default();
        let mut switch = RoomSwitch::default();
        assert!(switch.follow(None, pose, start).is_none());

        let (_, progress) = switch.follow(Some(room()), pose, start).unwrap();
        assert_eq!(progress, 0.0);
        let half = params::ROOM_SWITCH_SECONDS / 2.0;
        let (_, progress) = switch
            .follow(Some(room()), pose, after(start, half))
            .unwrap();
        assert!((progress - 0.5).abs() < 1e-3);
        assert!(switch.moving(after(start, half)));

        // Turned back halfway, the room is left from halfway, and is still
        // there to leave even though nothing asks for it any more.
        let quarter = after(start, half + params::ROOM_SWITCH_SECONDS / 4.0);
        let (_, progress) = switch.follow(None, pose, after(start, half)).unwrap();
        assert!((progress - 0.5).abs() < 1e-3);
        let (_, progress) = switch.follow(None, pose, quarter).unwrap();
        assert!((progress - 0.25).abs() < 1e-3);
        assert!(
            switch
                .follow(None, pose, after(start, half * 2.0))
                .is_none(),
            "out of the room once the switch is done"
        );
        assert!(!switch.moving(after(start, half * 2.0)));

        let entered = after(start, 10.0);
        switch.follow(Some(room()), pose, entered);
        let (_, progress) = switch
            .follow(
                Some(room()),
                pose,
                after(entered, params::ROOM_SWITCH_SECONDS + 1.0),
            )
            .unwrap();
        assert_eq!(progress, 1.0);
    }
}
