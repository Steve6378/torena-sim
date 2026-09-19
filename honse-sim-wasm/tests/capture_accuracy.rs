//! Accuracy harness: replays captured game races through the engine and
//! scores the result against the game's own replay.
//!
//! Each fixture under `tests/fixtures/captures/` is one real race: the exact
//! `WasmRaceSimParams` the app would send (every runner pinned to its recorded
//! gate and start delay) plus the replay the server handed the client. A
//! downstream exports them with `pnpm run race:fixture` in torena-hub.
//!
//! Two runs per fixture, each over `ACCURACY_SAMPLES` seeds (default 8; raise
//! it when investigating one race, `ACCURACY_FIXTURE=<substring>` narrows the
//! set, `ACCURACY_FIXTURE_DIR=<dir>` scores another fixture directory with its
//! own `baseline.json`):
//!
//! - **free**: the engine rolls its own skill activations. Scores the whole
//!   model, randomness included, against one drawn outcome.
//! - **pinned**: skills the game fired are forced at the recorded distance,
//!   skills it never fired are removed, the last spurt starts where the game
//!   recorded it, rushed spells run where the game recorded them, and downhill
//!   mode runs where the recorded HP drain shows it. What is left is the
//!   deterministic part (speed, acceleration, HP) plus the rolls the replay
//!   does not record (section variance, dueling).
//!
//! `ACCURACY_TRACE=<gate>` prints every recorded frame for that gate against
//! the mean pinned simulation, for reading one runner's race line by line.
//! `ACCURACY_TRACE=field` prints every runner's position and lane per frame.
//!
//! This is a local harness, not a CI gate: the test is `#[ignore]`d so
//! `cargo test --workspace` skips it, and `-- --ignored` runs it. The scores
//! print with `--nocapture` and gate against `baseline.json`; run with
//! `UPDATE_ACCURACY_BASELINE=1` to accept a new baseline after a change that
//! is meant to move them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use honse_sim::contested::replay::RaceReplay;
use honse_sim::contested::{run_race_sim, RaceSimResult};
use serde::{Deserialize, Serialize};
use uma_sim_wasm::dto::{WasmForcedRegion, WasmRaceSimParams};

const TICK_SECONDS: f64 = 1.0 / 15.0;
/// `SimulateEventType.SKILL` in both the game replay and `RaceReplay`.
const EVENT_SKILL: i8 = 3;
const DEFAULT_SAMPLES: usize = 8;
/// Slack on the regression gate, so float noise across platforms never trips it.
const FINISH_TIME_TOLERANCE: f64 = 0.02;
const TRAJECTORY_TOLERANCE: f64 = 0.25;
const SPEED_TOLERANCE: f64 = 0.02;
const LANE_TOLERANCE: f64 = 0.05;
/// The game's lane unit: one ten-thousandth of the course width.
const LANE_UNITS_PER_COURSE_WIDTH: f64 = 10000.0;
const HP_TOLERANCE: f64 = 2.0;
/// Drift allowed on the pooled blocked share before the gate trips, as a
/// fraction of (frame, runner) samples. The recorded share is about 3.7% of
/// mid-race samples and 0.2% of late ones, and across the 53 fixtures the
/// engine sits within 0.028 of the recording on every one (mean 0.007), so one
/// percentage point is well above the noise between two runs of the same code
/// and still well under the level being measured.
const BLOCKED_SHARE_TOLERANCE: f64 = 0.01;
/// The doc's rushed-spell ladder: "Every 3 seconds while rushed, the uma has a
/// 55% chance to snap out of it. Rushed ends if the uma is still affected after
/// 12 seconds" (`docs/mechanics/README.md`, Rushed State), so a spell lasts
/// 3, 6, 9 or 12 s and nothing in between.
const RUSHED_LADDER: [f64; 4] = [3.0, 6.0, 9.0, 12.0];
/// Slack when placing a measured span on the ladder. Spans are multiples of a
/// tick (1/15 s) and the engine replay stores frame times as `f32`, so a span
/// that is exactly a rung can land a hair above it.
const LADDER_EPSILON: f64 = 1e-3;
/// `temptationMode` as the recording carries it: the enum value, not a flag.
const TEMPTATION_MODES: [(i64, &str); 4] = [(1, "SASHI"), (2, "SENKO"), (3, "NIGE"), (4, "BOOST")];

// ---------- fixture shape (mirrors torena-hub `export-sim-fixture.ts`) ----------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    source: Source,
    params: WasmRaceSimParams,
    horses: Vec<FixtureHorse>,
    observed: Observed,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    file: String,
    course_id: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureHorse {
    name: String,
    dropped_skill_ids: Vec<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observed {
    frames: Vec<ObservedFrame>,
    results: Vec<ObservedResult>,
    events: Vec<ObservedEvent>,
}

/// One recorded frame, columnar per gate: meters, centimeters per second, HP.
#[derive(Deserialize)]
struct ObservedFrame {
    time: f64,
    distance: Vec<f64>,
    speed: Vec<f64>,
    hp: Vec<f64>,
    /// `temptationMode` per gate: 0 calm, otherwise rushing. Absent in older fixtures.
    #[serde(default)]
    rushed: Vec<i64>,
    /// Gate blocking each runner in front, or -1. Absent in older fixtures.
    #[serde(default)]
    blocker: Vec<i64>,
    /// `lanePosition` per gate, 10000 units per course width. Absent in older fixtures.
    #[serde(default)]
    lane: Vec<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObservedResult {
    finish_order: i32,
    finish_time_raw: f64,
    last_spurt_start_distance: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObservedEvent {
    frame_time: f64,
    #[serde(rename = "type")]
    kind: i8,
    params: Vec<i64>,
}

// ---------- scores ----------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scores {
    /// Mean over runners of |mean simulated raw finish time - observed|, seconds.
    finish_time_mae: f64,
    /// Mean over runners of (mean simulated - observed) raw finish time; negative = engine too fast.
    finish_time_bias: f64,
    /// Fraction of rounds whose winner is the observed winner.
    winner_hit_rate: f64,
    /// Mean Spearman correlation between simulated and observed finish order.
    order_spearman: f64,
    /// Mean over runners of |mean simulated spurt start - observed|, meters.
    spurt_start_mae: f64,
    /// Mean over (runner, skill) of |simulated activation rate - observed fired (0/1)|.
    skill_activation_error: f64,
    /// Mean over (observed frame, runner) of |mean simulated distance - observed|, meters.
    trajectory_mae: f64,
    /// Mean over (observed frame, runner) of |mean simulated speed - observed|, m/s.
    #[serde(default)]
    speed_mae: f64,
    /// Mean over (observed frame, runner) of (mean simulated speed - observed), m/s.
    #[serde(default)]
    speed_bias: f64,
    /// Mean over (observed frame, runner) of |mean simulated HP - observed|.
    #[serde(default)]
    hp_mae: f64,
    /// Mean over (observed frame, runner) of (mean simulated HP - observed); negative = engine drains more.
    #[serde(default)]
    hp_bias: f64,
    /// Mean over (observed frame, runner) of agreement between the simulated
    /// rushed rate and the recorded rushed flag (1 = identical).
    #[serde(default)]
    rushed_agreement: f64,
    /// Mean over (observed frame, runner) of |mean simulated lane - observed|, meters from the rail.
    #[serde(default)]
    lane_mae: f64,
    /// Pooled blocked share, simulated minus recorded: the fraction of
    /// (frame, runner) samples with a runner blocking in front. `None` in a
    /// baseline written before this metric existed, which is what keeps the
    /// gate quiet until a baseline records a value to drift from.
    #[serde(default)]
    blocked_share_diff: Option<f64>,
    /// Free mode only: recorded rushed spells per rung of [`RUSHED_LADDER`].
    #[serde(default)]
    rushed_spell_ladder_observed: [f64; 4],
    /// Free mode only: simulated rushed spells per rung of [`RUSHED_LADDER`],
    /// per round, so the entry does not depend on the seed count.
    #[serde(default)]
    rushed_spell_ladder_sim: [f64; 4],
    /// Free mode only: mean simulated spell duration minus mean recorded one,
    /// seconds, both read off the ladder. `None` when either side has no spell.
    #[serde(default)]
    rushed_spell_duration_diff: Option<f64>,
}

/// One runner's state in one frame, in engine units.
#[derive(Debug, Clone, Copy, Default)]
struct FrameSample {
    /// Meters.
    distance: f64,
    /// Meters per second.
    speed: f64,
    hp: f64,
    /// 1 when rushed; averaged over rounds it is the rushed rate.
    rushed: f64,
    /// 1 when a runner in front blocks this one; averaged it is the blocked rate.
    blocked: f64,
    /// Meters from the inner rail; `None` when the recording has no lane column.
    lane: Option<f64>,
}

/// Running comparison of mean simulated samples against recorded ones.
#[derive(Debug, Default, Clone, Copy)]
struct FrameErrors {
    distance_abs: f64,
    speed_abs: f64,
    speed_signed: f64,
    hp_abs: f64,
    hp_signed: f64,
    rushed_agreement: f64,
    blocked_sim: f64,
    blocked_observed: f64,
    lane_abs: f64,
    /// Frames that carried a lane on both sides; lane MAE averages over these.
    lane_count: usize,
    count: usize,
}

impl FrameErrors {
    fn add(&mut self, sim: FrameSample, observed: FrameSample) {
        self.distance_abs += (sim.distance - observed.distance).abs();
        self.speed_abs += (sim.speed - observed.speed).abs();
        self.speed_signed += sim.speed - observed.speed;
        self.hp_abs += (sim.hp - observed.hp).abs();
        self.hp_signed += sim.hp - observed.hp;
        self.rushed_agreement += 1.0 - (sim.rushed - observed.rushed).abs();
        self.blocked_sim += sim.blocked;
        self.blocked_observed += observed.blocked;
        if let (Some(sim_lane), Some(observed_lane)) = (sim.lane, observed.lane) {
            self.lane_abs += (sim_lane - observed_lane).abs();
            self.lane_count += 1;
        }
        self.count += 1;
    }

    fn mean(&self, sum: f64) -> f64 {
        sum / self.count.max(1) as f64
    }

    /// Pooled blocked share, simulated minus recorded. The per-runner report
    /// has always printed the two sides; nothing scored the difference, so the
    /// front-block rule could drift without any gate noticing.
    fn blocked_share_diff(&self) -> f64 {
        self.mean(self.blocked_sim - self.blocked_observed)
    }

    /// Lane MAE over frames that carried a lane on both sides; `NaN` when
    /// there were none, so a missing lane column can never score as perfect.
    fn lane_mae(&self) -> f64 {
        if self.lane_count == 0 {
            return f64::NAN;
        }
        self.lane_abs / self.lane_count as f64
    }
}

/// The game's three race phases, by fraction of the course covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Early,
    Mid,
    Late,
}

impl Phase {
    const ALL: [Phase; 3] = [Phase::Early, Phase::Mid, Phase::Late];

    fn of(distance: f64, course_distance: f64) -> Phase {
        let fraction = distance / course_distance;
        if fraction < 1.0 / 6.0 {
            Phase::Early
        } else if fraction < 2.0 / 3.0 {
            Phase::Mid
        } else {
            Phase::Late
        }
    }
}

/// One accumulator per (runner) across rounds.
#[derive(Default)]
struct RunnerAccumulator {
    finish_time_sum: f64,
    spurt_start_sum: f64,
    fired: HashMap<i64, usize>,
}

/// `ACCURACY_FIXTURE_DIR=<dir>` scores a fixture set kept outside the crate
/// (its own `baseline.json` lives beside those fixtures); default is the
/// crate's `tests/fixtures/captures/`.
fn fixture_dir() -> PathBuf {
    std::env::var_os("ACCURACY_FIXTURE_DIR").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/captures"),
        PathBuf::from,
    )
}

/// `ACCURACY_FIXTURE=<substring>` narrows a run to matching fixture files.
fn load_fixtures() -> Vec<Fixture> {
    let only = std::env::var("ACCURACY_FIXTURE").unwrap_or_default();
    let mut paths: Vec<PathBuf> = fs::read_dir(fixture_dir())
        .expect("fixture directory")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && path.file_name().is_some_and(|name| name != "baseline.json")
                && path.to_string_lossy().contains(&only)
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let text = fs::read_to_string(path).expect("read fixture");
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
        })
        .collect()
}

fn samples() -> usize {
    std::env::var("ACCURACY_SAMPLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SAMPLES)
}

/// Observed skill activations per runner: skill id -> distance at which it fired.
fn observed_activations(observed: &Observed) -> Vec<HashMap<i64, f64>> {
    let mut per_runner: Vec<HashMap<i64, f64>> = vec![HashMap::new(); observed.results.len()];
    for event in observed.events.iter().filter(|e| e.kind == EVENT_SKILL) {
        let (Some(&gate), Some(&skill_id)) = (event.params.first(), event.params.get(1)) else {
            continue;
        };
        let gate = gate as usize;
        let distance = observed_distance_at(observed, gate, event.frame_time);
        per_runner[gate].entry(skill_id).or_insert(distance);
    }
    per_runner
}

/// Linear interpolation of a runner's recorded distance at `time`.
fn observed_distance_at(observed: &Observed, gate: usize, time: f64) -> f64 {
    let frames = &observed.frames;
    let after = frames.partition_point(|f| f.time < time);
    match (
        after.checked_sub(1).and_then(|i| frames.get(i)),
        frames.get(after),
    ) {
        (None, Some(next)) => next.distance[gate],
        (Some(prev), None) => prev.distance[gate],
        (Some(prev), Some(next)) => {
            let span = next.time - prev.time;
            let t = if span > 0.0 {
                (time - prev.time) / span
            } else {
                0.0
            };
            prev.distance[gate] + t * (next.distance[gate] - prev.distance[gate])
        }
        (None, None) => 0.0,
    }
}

/// Contiguous recorded rushed spells per gate, as `[start, end)` distances.
/// A spell that is still running in the last recorded frame ends there.
fn observed_rushed_regions(observed: &Observed) -> Vec<Vec<WasmForcedRegion>> {
    let runners = observed.results.len();
    let mut regions: Vec<Vec<WasmForcedRegion>> = vec![Vec::new(); runners];
    let mut open: Vec<Option<f64>> = vec![None; runners];
    for frame in &observed.frames {
        for gate in 0..runners {
            let rushed = frame.rushed.get(gate).is_some_and(|&mode| mode != 0);
            match (open[gate], rushed) {
                (None, true) => open[gate] = Some(frame.distance[gate]),
                (Some(start), false) => {
                    regions[gate].push(WasmForcedRegion {
                        start,
                        end: frame.distance[gate],
                    });
                    open[gate] = None;
                }
                _ => {}
            }
        }
    }
    if let Some(last) = observed.frames.last() {
        for (gate, start) in open.into_iter().enumerate() {
            if let Some(start) = start {
                regions[gate].push(WasmForcedRegion {
                    start,
                    end: last.distance[gate].max(start + 1.0),
                });
            }
        }
    }
    regions
}

// ---------- rushed spell durations ----------

/// The rushed spells found in one or more sampled series, with how long they
/// ran and where they sit on the doc's ladder.
///
/// A sampled series brackets a spell rather than timing it: the state is only
/// visible at the sample times, so a spell that shows up in samples
/// `first..=last` started somewhere in the gap before `first` and ended
/// somewhere in the gap after `last`. The span `times[last] - times[first]` is
/// therefore a lower bound and `span + gap_before + gap_after` an upper one -
/// the `[d, d + 2 * dt]` bracket of a uniformly sampled recording (the game's
/// mid-race sampling is ~1.066 s, 0.066 s near the start and the finish).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct SpellTally {
    count: usize,
    /// Sum of the spans (each a lower bound on the spell's duration), seconds.
    lower_sum: f64,
    /// Sum of the bracket upper bounds, seconds.
    upper_sum: f64,
    /// Sum of the ladder rungs the spells were placed on, seconds.
    ladder_sum: f64,
    /// Spells per rung of [`RUSHED_LADDER`].
    ladder: [usize; 4],
}

impl SpellTally {
    /// Add every contiguous rushed run of one sampled series (one runner in one
    /// recording, or one runner in one simulated round). A run still going in
    /// the last sample ends there, as [`observed_rushed_regions`] also assumes.
    fn add_series(&mut self, times: &[f64], rushed: &[bool]) {
        let mut open: Option<usize> = None;
        for (index, &on) in rushed.iter().enumerate() {
            match (open, on) {
                (None, true) => open = Some(index),
                (Some(first), false) => {
                    self.add_run(times, first, index - 1);
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(first) = open {
            self.add_run(times, first, rushed.len() - 1);
        }
    }

    /// One spell, seen in samples `first..=last`.
    fn add_run(&mut self, times: &[f64], first: usize, last: usize) {
        let (Some(&start), Some(&end)) = (times.get(first), times.get(last)) else {
            return;
        };
        let lower = end - start;
        // A run that touches the edge of the series has no gap to widen the
        // bracket with on that side, so its upper bound is under-stated. None
        // of the 53 recordings has such a run.
        let before = first
            .checked_sub(1)
            .and_then(|i| times.get(i))
            .map_or(0.0, |&previous| start - previous);
        let after = times.get(last + 1).map_or(0.0, |&next| next - end);
        let rung = ladder_rung(lower);
        self.count += 1;
        self.lower_sum += lower;
        self.upper_sum += lower + before + after;
        self.ladder_sum += RUSHED_LADDER[rung];
        self.ladder[rung] += 1;
    }

    fn merge(&mut self, other: &SpellTally) {
        self.count += other.count;
        self.lower_sum += other.lower_sum;
        self.upper_sum += other.upper_sum;
        self.ladder_sum += other.ladder_sum;
        for (slot, added) in self.ladder.iter_mut().zip(other.ladder) {
            *slot += added;
        }
    }

    /// Mean of the spans, seconds; `NaN` with no spells.
    fn mean_lower(&self) -> f64 {
        self.mean(self.lower_sum)
    }

    /// Mean of the bracket upper bounds, seconds; `NaN` with no spells.
    fn mean_upper(&self) -> f64 {
        self.mean(self.upper_sum)
    }

    /// Mean duration read off the ladder, seconds; `NaN` with no spells.
    fn mean_ladder(&self) -> f64 {
        self.mean(self.ladder_sum)
    }

    fn mean(&self, sum: f64) -> f64 {
        if self.count == 0 {
            return f64::NAN;
        }
        sum / self.count as f64
    }

    /// Share of the spells on each rung of [`RUSHED_LADDER`].
    fn shares(&self) -> [f64; 4] {
        let total = self.count.max(1) as f64;
        self.ladder.map(|n| n as f64 / total)
    }
}

/// The shortest rung of the doc's ladder a spell can sit on given that it ran
/// for at least `lower` seconds: every shorter rung is ruled out by the
/// recording. Spans past the top rung clamp to it, since the doc makes 12 s the
/// maximum duration (a debuff extension aside, which no recording marks).
fn ladder_rung(lower: f64) -> usize {
    RUSHED_LADDER
        .iter()
        .position(|&rung| lower <= rung + LADDER_EPSILON)
        .unwrap_or(RUSHED_LADDER.len() - 1)
}

/// Rushed spells on both sides of one fixture.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct SpellComparison {
    observed: SpellTally,
    /// Summed over rounds; per-round figures divide by `rounds`.
    sim: SpellTally,
    rounds: usize,
}

impl SpellComparison {
    fn merge(&mut self, other: &SpellComparison) {
        self.observed.merge(&other.observed);
        self.sim.merge(&other.sim);
        self.rounds += other.rounds;
    }
}

/// The recorded rushed spells of a fixture against the simulated ones.
///
/// The recorded side reads the `rushed` column of `observed.frames`; the
/// simulated side reads each round's raw replay, not the seed-averaged rushed
/// rate that `rushed_agreement` scores. That rate compares a mean against a 0/1
/// flag over a 0.51% base rate, so it stays near 1 however wrong the spell
/// lengths are; a duration histogram is what sees them.
fn rushed_spells(fixture: &Fixture, replays: &[RaceReplay]) -> SpellComparison {
    let runners = fixture.observed.results.len();
    SpellComparison {
        observed: observed_rushed_spells(&fixture.observed),
        sim: simulated_rushed_spells(replays, runners),
        rounds: replays.len(),
    }
}

/// Recorded rushed spells, over every gate of one recording.
fn observed_rushed_spells(observed: &Observed) -> SpellTally {
    let times: Vec<f64> = observed.frames.iter().map(|frame| frame.time).collect();
    let mut tally = SpellTally::default();
    for gate in 0..observed.results.len() {
        let rushed: Vec<bool> = observed
            .frames
            .iter()
            .map(|frame| frame.rushed.get(gate).is_some_and(|&mode| mode != 0))
            .collect();
        tally.add_series(&times, &rushed);
    }
    tally
}

/// Simulated rushed spells, over every runner of every round.
fn simulated_rushed_spells(replays: &[RaceReplay], runners: usize) -> SpellTally {
    let mut tally = SpellTally::default();
    for replay in replays {
        let times: Vec<f64> = replay
            .frames
            .iter()
            .map(|frame| f64::from(frame.time))
            .collect();
        for gate in 0..runners {
            let rushed: Vec<bool> = replay
                .frames
                .iter()
                .map(|frame| {
                    frame
                        .horses
                        .get(gate)
                        .is_some_and(|horse| horse.temptation_mode != 0)
                })
                .collect();
            tally.add_series(&times, &rushed);
        }
    }
    tally
}

/// Copy a fixture's spell tallies into its scores, the simulated side per round
/// so the entry does not depend on the seed count.
fn apply_spell_scores(scores: &mut Scores, comparison: &SpellComparison) {
    let rounds = comparison.rounds.max(1) as f64;
    scores.rushed_spell_ladder_observed = comparison.observed.ladder.map(|n| n as f64);
    scores.rushed_spell_ladder_sim = comparison.sim.ladder.map(|n| n as f64 / rounds);
    scores.rushed_spell_duration_diff = (comparison.observed.count > 0 && comparison.sim.count > 0)
        .then(|| comparison.sim.mean_ladder() - comparison.observed.mean_ladder());
}

/// Recorded `temptationMode` samples per entry of [`TEMPTATION_MODES`]. The
/// scoring flattens the column to `!= 0`; this keeps the enum value, so the
/// recorded mode split can be read against the engine's rushed strategy
/// override (`README.md`, Rushed State: front runners speed up, pace chasers
/// become front runners, late surgers 75/25, end closers 70/20/10).
fn observed_mode_counts(observed: &Observed) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for frame in &observed.frames {
        for &mode in &frame.rushed {
            if let Some(slot) = TEMPTATION_MODES
                .iter()
                .position(|&(value, _)| value == mode)
            {
                counts[slot] += 1;
            }
        }
    }
    counts
}

/// Simulated `temptationMode` samples per entry of [`TEMPTATION_MODES`], over
/// every round of one run.
fn simulated_mode_counts(replays: &[RaceReplay]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for frame in replays.iter().flat_map(|replay| &replay.frames) {
        for horse in &frame.horses {
            let mode = i64::from(horse.temptation_mode);
            if let Some(slot) = TEMPTATION_MODES
                .iter()
                .position(|&(value, _)| value == mode)
            {
                counts[slot] += 1;
            }
        }
    }
    counts
}

/// Documented HP drain per second at `speed`, before status modifiers.
fn documented_drain(speed: f64, course_distance: f64) -> f64 {
    let base_speed = 20.0 - (course_distance - 2000.0) / 1000.0;
    20.0 * (speed - base_speed + 12.0).powi(2) / 144.0
}

/// Downhill-mode spells per gate, read off the recorded HP drain.
///
/// The game never records the mode, but it records its effect: a one-second
/// sample on a downhill draining under half the documented rate (after the
/// guts multiplier from two-thirds on) is the mode's 0.4 factor and nothing
/// else. Pace-down is 0.6 and never gets that low. Rushed samples are skipped.
fn observed_downhill_regions(fixture: &Fixture) -> Vec<Vec<WasmForcedRegion>> {
    let observed = &fixture.observed;
    let course = &fixture.params.course;
    let on_downhill = |distance: f64| {
        course
            .slopes
            .iter()
            .any(|s| s.slope < 0.0 && distance >= s.start && distance < s.start + s.length)
    };
    let mood_coefficient = |mood: i32| 1.0 + f64::from(mood) * 0.02;
    let runners = observed.results.len();
    let mut regions: Vec<Vec<WasmForcedRegion>> = vec![Vec::new(); runners];
    for (gate, spells) in regions.iter_mut().enumerate() {
        let runner = &fixture.params.runners[gate];
        let guts = f64::from(runner.stats.guts) * mood_coefficient(runner.mood);
        let guts_modifier = 1.0 + 200.0 / (600.0 * guts).sqrt();
        let mut open: Option<f64> = None;
        for pair in observed.frames.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let dt = b.time - a.time;
            let (x0, x1) = (a.distance[gate], b.distance[gate]);
            let usable = dt >= 0.5
                && on_downhill(x0)
                && on_downhill(x1)
                && a.rushed.get(gate).is_none_or(|&m| m == 0)
                && b.rushed.get(gate).is_none_or(|&m| m == 0)
                && b.hp[gate] > 0.0;
            let mut in_mode = false;
            if usable {
                let speed = (a.speed[gate] + b.speed[gate]) / 200.0;
                let rate = (a.hp[gate] - b.hp[gate]) / dt;
                let mut expected = documented_drain(speed, course.distance);
                if (x0 + x1) / 2.0 >= course.distance * 2.0 / 3.0 {
                    expected *= guts_modifier;
                }
                in_mode = rate > 0.0 && rate / expected < 0.5;
            }
            match (open, in_mode) {
                (None, true) => open = Some(x0),
                (Some(start), false) if usable || !on_downhill(x1) => {
                    spells.push(WasmForcedRegion { start, end: x0 });
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(start) = open {
            let end = observed
                .frames
                .last()
                .map_or(start + 1.0, |f| f.distance[gate]);
            spells.push(WasmForcedRegion {
                start,
                end: end.max(start + 1.0),
            });
        }
    }
    regions
}

/// Force the observed activations, drop every skill the game never fired,
/// start each last spurt where the game recorded it, and script the recorded
/// rushed and downhill spells in place of the engine's rolls.
fn pin_outcomes(params: &mut WasmRaceSimParams, observed: &Observed, fixture: &Fixture) {
    let activations = observed_activations(observed);
    let rushed = observed_rushed_regions(observed);
    let downhill = observed_downhill_regions(fixture);
    params.settings.rushed_runners = Some(vec![false; params.runners.len()]);
    params.settings.downhill_runners = Some(vec![false; params.runners.len()]);
    for ((((runner, fired), result), spells), descents) in params
        .runners
        .iter_mut()
        .zip(&activations)
        .zip(&observed.results)
        .zip(rushed)
        .zip(downhill)
    {
        runner.forced_rushed_regions = spells;
        runner.forced_downhill_regions = descents;
        runner.forced_last_spurt_distance =
            Some(result.last_spurt_start_distance).filter(|d| *d > 0.0);
        runner.skills.retain(|skill| {
            skill_base_id(&skill.skill_id).is_some_and(|id| fired.contains_key(&id))
        });
        runner.forced_positions = fired
            .iter()
            .map(|(id, distance)| (id.to_string(), *distance))
            .collect();
    }
}

fn skill_base_id(skill_id: &str) -> Option<i64> {
    skill_id.split('-').next()?.parse().ok()
}

fn run_full(params: &WasmRaceSimParams, nsamples: usize) -> RaceSimResult {
    let mut domain = params
        .clone()
        .into_domain()
        .expect("fixture params convert");
    domain.nsamples = nsamples;
    run_race_sim(domain).expect("race runs")
}

fn run(params: &WasmRaceSimParams, nsamples: usize) -> Vec<RaceReplay> {
    run_full(params, nsamples).replays
}

fn spearman(sim_order: &[i32], observed_order: &[i32]) -> f64 {
    let n = sim_order.len() as f64;
    let d2: f64 = sim_order
        .iter()
        .zip(observed_order)
        .map(|(a, b)| f64::from(a - b).powi(2))
        .sum();
    1.0 - 6.0 * d2 / (n * (n * n - 1.0))
}

/// The simulated sample nearest `time`, matched by frame time rather than
/// index: the engine's replay starts at the first tick while the game records
/// a frame at 0, so indexes are one tick apart. `None` once the replay ends.
fn sim_sample_at(
    replay: &RaceReplay,
    gate: usize,
    time: f64,
    course_width: f64,
) -> Option<FrameSample> {
    let after = replay
        .frames
        .partition_point(|frame| f64::from(frame.time) < time);
    let candidates = [after.checked_sub(1), Some(after)];
    let index = candidates
        .into_iter()
        .flatten()
        .filter(|&i| i < replay.frames.len())
        .min_by(|&a, &b| {
            let da = (f64::from(replay.frames[a].time) - time).abs();
            let db = (f64::from(replay.frames[b].time) - time).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .filter(|&i| (f64::from(replay.frames[i].time) - time).abs() <= TICK_SECONDS)?;
    replay.frames.get(index).map(|frame| {
        let horse = frame.horses[gate];
        FrameSample {
            distance: f64::from(horse.distance),
            speed: f64::from(horse.speed) / 100.0,
            hp: f64::from(horse.hp),
            rushed: if horse.temptation_mode != 0 { 1.0 } else { 0.0 },
            blocked: if horse.block_front_horse_index >= 0 {
                1.0
            } else {
                0.0
            },
            lane: Some(f64::from(horse.lane_position) / LANE_UNITS_PER_COURSE_WIDTH * course_width),
        }
    })
}

fn observed_sample(frame: &ObservedFrame, gate: usize, course_width: f64) -> FrameSample {
    FrameSample {
        distance: frame.distance[gate],
        speed: frame.speed[gate] / 100.0,
        hp: frame.hp[gate],
        rushed: if frame.rushed.get(gate).is_some_and(|&mode| mode != 0) {
            1.0
        } else {
            0.0
        },
        blocked: if frame.blocker.get(gate).is_some_and(|&b| b >= 0) {
            1.0
        } else {
            0.0
        },
        lane: frame
            .lane
            .get(gate)
            .map(|units| units / LANE_UNITS_PER_COURSE_WIDTH * course_width),
    }
}

/// Mean simulated sample per (observed frame index, gate), over the rounds
/// still running at that frame.
fn mean_sim_samples(
    fixture: &Fixture,
    replays: &[RaceReplay],
) -> BTreeMap<(usize, usize), FrameSample> {
    let observed = &fixture.observed;
    let course_width = fixture.params.course.course_width;
    let runners = observed.results.len();
    let mut sums: BTreeMap<(usize, usize), (FrameSample, usize)> = BTreeMap::new();
    for replay in replays {
        for (frame_index, frame) in observed.frames.iter().enumerate() {
            for gate in 0..runners {
                if let Some(sample) = sim_sample_at(replay, gate, frame.time, course_width) {
                    let slot = sums.entry((frame_index, gate)).or_default();
                    slot.0.distance += sample.distance;
                    slot.0.speed += sample.speed;
                    slot.0.hp += sample.hp;
                    slot.0.rushed += sample.rushed;
                    slot.0.blocked += sample.blocked;
                    slot.0.lane = match (slot.0.lane, sample.lane) {
                        (Some(sum), Some(lane)) => Some(sum + lane),
                        (None, Some(lane)) => Some(lane),
                        (sum, None) => sum,
                    };
                    slot.1 += 1;
                }
            }
        }
    }
    sums.into_iter()
        .map(|(key, (sum, count))| {
            let n = count as f64;
            (
                key,
                FrameSample {
                    distance: sum.distance / n,
                    speed: sum.speed / n,
                    hp: sum.hp / n,
                    rushed: sum.rushed / n,
                    blocked: sum.blocked / n,
                    lane: sum.lane.map(|lane| lane / n),
                },
            )
        })
        .collect()
}

/// Frame errors over the recorded frames, optionally limited to one gate and
/// one phase (by the recorded distance).
fn frame_errors(
    fixture: &Fixture,
    means: &BTreeMap<(usize, usize), FrameSample>,
    gate: Option<usize>,
    phase: Option<Phase>,
) -> FrameErrors {
    let course_distance = fixture.params.course.distance;
    let mut errors = FrameErrors::default();
    for (&(frame_index, sample_gate), sim) in means {
        if gate.is_some_and(|g| g != sample_gate) {
            continue;
        }
        let observed = observed_sample(
            &fixture.observed.frames[frame_index],
            sample_gate,
            fixture.params.course.course_width,
        );
        if phase.is_some_and(|p| p != Phase::of(observed.distance, course_distance)) {
            continue;
        }
        errors.add(*sim, observed);
    }
    errors
}

fn score(fixture: &Fixture, replays: &[RaceReplay], skills_per_runner: &[Vec<i64>]) -> Scores {
    let observed = &fixture.observed;
    let runners = observed.results.len();
    let rounds = replays.len() as f64;
    let observed_order: Vec<i32> = observed.results.iter().map(|r| r.finish_order).collect();
    let observed_winner = observed_order.iter().position(|&o| o == 0);
    let fired = observed_activations(observed);

    let mut acc: Vec<RunnerAccumulator> =
        (0..runners).map(|_| RunnerAccumulator::default()).collect();
    let mut winner_hits = 0usize;
    let mut spearman_sum = 0.0;

    for replay in replays {
        let sim_order: Vec<i32> = replay.results.iter().map(|r| r.finish_order).collect();
        if sim_order.iter().position(|&o| o == 0) == observed_winner {
            winner_hits += 1;
        }
        spearman_sum += spearman(&sim_order, &observed_order);
        for (gate, result) in replay.results.iter().enumerate() {
            acc[gate].finish_time_sum += f64::from(result.finish_time_raw);
            acc[gate].spurt_start_sum += f64::from(result.last_spurt_start_distance);
        }
        let mut seen: HashSet<(usize, i32)> = HashSet::new();
        for event in replay.events.iter().filter(|e| e.kind == EVENT_SKILL) {
            let (Some(&gate), Some(&skill)) = (event.params.first(), event.params.get(1)) else {
                continue;
            };
            if seen.insert((gate as usize, skill)) {
                *acc[gate as usize]
                    .fired
                    .entry(i64::from(skill))
                    .or_insert(0) += 1;
            }
        }
    }

    let mut finish_abs = 0.0;
    let mut finish_signed = 0.0;
    let mut spurt_abs = 0.0;
    let mut spurt_count = 0usize;
    let mut skill_abs = 0.0;
    let mut skill_count = 0usize;
    for (gate, result) in observed.results.iter().enumerate() {
        let mean_finish = acc[gate].finish_time_sum / rounds;
        finish_abs += (mean_finish - result.finish_time_raw).abs();
        finish_signed += mean_finish - result.finish_time_raw;
        if result.last_spurt_start_distance > 0.0 {
            spurt_abs +=
                (acc[gate].spurt_start_sum / rounds - result.last_spurt_start_distance).abs();
            spurt_count += 1;
        }
        for skill in &skills_per_runner[gate] {
            let rate = acc[gate].fired.get(skill).copied().unwrap_or(0) as f64 / rounds;
            let observed_fired = if fired[gate].contains_key(skill) {
                1.0
            } else {
                0.0
            };
            skill_abs += (rate - observed_fired).abs();
            skill_count += 1;
        }
    }

    let frames = frame_errors(fixture, &mean_sim_samples(fixture, replays), None, None);

    Scores {
        finish_time_mae: finish_abs / runners as f64,
        finish_time_bias: finish_signed / runners as f64,
        winner_hit_rate: winner_hits as f64 / rounds,
        order_spearman: spearman_sum / rounds,
        spurt_start_mae: spurt_abs / spurt_count.max(1) as f64,
        skill_activation_error: skill_abs / skill_count.max(1) as f64,
        trajectory_mae: frames.mean(frames.distance_abs),
        speed_mae: frames.mean(frames.speed_abs),
        speed_bias: frames.mean(frames.speed_signed),
        hp_mae: frames.mean(frames.hp_abs),
        hp_bias: frames.mean(frames.hp_signed),
        rushed_agreement: frames.mean(frames.rushed_agreement),
        lane_mae: frames.lane_mae(),
        blocked_share_diff: Some(frames.blocked_share_diff()),
        // Free mode only, filled by `apply_spell_scores`: in pinned mode the
        // engine replays the recorded spells, so their durations are the
        // recording's own.
        rushed_spell_ladder_observed: [0.0; 4],
        rushed_spell_ladder_sim: [0.0; 4],
        rushed_spell_duration_diff: None,
    }
}

/// The skills the engine actually holds per runner, as numeric base ids.
fn engine_skills(params: &WasmRaceSimParams) -> Vec<Vec<i64>> {
    params
        .runners
        .iter()
        .map(|runner| {
            runner
                .skills
                .iter()
                .filter_map(|skill| skill_base_id(&skill.skill_id))
                .collect()
        })
        .collect()
}

fn print_scores(label: &str, scores: &Scores) {
    println!(
        "  {label:<7} finish MAE {:.3}s (bias {:+.3}s)  winner {:>3.0}%  spearman {:.3}  spurt MAE {:>6.1}m  skill err {:.3}  trajectory MAE {:>5.1}m  speed MAE {:.3} (bias {:+.3}) m/s  hp MAE {:>5.1} (bias {:+6.1})  rushed agree {:.3}  lane MAE {:.2}m  blocked diff {:+.3}",
        scores.finish_time_mae,
        scores.finish_time_bias,
        scores.winner_hit_rate * 100.0,
        scores.order_spearman,
        scores.spurt_start_mae,
        scores.skill_activation_error,
        scores.trajectory_mae,
        scores.speed_mae,
        scores.speed_bias,
        scores.hp_mae,
        scores.hp_bias,
        scores.rushed_agreement,
        scores.lane_mae,
        scores.blocked_share_diff.unwrap_or(f64::NAN),
    );
}

/// One fixture's rushed spells, recorded against simulated.
fn print_rushed_spells(comparison: &SpellComparison) {
    print_spell_tally("recorded", &comparison.observed, 1.0);
    print_spell_tally(
        "simulated",
        &comparison.sim,
        comparison.rounds.max(1) as f64,
    );
}

/// One side's spell tally: how many spells, how long they ran (span, bracket
/// upper bound, ladder rung) and the ladder histogram. Counts and per-rung
/// figures are divided by `divisor`, which is the round count on the simulated
/// side and 1 on the recorded one.
fn print_spell_tally(label: &str, tally: &SpellTally, divisor: f64) {
    let shares = tally.shares();
    println!(
        "          spells {label:<9} {:>6.2}  span {:>5.2}s  bracket <={:>5.2}s  ladder {:>5.2}s   3s {:>5.2} ({:>3.0}%)  6s {:>5.2} ({:>3.0}%)  9s {:>5.2} ({:>3.0}%)  12s {:>5.2} ({:>3.0}%)",
        tally.count as f64 / divisor,
        tally.mean_lower(),
        tally.mean_upper(),
        tally.mean_ladder(),
        tally.ladder[0] as f64 / divisor,
        shares[0] * 100.0,
        tally.ladder[1] as f64 / divisor,
        shares[1] * 100.0,
        tally.ladder[2] as f64 / divisor,
        shares[2] * 100.0,
        tally.ladder[3] as f64 / divisor,
        shares[3] * 100.0,
    );
}

/// The run's pooled rushed picture: spell durations on both sides, and the
/// recorded `temptationMode` split against the simulated one. The recorded
/// split is the enum the scoring flattens away; the engine tags a rushed runner
/// by its style (`replay.rs`: front runner / runaway 3, pace chaser 2, else 1),
/// so the two splits together say whether the rushed strategy override sends
/// runners to the styles the game's recording shows.
fn print_rushed_summary(
    pooled: &SpellComparison,
    observed_modes: [usize; 4],
    sim_modes: [usize; 4],
    fixtures: usize,
) {
    println!("rushed spell durations, free mode, pooled over {fixtures} fixtures");
    print_rushed_spells(pooled);
    println!("temptation mode samples, pooled over {fixtures} fixtures");
    print_mode_counts("recorded", observed_modes);
    print_mode_counts("simulated", sim_modes);
}

fn print_mode_counts(label: &str, counts: [usize; 4]) {
    let total = counts.iter().sum::<usize>().max(1) as f64;
    let mut line = format!(
        "          modes {label:<9} {:>8} rushed samples ",
        counts.iter().sum::<usize>()
    );
    for (&(value, name), count) in TEMPTATION_MODES.iter().zip(counts) {
        line.push_str(&format!(
            " {value} {name} {count:>7} ({:>4.1}%)",
            count as f64 / total * 100.0
        ));
    }
    println!("{line}");
}

/// Per-runner breakdown of a run, so a bad aggregate points at a runner.
/// Speed and HP biases are (simulated - recorded) per race phase.
fn print_runner_report(fixture: &Fixture, replays: &[RaceReplay]) {
    let observed = &fixture.observed;
    let rounds = replays.len() as f64;
    let means = mean_sim_samples(fixture, replays);
    println!(
        "          {:<18} {:>5} {:>8} {:>8} {:>9}  {:^24}  {:^24}  {:>8}",
        "runner",
        "style",
        "obs fin",
        "fin dev",
        "spurt dev",
        "speed bias early/mid/late",
        "hp bias early/mid/late",
        "traj MAE"
    );
    println!(
        "          {:<18} {:>5} {:>8} {:>8} {:>9}  {:^24}  {:^24}  {:>8}  {:>17}  {:>23}",
        "", "", "", "", "", "", "", "", "blocked mid/late", "lane MAE early/mid/late"
    );
    for (gate, result) in observed.results.iter().enumerate() {
        let mean_finish = replays
            .iter()
            .map(|r| f64::from(r.results[gate].finish_time_raw))
            .sum::<f64>()
            / rounds;
        let mean_spurt = replays
            .iter()
            .map(|r| f64::from(r.results[gate].last_spurt_start_distance))
            .sum::<f64>()
            / rounds;
        let by_phase: Vec<FrameErrors> = Phase::ALL
            .iter()
            .map(|&phase| frame_errors(fixture, &means, Some(gate), Some(phase)))
            .collect();
        let whole = frame_errors(fixture, &means, Some(gate), None);
        println!(
            "          {:<18} {:>5} {:>8.3} {:>+8.3} {:>+9.1}  {:>+7.3} {:>+7.3} {:>+7.3}  {:>+7.1} {:>+7.1} {:>+7.1}  {:>7.1}m  obs {:.2}/{:.2} sim {:.2}/{:.2}  lane MAE {:.2}/{:.2}/{:.2}m",
            fixture.horses[gate]
                .name
                .chars()
                .take(18)
                .collect::<String>(),
            fixture.params.runners[gate].strategy,
            result.finish_time_raw,
            mean_finish - result.finish_time_raw,
            mean_spurt - result.last_spurt_start_distance,
            by_phase[0].mean(by_phase[0].speed_signed),
            by_phase[1].mean(by_phase[1].speed_signed),
            by_phase[2].mean(by_phase[2].speed_signed),
            by_phase[0].mean(by_phase[0].hp_signed),
            by_phase[1].mean(by_phase[1].hp_signed),
            by_phase[2].mean(by_phase[2].hp_signed),
            whole.mean(whole.distance_abs),
            by_phase[1].mean(by_phase[1].blocked_observed),
            by_phase[2].mean(by_phase[2].blocked_observed),
            by_phase[1].mean(by_phase[1].blocked_sim),
            by_phase[2].mean(by_phase[2].blocked_sim),
            by_phase[0].lane_mae(),
            by_phase[1].lane_mae(),
            by_phase[2].lane_mae(),
        );
    }
    let field: Vec<FrameErrors> = Phase::ALL
        .iter()
        .map(|&phase| frame_errors(fixture, &means, None, Some(phase)))
        .collect();
    println!(
        "          {:<18} {:>5} {:>8} {:>8} {:>9}  {:>+7.3} {:>+7.3} {:>+7.3}  {:>+7.1} {:>+7.1} {:>+7.1}",
        "field",
        "",
        "",
        "",
        "",
        field[0].mean(field[0].speed_signed),
        field[1].mean(field[1].speed_signed),
        field[2].mean(field[2].speed_signed),
        field[0].mean(field[0].hp_signed),
        field[1].mean(field[1].hp_signed),
        field[2].mean(field[2].hp_signed),
    );
}

/// One runner's recorded frames against the mean simulated state, line by
/// line, for `ACCURACY_TRACE=<gate>`.
fn print_trace(fixture: &Fixture, result: &RaceSimResult, gate: usize) {
    let replays = &result.replays;
    let means = mean_sim_samples(fixture, replays);
    let course_width = fixture.params.course.course_width;
    // The engine's own effect windows for this gate in the first round, so a
    // speed or acceleration excess can be matched to the skill behind it.
    if let Some(trace) = result
        .collected
        .rounds
        .first()
        .and_then(|round| round.focus.iter().find(|t| t.runner_id.0 as usize == gate))
    {
        let mut logs: Vec<_> = trace
            .skill_activations
            .values()
            .flatten()
            .map(|log| (log.start, log.end, log.skill_id.clone(), log.effect_type))
            .collect();
        logs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        println!("          engine effect windows (round 0): start-end m  skill  effect type");
        for (start, end, skill_id, effect_type) in logs {
            println!("          {start:7.1}-{end:7.1}  {skill_id:>8}  type {effect_type}");
        }
    }
    println!(
        "          trace gate {gate} {}: time | distance obs sim dev | speed obs sim dev | hp obs sim dev | rushed obs sim | lane m obs sim",
        fixture.horses[gate].name
    );
    for (frame_index, frame) in fixture.observed.frames.iter().enumerate() {
        let Some(sim) = means.get(&(frame_index, gate)) else {
            continue;
        };
        let obs = observed_sample(frame, gate, course_width);
        println!(
            "          {:6.2} | {:7.1} {:7.1} {:+6.1} | {:6.2} {:6.2} {:+6.2} | {:6.0} {:6.0} {:+6.0} | {:.0} {:.2} | {:5.2} {:5.2}",
            frame.time,
            obs.distance,
            sim.distance,
            sim.distance - obs.distance,
            obs.speed,
            sim.speed,
            sim.speed - obs.speed,
            obs.hp,
            sim.hp,
            sim.hp - obs.hp,
            obs.rushed,
            sim.rushed,
            obs.lane.unwrap_or(f64::NAN),
            sim.lane.unwrap_or(f64::NAN),
        );
    }
}

/// Every runner's recorded and mean simulated position and lane per frame,
/// for `ACCURACY_TRACE=field`: the field geometry the lane rules react to.
fn print_field_trace(fixture: &Fixture, result: &RaceSimResult) {
    let means = mean_sim_samples(fixture, &result.replays);
    let course_width = fixture.params.course.course_width;
    let gates = fixture.horses.len();
    println!(
        "          field trace: time | per gate: distance obs/sim  lane m obs/sim  blocked obs/sim"
    );
    for (frame_index, frame) in fixture.observed.frames.iter().enumerate() {
        let mut line = format!("          {:6.2} |", frame.time);
        for gate in 0..gates {
            let Some(sim) = means.get(&(frame_index, gate)) else {
                continue;
            };
            let obs = observed_sample(frame, gate, course_width);
            line.push_str(&format!(
                " {gate}:{:6.1}/{:6.1} {:4.2}/{:4.2} b{:.0}/{:.2}|",
                obs.distance,
                sim.distance,
                obs.lane.unwrap_or(f64::NAN),
                sim.lane.unwrap_or(f64::NAN),
                obs.blocked,
                sim.blocked,
            ));
        }
        println!("{line}");
    }
}

/// Baseline file shape: fixture file -> mode -> scores.
type Baseline = BTreeMap<String, BTreeMap<String, Scores>>;

fn baseline_path() -> PathBuf {
    fixture_dir().join("baseline.json")
}

fn load_baseline() -> Baseline {
    fs::read_to_string(baseline_path())
        .ok()
        .map(|text| serde_json::from_str(&text).expect("parse baseline"))
        .unwrap_or_default()
}

fn check_against_baseline(
    baseline: &Baseline,
    file: &str,
    mode: &str,
    scores: &Scores,
) -> Vec<String> {
    let Some(previous) = baseline.get(file).and_then(|modes| modes.get(mode)) else {
        return vec![format!("{file} [{mode}]: no baseline entry")];
    };
    let mut failures = Vec::new();
    if scores.finish_time_mae > previous.finish_time_mae + FINISH_TIME_TOLERANCE {
        failures.push(format!(
            "{file} [{mode}]: finish MAE regressed {:.3}s -> {:.3}s",
            previous.finish_time_mae, scores.finish_time_mae
        ));
    }
    if scores.trajectory_mae > previous.trajectory_mae + TRAJECTORY_TOLERANCE {
        failures.push(format!(
            "{file} [{mode}]: trajectory MAE regressed {:.2}m -> {:.2}m",
            previous.trajectory_mae, scores.trajectory_mae
        ));
    }
    if scores.speed_mae > previous.speed_mae + SPEED_TOLERANCE {
        failures.push(format!(
            "{file} [{mode}]: speed MAE regressed {:.3} -> {:.3} m/s",
            previous.speed_mae, scores.speed_mae
        ));
    }
    if scores.lane_mae > previous.lane_mae + LANE_TOLERANCE {
        failures.push(format!(
            "{file} [{mode}]: lane MAE regressed {:.2}m -> {:.2}m",
            previous.lane_mae, scores.lane_mae
        ));
    }
    // Blocked share is gated on drift of its magnitude, not its level: the
    // engine's level is what the fixtures measure, but it must not wander.
    // A baseline written before the metric existed carries no value to drift
    // from, so the check waits for one.
    if let (Some(before), Some(now)) = (previous.blocked_share_diff, scores.blocked_share_diff) {
        if now.abs() > before.abs() + BLOCKED_SHARE_TOLERANCE {
            failures.push(format!(
                "{file} [{mode}]: blocked share drifted {before:+.3} -> {now:+.3}"
            ));
        }
    }
    if scores.hp_mae > previous.hp_mae + HP_TOLERANCE {
        failures.push(format!(
            "{file} [{mode}]: HP MAE regressed {:.1} -> {:.1}",
            previous.hp_mae, scores.hp_mae
        ));
    }
    failures
}

#[test]
#[ignore = "local accuracy harness; run with `cargo test -p honse-sim-wasm --test capture_accuracy -- --ignored`"]
fn captured_races_score_no_worse_than_baseline() {
    let fixtures = load_fixtures();
    assert!(!fixtures.is_empty(), "no capture fixtures found");
    let nsamples = samples();
    let update = std::env::var_os("UPDATE_ACCURACY_BASELINE").is_some();
    let baseline = load_baseline();
    let mut next_baseline: Baseline = BTreeMap::new();
    let mut failures = Vec::new();

    let mut pooled_spells = SpellComparison::default();
    let mut observed_modes = [0usize; 4];
    let mut sim_modes = [0usize; 4];

    println!("capture accuracy over {nsamples} seeds per run");
    for fixture in &fixtures {
        let dropped: usize = fixture
            .horses
            .iter()
            .map(|h| h.dropped_skill_ids.len())
            .sum();
        println!(
            "{} (course {}, {} runners, {} skills dropped by the exporter)",
            fixture.source.file,
            fixture.source.course_id,
            fixture.horses.len(),
            dropped
        );
        assert_eq!(
            fixture.params.runners.len(),
            fixture.observed.results.len(),
            "{}: runner count differs from replay",
            fixture.source.file
        );
        assert!(
            fixture
                .params
                .runners
                .iter()
                .enumerate()
                .all(|(index, runner)| runner.gate == Some(index as i64)),
            "{}: every runner must be pinned to its gate",
            fixture.source.file
        );

        let started = std::time::Instant::now();
        let free_replays = run(&fixture.params, nsamples);
        let simulated = started.elapsed();
        let mut free = score(fixture, &free_replays, &engine_skills(&fixture.params));
        assert!(
            free.lane_mae.is_finite(),
            "{}: no lane data in the recording; regenerate the fixture with `pnpm run race:fixture`",
            fixture.source.file
        );
        println!(
            "  timing  simulate {:.2}s  score {:.2}s",
            simulated.as_secs_f64(),
            (started.elapsed() - simulated).as_secs_f64()
        );
        let spells = rushed_spells(fixture, &free_replays);
        apply_spell_scores(&mut free, &spells);
        pooled_spells.merge(&spells);
        for (slot, added) in observed_modes
            .iter_mut()
            .zip(observed_mode_counts(&fixture.observed))
        {
            *slot += added;
        }
        for (slot, added) in sim_modes
            .iter_mut()
            .zip(simulated_mode_counts(&free_replays))
        {
            *slot += added;
        }
        print_scores("free", &free);
        print_rushed_spells(&spells);

        let mut pinned_params = fixture.params.clone();
        pin_outcomes(&mut pinned_params, &fixture.observed, fixture);
        let started = std::time::Instant::now();
        let pinned_result = run_full(&pinned_params, nsamples);
        let pinned_replays = pinned_result.replays.clone();
        let simulated = started.elapsed();
        let pinned = score(fixture, &pinned_replays, &engine_skills(&pinned_params));
        print_scores("pinned", &pinned);
        print_runner_report(fixture, &pinned_replays);
        match std::env::var("ACCURACY_TRACE").ok().as_deref() {
            Some("field") => print_field_trace(fixture, &pinned_result),
            Some(gate) => {
                if let Some(gate) = gate
                    .parse::<usize>()
                    .ok()
                    .filter(|&g| g < fixture.horses.len())
                {
                    print_trace(fixture, &pinned_result, gate);
                }
            }
            None => {}
        }
        // The pinning seams must hold or the pinned scores mean nothing.
        assert!(
            pinned.skill_activation_error < 0.05,
            "{}: pinned run fired the wrong skills (error {:.3})",
            fixture.source.file,
            pinned.skill_activation_error
        );
        println!(
            "  timing  simulate {:.2}s  score+report {:.2}s",
            simulated.as_secs_f64(),
            (started.elapsed() - simulated).as_secs_f64()
        );

        for (mode, scores) in [("free", free), ("pinned", pinned)] {
            failures.extend(check_against_baseline(
                &baseline,
                &fixture.source.file,
                mode,
                &scores,
            ));
            next_baseline
                .entry(fixture.source.file.clone())
                .or_default()
                .insert(mode.to_owned(), scores);
        }
    }

    print_rushed_summary(&pooled_spells, observed_modes, sim_modes, fixtures.len());

    if update {
        let text = serde_json::to_string_pretty(&next_baseline).expect("serialize baseline");
        fs::write(baseline_path(), text + "\n").expect("write baseline");
        println!("baseline updated at {}", baseline_path().display());
        return;
    }
    assert!(
        failures.is_empty(),
        "accuracy regressed:\n{}",
        failures.join("\n")
    );
}

// ---------- unit tests for the metrics above ----------

/// A recording frame with nothing in it but the rushed column.
fn rushed_frame(time: f64, rushed: Vec<i64>) -> ObservedFrame {
    let gates = rushed.len();
    ObservedFrame {
        time,
        distance: vec![0.0; gates],
        speed: vec![0.0; gates],
        hp: vec![0.0; gates],
        rushed,
        blocker: vec![-1; gates],
        lane: vec![0.0; gates],
    }
}

/// A recording of `modes[gate][frame]` temptation modes, sampled every `dt`.
fn recording(dt: f64, modes: &[Vec<i64>]) -> Observed {
    let frames = (0..modes[0].len())
        .map(|index| {
            rushed_frame(
                index as f64 * dt,
                modes.iter().map(|gate| gate[index]).collect(),
            )
        })
        .collect();
    Observed {
        frames,
        results: (0..modes.len())
            .map(|gate| ObservedResult {
                finish_order: gate as i32,
                finish_time_raw: 60.0,
                last_spurt_start_distance: 0.0,
            })
            .collect(),
        events: Vec::new(),
    }
}

#[test]
fn ladder_rung_takes_the_shortest_rung_the_recording_allows() {
    // A single rushed sample bounds the spell from below at 0 s, and the
    // shortest spell the doc allows is one 3 s interval.
    assert_eq!(ladder_rung(0.0), 0);
    // Three samples 1.066 s apart span 2.13 s: still a 3 s spell.
    assert_eq!(ladder_rung(2.132), 0);
    // A span at a rung stays on it, at tick resolution too (2.933 = 44 ticks).
    assert_eq!(ladder_rung(3.0), 0);
    assert_eq!(ladder_rung(2.9333), 0);
    // Past a rung, the rung is ruled out: the spell survived that roll.
    assert_eq!(ladder_rung(3.2), 1);
    assert_eq!(ladder_rung(6.4), 2);
    assert_eq!(ladder_rung(9.6), 3);
    // The doc caps a spell at 12 s, so longer spans clamp to the top rung.
    assert_eq!(ladder_rung(12.0), 3);
    assert_eq!(ladder_rung(30.0), 3);
}

#[test]
fn recorded_spells_are_bracketed_by_the_sampling_gaps() {
    // The game's mid-race sampling: one frame every ~1.066 s. Gate 0 is rushed
    // in three consecutive frames, gate 1 in one frame only.
    let dt = 1.066;
    let observed = recording(
        dt,
        &[vec![0, 0, 3, 3, 3, 0, 0, 0], vec![0, 0, 0, 0, 0, 0, 4, 0]],
    );
    let tally = observed_rushed_spells(&observed);
    assert_eq!(tally.count, 2);
    // Spans are 2 * dt and 0; both bracket to a 3 s spell.
    assert!((tally.lower_sum - 2.0 * dt).abs() < 1e-9, "{tally:?}");
    // Upper bounds add the gap before the first and after the last sample.
    assert!(
        (tally.upper_sum - (2.0 * dt + 4.0 * dt)).abs() < 1e-9,
        "{tally:?}"
    );
    assert_eq!(tally.ladder, [2, 0, 0, 0]);
    assert!((tally.mean_ladder() - 3.0).abs() < 1e-9);
}

#[test]
fn a_spell_running_into_the_last_sample_still_counts() {
    let observed = recording(1.066, &[vec![0, 0, 1, 1]]);
    let tally = observed_rushed_spells(&observed);
    assert_eq!(tally.count, 1);
    assert_eq!(tally.ladder, [1, 0, 0, 0]);
}

#[test]
fn tick_resolution_spells_land_on_their_ladder_rung() {
    // The engine ticks at 1/15 s: a 3 s spell shows up in 45 frames spanning
    // 44 ticks, a 12 s one in 181 frames spanning 180 ticks. Both must land on
    // their own rung rather than one below.
    for (ticks, rung) in [(45usize, 0usize), (90, 1), (135, 2), (181, 3)] {
        let times: Vec<f64> = (0..400).map(|i| f64::from(i) * TICK_SECONDS).collect();
        let mut rushed = vec![false; 400];
        for flag in rushed.iter_mut().skip(10).take(ticks) {
            *flag = true;
        }
        let mut tally = SpellTally::default();
        tally.add_series(&times, &rushed);
        assert_eq!(tally.count, 1);
        assert_eq!(tally.ladder[rung], 1, "{ticks} ticks: {tally:?}");
    }
}

#[test]
fn the_duration_metric_sees_what_rushed_agreement_cannot() {
    // Rushed agreement compares a seed-averaged rate against a 0/1 flag over a
    // 0.51% base rate: an engine that always runs a spell to the 12 s cap where
    // the recording shows a 3 s one still scores ~0.99, because the frames it
    // gets wrong are a rounding error in the pool.
    let mut errors = FrameErrors::default();
    // Frames of a 3 s spell against frames of a 12 s one, at a 1/15 s tick.
    let recorded_frames = 45;
    let simulated_frames = 181;
    for frame in 0..10_000 {
        let observed_rushed = f64::from(u8::from(frame < recorded_frames));
        let sim_rushed = f64::from(u8::from(frame < simulated_frames));
        errors.add(
            FrameSample {
                rushed: sim_rushed,
                ..FrameSample::default()
            },
            FrameSample {
                rushed: observed_rushed,
                ..FrameSample::default()
            },
        );
    }
    let agreement = errors.mean(errors.rushed_agreement);
    assert!(agreement > 0.98, "agreement {agreement:.4}");

    // The duration metric puts the same pair three rungs apart.
    let times: Vec<f64> = (0..400).map(|i| f64::from(i) * TICK_SECONDS).collect();
    let mut recorded = SpellTally::default();
    let mut simulated = SpellTally::default();
    let mut flags = vec![false; 400];
    flags[..45].fill(true);
    recorded.add_series(&times, &flags);
    let mut flags = vec![false; 400];
    flags[..181].fill(true);
    simulated.add_series(&times, &flags);
    assert_eq!(recorded.ladder, [1, 0, 0, 0]);
    assert_eq!(simulated.ladder, [0, 0, 0, 1]);
    assert!((simulated.mean_ladder() - recorded.mean_ladder() - 9.0).abs() < 1e-9);
}

#[test]
fn mode_counts_keep_the_temptation_enum() {
    // The scoring flattens the column to `!= 0`; the distribution keeps the
    // enum, including mode 4 (BOOST), which the engine's replay never writes.
    let observed = recording(1.066, &[vec![0, 3, 3, 1], vec![0, 0, 4, 2]]);
    assert_eq!(observed_mode_counts(&observed), [1, 1, 2, 1]);
}

#[test]
fn blocked_share_difference_is_signed_and_pooled() {
    let mut errors = FrameErrors::default();
    let blocked = |value: f64| FrameSample {
        blocked: value,
        ..FrameSample::default()
    };
    // Four samples: the engine blocks in half of them, the recording in one.
    errors.add(blocked(1.0), blocked(1.0));
    errors.add(blocked(1.0), blocked(0.0));
    errors.add(blocked(0.0), blocked(0.0));
    errors.add(blocked(0.0), blocked(0.0));
    assert!((errors.blocked_share_diff() - 0.25).abs() < 1e-12);
}
