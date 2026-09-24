//! Numeric **value objects**: [`Timer`], the [`RaceClock`] and
//! [`CompensatedAccumulator`] (Kahan summation) used for stable per-tick
//! accumulation.

use serde::{Deserialize, Serialize};

/// A simple countdown/elapsed timer.
///
/// Timers are usually initialized to a negative value and advanced each tick;
/// expiry is detected by checking `t >= 0`. Doing it this way (rather than
/// counting down to zero) lets the code that *checks* a duration be separate
/// from the code that *initializes* it with a particular duration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Timer {
    pub t: f64,
}

impl Timer {
    pub fn new(t: f64) -> Self {
        Timer { t }
    }

    /// Advance the timer by `dt`.
    pub fn advance(&mut self, dt: f64) {
        self.t += dt;
    }
}

/// The race clock as the game keeps it: 0 at the gate, and each tick adds the
/// step in float32, `t(n+1) = float32(t(n) + float32(dt))`.
///
/// Every frame time and every skill event time in the 117 tournament
/// recordings is one value of that sum with the game's 0.0666 s tick (15,840
/// frames and 20,874 skill events, bit for bit). A float64 sum of the same
/// float32 step leaves those values at tick 3, and 0.0666 n at tick 1; the
/// float32 sum itself runs 0.85 ms behind 0.0666 n by tick 1426 (95 s).
/// Readers get the float32 value widened to f64, so a time written back as
/// float32 (the replay's frames and events) is the game's bit pattern.
///
/// The clock also counts its ticks, for readers that index frames.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RaceClock {
    seconds: f32,
    ticks: u32,
}

impl RaceClock {
    /// The clock at the gate: 0 s, no ticks.
    pub const fn new() -> Self {
        RaceClock {
            seconds: 0.0,
            ticks: 0,
        }
    }

    /// The clock after `ticks` steps of `dt` from the gate.
    pub fn after_ticks(ticks: u32, dt: f64) -> Self {
        let mut clock = RaceClock::new();
        for _ in 0..ticks {
            clock.advance(dt);
        }
        clock
    }

    /// One step of `dt` seconds, summed in float32.
    pub fn advance(&mut self, dt: f64) {
        self.seconds += dt as f32;
        self.ticks += 1;
    }

    /// Seconds since the gate.
    pub fn seconds(&self) -> f64 {
        f64::from(self.seconds)
    }

    /// Steps taken since the gate.
    pub fn ticks(&self) -> u32 {
        self.ticks
    }
}

/// Kahan (compensated) summation accumulator.
///
/// Tracks a running error term so that adding many small increments to a large
/// running total does not lose precision — important because the simulation
/// accumulates speed/position modifiers over thousands of ticks.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompensatedAccumulator {
    pub acc: f64,
    pub err: f64,
}

impl CompensatedAccumulator {
    pub fn new(acc: f64) -> Self {
        CompensatedAccumulator { acc, err: 0.0 }
    }

    /// Add `n`, folding the rounding error into `err`. Mirrors the reference
    /// TypeScript implementation exactly.
    pub fn add(&mut self, n: f64) {
        let t = self.acc + n;
        if self.acc.abs() >= n.abs() {
            self.err += self.acc - t + n;
        } else {
            self.err += n - t + self.acc;
        }
        self.acc = t;
    }

    /// The compensated total (`acc + err`).
    pub fn total(&self) -> f64 {
        self.acc + self.err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_advances() {
        let mut timer = Timer::new(-1.0);
        timer.advance(0.5);
        timer.advance(0.5);
        assert_eq!(timer.t, 0.0);
    }

    /// The clock is the float32 sum of the float32 step, as the recordings'
    /// frame and event times are. The literals are recorded times, bit for
    /// bit: ticks 1, 2, 15, 16 (the first of the sparse frames), 1424 and
    /// 1426 are frames of 10104-r0006, tick 150 a skill event of 10104-r0016
    /// and tick 151 one of 10104-r0013.
    #[test]
    fn the_race_clock_sums_float32_steps() {
        let dt = f64::from(0.0666_f32);
        let mut clock = RaceClock::new();
        assert_eq!(clock.seconds().to_bits(), 0.0_f64.to_bits());
        let mut game = 0.0_f32;
        for tick in 1..=1500_u32 {
            clock.advance(dt);
            game += 0.0666_f32;
            assert_eq!(clock.ticks(), tick);
            assert_eq!(
                clock.seconds().to_bits(),
                f64::from(game).to_bits(),
                "tick {tick}"
            );
        }
        let at = |n| RaceClock::after_ticks(n, dt).seconds();
        assert_eq!(at(1), 0.066600002348423);
        assert_eq!(at(2), 0.133200004696846);
        assert_eq!(at(15), 0.9990001916885376);
        assert_eq!(at(16), 1.0656001567840576);
        assert_eq!(at(150), 9.989988327026367);
        assert_eq!(at(151), 10.056588172912598);
        assert_eq!(at(1424), 94.83755493164062);
        assert_eq!(at(1426), 94.97074890136719);
    }

    /// The clock's float32 sum drifts from n ticks by under a tenth of a tick
    /// through tick 5000 and rounds to n through tick 7581 (505 s), so a
    /// time read off it names its tick. Readers that compare ticks by
    /// rounding (the skill cooldowns) rest on this.
    #[test]
    fn the_race_clock_rounds_to_its_tick_through_a_long_race() {
        let dt = f64::from(0.0666_f32);
        let mut clock = RaceClock::new();
        for tick in 1..=7581_u32 {
            clock.advance(dt);
            let ticks = clock.seconds() / dt;
            assert_eq!(ticks.round(), f64::from(tick), "tick {tick}");
            if tick <= 5000 {
                assert!(
                    (ticks - f64::from(tick)).abs() < 0.1,
                    "tick {tick}: {ticks}"
                );
            }
        }
    }

    #[test]
    fn compensated_accumulator_beats_naive_sum() {
        // Adding a tiny value many times to a large base loses precision with a
        // naive f64 sum; the compensated accumulator recovers it.
        let mut acc = CompensatedAccumulator::new(1.0e8);
        let mut naive = 1.0e8_f64;
        for _ in 0..1_000_000 {
            acc.add(1.0e-3);
            naive += 1.0e-3;
        }
        let expected = 1.0e8 + 1_000_000.0 * 1.0e-3;
        let comp_err = (acc.total() - expected).abs();
        let naive_err = (naive - expected).abs();
        assert!(
            comp_err <= naive_err,
            "compensated error {comp_err} should not exceed naive error {naive_err}"
        );
    }
}
