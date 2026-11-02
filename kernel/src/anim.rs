//! Animation: how far through a transition a frame is, in integer arithmetic.
//!
//! # Why there is no floating point here
//!
//! The kernel is built for `x86_64-unknown-none`, which forbids SSE, and it links no `libm`.
//! So an easing curve cannot be a formula evaluated per frame: `1 - (1 - t)^3` has no `t` in
//! this arithmetic. Every curve is a table of 33 samples - the ends and every thirty-second
//! between - and a frame's progress is one division and one table index. The tables are
//! generated from the real curves rather than eyeballed, which is why they are `const` data
//! with a test that checks the shape they describe: monotonic, starting at zero, ending at
//! full.
//!
//! # Why an animation is a number, not a loop
//!
//! [`Animation`] holds a start tick and a duration. It does not run, hold state or call
//! anything: a frame asks it how far along it is, and that answer depends only on the clock.
//! A frame that is dropped therefore does not desynchronise the animation - the next frame is
//! simply further along - and two frames drawing the same tick draw the same picture, which is
//! what makes an animation testable without a screen.
//!
//! Progress is `u8`: zero to [`FULL`] inclusive. That is the precision an alpha or a position
//! interpolation needs, and it keeps every step of the arithmetic in a machine word.

/// The number of intervals a curve is sampled into. A curve has [`SAMPLES`] + 1 entries: one
/// at each end and one at every thirty-second between.
pub const SAMPLES: usize = 32;

/// Full progress: the animation has finished, and an interpolated value is the target's.
pub const FULL: u8 = u8::MAX;

/// `1 - (1 - t)^3`, sampled: fast at the start, settling at the end.
///
/// The curve a window opening or a panel sliding in uses. It covers more than half its
/// distance in the first third, which is what makes a short transition feel immediate rather
/// than slow.
pub const EASE_OUT: [u8; SAMPLES + 1] = [
    0, 23, 45, 65, 84, 102, 118, 133, 147, 160, 172, 183, 193, 202, 210, 217, 223, 229, 234, 238,
    242, 245, 247, 249, 251, 252, 253, 254, 255, 255, 255, 255, 255,
];

/// `t^2 * (3 - 2t)`, sampled: smoothstep, symmetric about the middle.
///
/// The curve something that was already on screen uses - a focus ring, a colour change - where
/// a jump at either end would be visible.
pub const EASE_IN_OUT: [u8; SAMPLES + 1] = [
    0, 1, 3, 6, 11, 17, 24, 31, 40, 49, 59, 70, 81, 92, 104, 116, 128, 139, 151, 163, 174, 185,
    196, 206, 215, 224, 231, 238, 244, 249, 252, 254, 255,
];

/// `t`, sampled: a linear change, for an animation that should look mechanical.
pub const LINEAR: [u8; SAMPLES + 1] = [
    0, 7, 15, 23, 31, 39, 47, 55, 63, 71, 79, 87, 95, 103, 111, 119, 127, 135, 143, 151, 159, 167,
    175, 183, 191, 199, 207, 215, 223, 231, 239, 247, 255,
];

/// Which curve an animation follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Curve {
    /// Fast at the start, settling: opening, sliding in.
    #[default]
    EaseOut,
    /// Smooth at both ends: colour and focus changes.
    EaseInOut,
    /// A constant rate.
    Linear,
}

impl Curve {
    /// The sample table for this curve.
    pub const fn table(self) -> &'static [u8; SAMPLES + 1] {
        match self {
            Curve::EaseOut => &EASE_OUT,
            Curve::EaseInOut => &EASE_IN_OUT,
            Curve::Linear => &LINEAR,
        }
    }

    /// How far along this curve `elapsed` out of `duration` is, in the range `0..=`[`FULL`].
    ///
    /// A zero-length animation is finished: the alternative is a division by zero, and a
    /// transition that never starts is not what a caller who asked for no duration wants.
    pub fn progress(self, elapsed: u64, duration: u64) -> u8 {
        if duration == 0 || elapsed >= duration {
            return FULL;
        }
        let index = (elapsed * SAMPLES as u64 / duration) as usize;
        self.table()[index.min(SAMPLES)]
    }
}

/// One transition: when it started, and how long it lasts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Animation {
    /// The tick the animation started on.
    pub start: u64,
    /// How many ticks it takes. Zero is finished immediately.
    pub duration: u64,
    /// How the progress is shaped.
    pub curve: Curve,
}

impl Animation {
    /// An animation that starts at `start` and lasts `duration` ticks.
    pub const fn new(start: u64, duration: u64, curve: Curve) -> Self {
        Self {
            start,
            duration,
            curve,
        }
    }

    /// How far along it is at `now`, in the range `0..=`[`FULL`].
    ///
    /// A `now` before the start reads as zero rather than underflowing: an animation whose
    /// start is in the future has not begun, and a wrapping subtraction would report it as
    /// nearly finished.
    pub fn progress(&self, now: u64) -> u8 {
        if now <= self.start {
            return 0;
        }
        self.curve.progress(now - self.start, self.duration)
    }

    /// Whether it has finished at `now`, which is when a caller stops repainting for it.
    pub fn done(&self, now: u64) -> bool {
        now > self.start && now - self.start >= self.duration
    }

    /// The tick it finishes on.
    pub fn ends(&self) -> u64 {
        self.start.saturating_add(self.duration)
    }
}

/// `from` to `to` at `progress`, rounded to nearest.
///
/// The one interpolation every animated value uses: an alpha, a colour channel, a coordinate.
/// Rounded rather than truncated because a fade that ends one step short of its target is a
/// frame that never quite arrives.
pub fn lerp(from: u8, to: u8, progress: u8) -> u8 {
    let span = i32::from(to) - i32::from(from);
    let scaled = span * i32::from(progress);
    // Half of 255 before the division: the same rounding `blend_channel` uses.
    let rounded = if scaled >= 0 {
        (scaled + 127) / 255
    } else {
        (scaled - 127) / 255
    };
    (i32::from(from) + rounded).clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_curve_starts_at_nothing_ends_at_full_and_never_goes_back() {
        for curve in [Curve::EaseOut, Curve::EaseInOut, Curve::Linear] {
            let table = curve.table();
            assert_eq!(table[0], 0, "{curve:?} starts part-way");
            assert_eq!(table[SAMPLES], FULL, "{curve:?} does not reach the end");
            for pair in table.windows(2) {
                assert!(
                    pair[1] >= pair[0],
                    "{curve:?} goes backwards: {} then {}",
                    pair[0],
                    pair[1]
                );
            }
        }
        // The shape each curve is for, stated as the thing it has to do: ease-out is more
        // than halfway at the halfway point, and smoothstep is exactly halfway.
        assert!(EASE_OUT[SAMPLES / 2] > 128);
        assert_eq!(EASE_IN_OUT[SAMPLES / 2], 128);
    }

    #[test]
    fn progress_walks_the_curve_and_finishes_exactly_on_time() {
        let animation = Animation::new(100, 40, Curve::EaseOut);
        assert_eq!(animation.progress(100), 0, "it had not started");
        assert_eq!(
            animation.progress(90),
            0,
            "a start in the future is not progress"
        );
        assert!(!animation.done(120));
        assert_eq!(animation.progress(140), FULL);
        assert!(animation.done(141), "a tick past the end is finished");
        assert_eq!(animation.ends(), 140);
        // Monotone across the run: an animation that stepped back would flicker.
        let mut previous = 0;
        for now in 100..=140 {
            let progress = animation.progress(now);
            assert!(progress >= previous, "progress went backwards at {now}");
            previous = progress;
        }
    }

    #[test]
    fn a_zero_length_animation_is_finished_rather_than_a_division_by_zero() {
        let animation = Animation::new(10, 0, Curve::Linear);
        assert_eq!(animation.progress(11), FULL);
        assert_eq!(animation.progress(10), 0);
        assert!(animation.done(11));
        assert_eq!(Curve::EaseOut.progress(5, 0), FULL);
    }

    #[test]
    fn interpolation_ends_on_the_target_exactly() {
        assert_eq!(lerp(0, 255, 0), 0);
        assert_eq!(lerp(0, 255, FULL), 255);
        assert_eq!(lerp(255, 0, 0), 255);
        assert_eq!(lerp(255, 0, FULL), 0);
        // Halfway is halfway, rounded to nearest rather than truncated.
        assert_eq!(lerp(0, 255, 128), 128);
        assert_eq!(lerp(20, 40, 128), 30);
        // A value that does not move stays where it is, at every progress.
        for progress in [0, 1, 127, 200, 255] {
            assert_eq!(lerp(70, 70, progress), 70);
        }
        // Far apart, and in both directions, nothing wraps or saturates early.
        assert_eq!(lerp(255, 0, 254), 1);
        assert_eq!(lerp(0, 255, 1), 1);
    }
}
