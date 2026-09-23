//! Field-agnostic race-orchestration **support** shared by both engines
//! (ADR-0005).
//!
//! The per-frame field snapshot, the per-runner field view, and the small
//! producer leaf-helpers (side-block / overtake proximity reads, approximate
//! condition reads) are pure mechanics over runner positions — they never ask
//! which paradigm is running. Both the contested module (contested) and
//! the vacuum module (synthetic) reuse them so the snapshot machinery cannot
//! drift between engines; each engine differs only in how it *produces*
//! [`FieldInputs`](crate::runner::physics::FieldInputs) from this support.

use std::collections::HashMap;

use crate::pacing::{select_pacer, PacerBranch};
use crate::runner::physics::{FrontBlock, RunnerSnapshot};
use crate::runner::skills::FieldView;
use crate::runner::Runner;
use crate::shared_kernel::ids::RunnerId;
use crate::shared_kernel::language::{strategy_matches, Strategy};
use crate::shared_kernel::rng::Prng;
use crate::skills::condition::dynamic::{
    ActiveRunner, ConditionTimers, RunnerSnapshot as DynRunnerSnapshot, ORDER_RATE_BANDS,
};
use crate::skills::effect::SkillTarget;

/// A read-only snapshot of the whole field, frozen at the start of a frame.
pub struct FieldSnapshot {
    /// Active (unfinished) runners' frozen per-frame state.
    pub entries: Vec<SnapEntry>,
    /// Current finishing order (1-based), with forced-rank overrides applied.
    pub order: HashMap<RunnerId, i64>,
    /// Previous-frame finishing order.
    pub previous_order: HashMap<RunnerId, i64>,
    /// The selected pacer, if any.
    pub pacer: Option<RunnerId>,
    /// The pacer's current position.
    pub pacer_position: Option<f64>,
    /// The pacer's own position-keep strategy.
    pub pacer_strategy: Option<Strategy>,
    /// Position of the second-furthest-forward runner.
    pub second_place_position: Option<f64>,
    /// Position of the furthest-forward runner.
    pub leader_position: Option<f64>,
    /// Number of active runners.
    pub num_active: i64,
    /// Total field size (finished + active). Used as `num_umas` for order
    /// conditions so thresholds do not shrink as runners cross the line.
    pub num_total: i64,
    /// Per-runner condition timers and latches as of this frame, filled by
    /// [`update_condition_timers`] (empty until it runs).
    pub condition_timers: HashMap<RunnerId, ConditionTimers>,
}

/// One active runner's frozen per-frame state.
#[derive(Clone, Copy)]
pub struct SnapEntry {
    /// Runner id.
    pub id: RunnerId,
    /// Longitudinal position.
    pub position: f64,
    /// Lateral lane offset.
    pub current_lane: f64,
    /// Current speed.
    pub current_speed: f64,
    /// Target speed.
    pub target_speed: f64,
    /// Whether a runner blocked this one in front last tick.
    pub is_front_blocked: bool,
    /// Immutable running style.
    pub strategy: Strategy,
    /// Starting gate.
    pub gate: i64,
    /// Whether the runner is rushed.
    pub is_rushed: bool,
    /// Whether the runner is dueling.
    pub is_dueling: bool,
    /// Bitmask of positive self-applied effect types activated so far.
    pub activated_advantage_effect_types: u64,
}

/// The aggregate's running pacer + finishing-order state, threaded across frames.
#[derive(Default)]
pub struct FieldOrderTracker {
    /// Current pacer id.
    pub pacer: Option<RunnerId>,
    /// Current pacer position (for the observation view).
    pub pacer_position: Option<f64>,
    /// The runner the front-less election returned first, which is the opening
    /// frame's. Recorded so [`select_pacer`] can hold the role for her first
    /// [`PACEMAKER_HOLD_CHECKS`](crate::pacing::PACEMAKER_HOLD_CHECKS) checks;
    /// the hold expires on her own check tally, so this is set once per round and
    /// never cleared. `None` in a fronted field, which never runs that election.
    pub opening_pacemaker: Option<RunnerId>,
    /// Current finishing order.
    pub runner_order: HashMap<RunnerId, i64>,
    /// Previous-tick finishing order.
    pub previous_runner_order: HashMap<RunnerId, i64>,
    /// Per-runner condition timers and latches, carried across frames.
    pub condition_timers: HashMap<RunnerId, ConditionTimers>,
}

impl FieldOrderTracker {
    /// A fresh tracker (no pacer, empty order maps).
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset to the pre-round state.
    pub fn reset(&mut self) {
        self.pacer = None;
        self.pacer_position = None;
        self.opening_pacemaker = None;
        self.runner_order.clear();
        self.previous_runner_order.clear();
        self.condition_timers.clear();
    }
}

/// Build the immutable field snapshot for this frame: resolve the pacer, then
/// freeze every active runner's state and refresh the order maps in `tracker`.
///
/// The field is read, never written. v0.13.0 rewrote the front-less pacemaker's
/// `position_keep_strategy` to Front Runner here, once and for all, so that she
/// kept position as a front runner (speed up 1.04x, overtake 1.05x, Pace Up Ex
/// 2.0x) and answered the exact-strategy pass of [`select_pacer`] for the rest
/// of the race. The recordings refute both halves: over 21 front-less races the
/// leader of the most forward style runs at the plain normal target (median
/// 0.999 / 0.996 of base target speed, max 1.004, no frame anywhere above
/// 1.30), and the role changes hands. So the snapshot only reports her, and
/// `pacer_strategy` is her own style -- which is what `should_pace_up_ex`
/// reads, so her style-mates no longer see a synthetic Front Runner ahead of
/// them either. The exemption that keeps her out of pace down is in
/// [`enter_from_none_pacer`](crate::position_keep).
///
/// The one piece of pacer history it keeps is `tracker.opening_pacemaker`: who
/// the opening frame elected, which [`select_pacer`] holds the role for over her
/// first [`PACEMAKER_HOLD_CHECKS`](crate::pacing::PACEMAKER_HOLD_CHECKS) checks.
pub fn build_field_snapshot(
    runners: &[Runner],
    finished_runners: &[RunnerId],
    tracker: &mut FieldOrderTracker,
) -> FieldSnapshot {
    let choice = select_pacer(runners, tracker.pacer, tracker.opening_pacemaker);
    let mut pacer_strategy = None;
    tracker.pacer = choice.map(|c| c.runner_id);
    tracker.pacer_position = None;
    if let Some(choice) = choice {
        // The first runner the front-less election returns is the one its hold
        // protects. `select_pacer` says which pass answered, so this reads no
        // meaning into the frame number or into the elected runner's style; a
        // fronted field never reports that pass, so it records nothing.
        if choice.branch == PacerBranch::FrontLessElection && tracker.opening_pacemaker.is_none() {
            tracker.opening_pacemaker = Some(choice.runner_id);
        }
        if let Some(runner) = runners.iter().find(|r| r.id == choice.runner_id) {
            tracker.pacer_position = Some(runner.position);
            pacer_strategy = Some(runner.position_keep_strategy);
        }
    }

    let entries: Vec<SnapEntry> = runners
        .iter()
        .filter(|r| !finished_runners.contains(&r.id))
        .map(|r| SnapEntry {
            id: r.id,
            position: r.position,
            current_lane: r.current_lane,
            current_speed: r.current_speed,
            target_speed: r.target_speed,
            is_front_blocked: r.front_blocker.is_some(),
            strategy: r.strategy,
            gate: r.gate,
            is_rushed: r.is_rushed,
            is_dueling: r.is_dueling,
            activated_advantage_effect_types: r.activated_advantage_effect_types,
        })
        .collect();

    // Order by position descending.
    let mut sorted: Vec<&SnapEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        b.position
            .partial_cmp(&a.position)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let previous_order = std::mem::take(&mut tracker.runner_order);
    let mut order: HashMap<RunnerId, i64> = HashMap::new();
    // Finished runners hold the top places (in finish order); runners still on
    // course are ranked behind them. Without this, the order map only ranks
    // active runners, so a trailing runner becomes "order 1" the moment the real
    // leaders cross the line — wrongly satisfying in-the-lead skill gates (e.g.
    // an `order==1` unique) at the very end of the race.
    let finished_count = finished_runners.len() as i64;
    for (place, id) in finished_runners.iter().enumerate() {
        order.insert(*id, place as i64 + 1);
    }
    for (i, entry) in sorted.iter().enumerate() {
        order.insert(entry.id, finished_count + i as i64 + 1);
    }
    // Forced-rank overrides.
    for runner in runners {
        for region in &runner.forced_rank {
            if runner.position >= region.start && runner.position < region.end {
                order.insert(runner.id, region.rank);
                break;
            }
        }
    }
    tracker.runner_order = order.clone();
    tracker.previous_runner_order = previous_order.clone();

    let leader_position = sorted.first().map(|e| e.position);
    let second_place_position = sorted.get(1).map(|e| e.position);

    let num_active = entries.len() as i64;
    FieldSnapshot {
        num_active,
        num_total: finished_count + num_active,
        entries,
        order,
        previous_order,
        pacer: tracker.pacer,
        pacer_position: tracker.pacer_position,
        pacer_strategy,
        second_place_position,
        leader_position,
        condition_timers: HashMap::new(),
    }
}

/// Seconds to look ahead for an overtake target (GameTora: "you can catch up
/// with her within 15 seconds at the current speed").
const OVERTAKE_CATCH_SECONDS: f64 = 15.0;
/// How far ahead an overtake target can be (GameTora: "up to 20 meters").
const OVERTAKE_RANGE_METERS: f64 = 20.0;
/// `*_near_lane_time`: 2.5 m and 1 lane; `behind_near_lane_time_set1`: 5 m and
/// 2.7 lanes (GameTora).
const NEAR_LANE_METERS: f64 = 2.5;
const NEAR_LANE_LANES: f64 = 1.0;
const NEAR_LANE_SET1_METERS: f64 = 5.0;
const NEAR_LANE_SET1_LANES: f64 = 2.7;
/// Side blocking: within 3 m along the course and 1 lane across, on another
/// line (the window `blocked_side` already reads).
const SIDE_BLOCK_METERS: f64 = 3.0;
const SIDE_BLOCK_LANES: f64 = 1.0;
const SIDE_BLOCK_LANE_EPSILON: f64 = 0.00001;
/// The `*_continue` conditions ignore the first 5 s of the race (GameTora).
const ORDER_CONTINUE_GRACE_SECONDS: f64 = 5.0;

/// Whether `ahead` is an overtake target of `behind`: up to 20 m ahead, and
/// caught within 15 s at the two runners' current speeds.
fn is_overtake_target(behind: &SnapEntry, ahead: &SnapEntry) -> bool {
    let gap = ahead.position - behind.position;
    let closing = behind.current_speed - ahead.current_speed;
    gap > 0.0
        && gap <= OVERTAKE_RANGE_METERS
        && closing > 0.0
        && gap / closing <= OVERTAKE_CATCH_SECONDS
}

/// Advance every active runner's condition timers by one `dt` step and copy
/// them into the snapshot, so the conditions read durations and histories
/// instead of the race clock. Called once per frame, right after
/// [`build_field_snapshot`]; `elapsed` is each runner's own race time (the
/// `*_continue` grace period is measured on it).
pub fn update_condition_timers(
    snapshot: &mut FieldSnapshot,
    tracker: &mut FieldOrderTracker,
    runners: &[Runner],
    dt: f64,
    horse_lane: f64,
) {
    let n = snapshot.num_total.max(1) as f64;
    let thresholds: Vec<i64> = ORDER_RATE_BANDS
        .iter()
        .map(|b| (n * b).round() as i64)
        .collect();
    let mut by_order: Vec<&SnapEntry> = snapshot.entries.iter().collect();
    by_order.sort_by(|a, b| {
        let oa = snapshot.order.get(&a.id).copied().unwrap_or(i64::MAX);
        let ob = snapshot.order.get(&b.id).copied().unwrap_or(i64::MAX);
        oa.cmp(&ob)
    });
    for me in &snapshot.entries {
        let order = snapshot.order.get(&me.id).copied();
        let previous = snapshot.previous_order.get(&me.id).copied();
        let placement_changed = matches!((order, previous), (Some(o), Some(p)) if o != p);
        let moved_up = matches!((order, previous), (Some(o), Some(p)) if o < p);
        let elapsed = runners
            .iter()
            .find(|r| r.id == me.id)
            .map_or(0.0, |r| r.accumulate_time.t);

        let mut near_behind = false;
        let mut near_behind_set1 = false;
        let mut near_infront = false;
        let mut side_blocked = false;
        let mut has_target = false;
        let mut is_target = false;
        for other in snapshot.entries.iter().filter(|e| e.id != me.id) {
            let along = other.position - me.position;
            let across = (other.current_lane - me.current_lane).abs();
            if along < 0.0 && -along <= NEAR_LANE_METERS && across <= NEAR_LANE_LANES * horse_lane {
                near_behind = true;
            }
            if along < 0.0
                && -along <= NEAR_LANE_SET1_METERS
                && across <= NEAR_LANE_SET1_LANES * horse_lane
            {
                near_behind_set1 = true;
            }
            if along > 0.0 && along <= NEAR_LANE_METERS && across <= NEAR_LANE_LANES * horse_lane {
                near_infront = true;
            }
            if along.abs() <= SIDE_BLOCK_METERS
                && across <= SIDE_BLOCK_LANES * horse_lane
                && across >= SIDE_BLOCK_LANE_EPSILON
            {
                side_blocked = true;
            }
            if is_overtake_target(me, other) {
                has_target = true;
            }
            if is_overtake_target(other, me) {
                is_target = true;
            }
        }
        let behind_is_inner = order
            .and_then(|o| {
                by_order
                    .iter()
                    .find(|e| snapshot.order.get(&e.id) == Some(&(o + 1)))
            })
            .is_some_and(|behind| behind.current_lane < me.current_lane);

        let t = tracker.condition_timers.entry(me.id).or_default();
        let step = |held: bool, value: f64| if held { value + dt } else { 0.0 };
        t.near_behind = if placement_changed {
            0.0
        } else {
            step(near_behind, t.near_behind)
        };
        t.near_behind_set1 = if placement_changed {
            0.0
        } else {
            step(near_behind_set1, t.near_behind_set1)
        };
        t.near_infront = if placement_changed {
            0.0
        } else {
            step(near_infront, t.near_infront)
        };
        t.blocked_front = step(me.is_front_blocked, t.blocked_front);
        t.blocked_side = step(side_blocked, t.blocked_side);
        t.blocked_all = step(me.is_front_blocked && side_blocked, t.blocked_all);
        t.overtake_target_no_order_up = if moved_up {
            0.0
        } else {
            step(has_target, t.overtake_target_no_order_up)
        };
        t.overtaken = step(is_target, t.overtaken);
        t.has_overtake_target = has_target;
        t.behind_is_inner = behind_is_inner;
        if let Some(o) = order {
            if elapsed > ORDER_CONTINUE_GRACE_SECONDS {
                for (i, thr) in thresholds.iter().enumerate() {
                    t.in_band[i] &= o <= *thr;
                    t.out_band[i] &= o > *thr;
                }
            }
        }
        snapshot.condition_timers.insert(me.id, *t);
    }
}

/// Project the frozen snapshot into the proximity-snapshot list the step's
/// lane-movement / side-block reads consume.
pub fn proximity_snapshots(snapshot: &FieldSnapshot) -> Vec<RunnerSnapshot> {
    snapshot
        .entries
        .iter()
        .map(|e| RunnerSnapshot {
            id: e.id,
            position: e.position,
            current_lane: e.current_lane,
            current_speed: e.current_speed,
            target_speed: e.target_speed,
            is_front_blocked: e.is_front_blocked,
        })
        .collect()
}

/// Build the per-runner [`FieldView`] from the frozen snapshot.
///
/// `is_front_blocked` is the producer's answer to "does a runner block this one
/// in front this tick" (mechanics § Front Blocking) — the very value the physics
/// step's speed cap reads, so the `blocked_front*` token conditions evaluate the
/// same predicate rather than a second, looser one of their own.
pub fn build_field_view(
    self_id: RunnerId,
    snapshot: &FieldSnapshot,
    is_front_blocked: bool,
) -> FieldView {
    let other_snapshots: Vec<DynRunnerSnapshot> = snapshot
        .entries
        .iter()
        .filter(|e| e.id != self_id)
        .map(|e| DynRunnerSnapshot {
            position: e.position,
            current_lane: e.current_lane,
            current_speed: e.current_speed,
        })
        .collect();
    let active_runners: Vec<ActiveRunner> = snapshot
        .entries
        .iter()
        .map(|e| ActiveRunner {
            is_self: e.id == self_id,
            position: e.position,
            strategy: e.strategy,
            gate: e.gate,
            is_rushed: e.is_rushed,
            is_dueling: e.is_dueling,
            activated_advantage_effect_types: e.activated_advantage_effect_types,
        })
        .collect();
    FieldView {
        self_order: snapshot.order.get(&self_id).copied(),
        self_previous_order: snapshot.previous_order.get(&self_id).copied(),
        num_umas: snapshot.num_total,
        leader_position: snapshot.leader_position,
        condition_timers: snapshot.condition_timers.get(&self_id).copied(),
        is_front_blocked,
        other_snapshots,
        active_runners,
    }
}

/// Resolve the target runner ids for an external debuff emitted by `source_id`,
/// reading the frozen field snapshot. The caster is never a target, and finished
/// runners (absent from `entries`) are never hit.
///
/// Only the targets reachable by the self-activation routing path are modeled:
/// `EnemyStrategy` (the Hesitant family) and `KakariStrategy` (the Frenzied
/// family) both match opponents of the derived running style; `All`, and the
/// position-relative `AheadOfSelf`/`BehindSelf`. Other selectors (`InFov`,
/// `UmaId`, `UsedRecovery`, `AheadOfPosition`, `KakariAhead`/`KakariBehind`, ally
/// targets) are not yet routed from a live cast — manual debuff injection remains
/// their path.
pub fn resolve_debuff_targets(
    snapshot: &FieldSnapshot,
    source_id: RunnerId,
    target: SkillTarget,
    target_strategy: Option<Strategy>,
) -> Vec<RunnerId> {
    let source_pos = snapshot
        .entries
        .iter()
        .find(|e| e.id == source_id)
        .map(|e| e.position);
    snapshot
        .entries
        .iter()
        .filter(|e| e.id != source_id)
        .filter(|e| match target {
            SkillTarget::EnemyStrategy | SkillTarget::KakariStrategy => {
                target_strategy.is_some_and(|s| strategy_matches(e.strategy, s))
            }
            SkillTarget::All => true,
            SkillTarget::AheadOfSelf => source_pos.is_some_and(|p| e.position > p),
            SkillTarget::BehindSelf => source_pos.is_some_and(|p| e.position < p),
            _ => false,
        })
        .map(|e| e.id)
        .collect()
}

/// Front blocking reach in meters (mechanics § Front Blocking).
const FRONT_BLOCK_DISTANCE: f64 = 2.0;
/// Side blocking reach in meters either way (mechanics § Side Blocking).
const SIDE_BLOCK_DISTANCE: f64 = 1.05;

/// The runner blocking `runner` in front (mechanics § Front Blocking): under
/// 2 m ahead, within 0.75 horse lane when touching narrowing to 0.3 at the
/// limit. The closest one wins.
pub fn front_blocking_runner(
    runner: &Runner,
    snapshots: &[RunnerSnapshot],
    horse_lane: f64,
) -> Option<FrontBlock> {
    snapshots
        .iter()
        .filter(|snapshot| snapshot.id != runner.id)
        .filter_map(|snapshot| {
            let distance_gap = snapshot.position - runner.position;
            if distance_gap <= 0.0 || distance_gap >= FRONT_BLOCK_DISTANCE {
                return None;
            }
            let lane_reach = (1.0 - 0.6 * distance_gap / FRONT_BLOCK_DISTANCE) * 0.75 * horse_lane;
            let lane_gap = (snapshot.current_lane - runner.current_lane).abs();
            (lane_gap <= lane_reach).then_some(FrontBlock {
                id: snapshot.id,
                distance_gap,
                speed: snapshot.current_speed,
            })
        })
        .min_by(|a, b| a.distance_gap.total_cmp(&b.distance_gap))
}

/// Whether another runner sits beside `runner` (mechanics § Side Blocking):
/// within 1.05 m ahead or behind and under two horse lanes across.
pub fn has_side_blocking_runner(
    runner: &Runner,
    snapshots: &[RunnerSnapshot],
    horse_lane: f64,
) -> bool {
    snapshots.iter().any(|snapshot| {
        snapshot.id != runner.id
            && (snapshot.position - runner.position).abs() < SIDE_BLOCK_DISTANCE
            && (snapshot.current_lane - runner.current_lane).abs() < 2.0 * horse_lane
    })
}

/// Whether `runner` is overtaking another runner (pushes the target lane out).
/// A contested-field producer reads this from the live proximity snapshot.
pub fn is_overtaking_runner(
    runner: &Runner,
    snapshots: &[RunnerSnapshot],
    horse_lane: f64,
) -> bool {
    let lane_threshold = horse_lane * 2.0;
    snapshots.iter().any(|snapshot| {
        if snapshot.id == runner.id {
            return false;
        }
        let is_faster = runner.current_speed > snapshot.current_speed;
        let distance_gap = (snapshot.position - runner.position).abs();
        let lane_delta = (snapshot.current_lane - runner.current_lane).abs();
        is_faster && distance_gap <= 5.0 && lane_delta <= lane_threshold
    })
}

/// Read an approximate-condition value, falling back to its start value when
/// not yet ticked. A synthetic-field producer reads side-block / overtake from
/// these instead of a live field.
pub fn condition_value(runner: &Runner, name: &str) -> i32 {
    if let Some(value) = runner.condition_values.get(name) {
        return *value;
    }
    runner
        .conditions
        .get(name)
        .map_or(0, |condition| condition.value_on_start())
}

/// Assign one gate per runner for a round.
///
/// A runner with a fixed gate keeps it. The remaining gates are dealt to the
/// remaining runners by a Fisher-Yates shuffle over `rng`, so an all-free field
/// consumes exactly the draws it always did and a fully pinned field consumes
/// none. `gate_count` may exceed the field (the vacuum engine always deals
/// from nine); a fixed gate outside `0..gate_count` or claimed twice is a
/// caller bug and is honored as given.
pub fn assign_gates(fixed: &[Option<i64>], gate_count: usize, rng: &mut dyn Prng) -> Vec<i64> {
    let mut free_gates: Vec<i64> = (0..gate_count as i64)
        .filter(|gate| !fixed.contains(&Some(*gate)))
        .collect();
    for i in (1..free_gates.len()).rev() {
        let j = rng.uniform(i as u32 + 1) as usize;
        free_gates.swap(i, j);
    }
    let mut free_gates = free_gates.into_iter();
    fixed
        .iter()
        .map(|slot| slot.or_else(|| free_gates.next()).unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn entry(id: u32, position: f64, strategy: Strategy) -> SnapEntry {
        SnapEntry {
            id: RunnerId(id),
            position,
            current_lane: 0.0,
            current_speed: 20.0,
            target_speed: 20.0,
            is_front_blocked: false,
            strategy,
            gate: 0,
            is_rushed: false,
            is_dueling: false,
            activated_advantage_effect_types: 0,
        }
    }

    fn snapshot(entries: Vec<SnapEntry>) -> FieldSnapshot {
        FieldSnapshot {
            num_total: entries.len() as i64,
            num_active: entries.len() as i64,
            entries,
            order: HashMap::new(),
            previous_order: HashMap::new(),
            pacer: None,
            pacer_position: None,
            pacer_strategy: None,
            second_place_position: None,
            leader_position: None,
            condition_timers: HashMap::new(),
        }
    }

    fn ids(mut v: Vec<RunnerId>) -> Vec<RunnerId> {
        v.sort_by_key(|r| r.0);
        v
    }

    /// A front-less field is read, never rewritten. v0.13.0 set the elected
    /// pacemaker's `position_keep_strategy` to Front Runner right here and left
    /// it there, so she kept position as a front runner and answered the
    /// exact-strategy pass of `select_pacer` for the rest of the race. Now the
    /// snapshot publishes her real style, holds the opening election for her
    /// first `PACEMAKER_HOLD_CHECKS` checks, and then lets the role follow the
    /// leader of the most forward style frame by frame.
    #[test]
    fn a_front_less_snapshot_rewrites_nobody_and_lets_the_role_move() {
        use crate::runner::test_support::test_runner;

        let mut runners: Vec<Runner> = [
            (0_u32, Strategy::PaceChaser),
            (1, Strategy::PaceChaser),
            (2, Strategy::LateSurger),
        ]
        .into_iter()
        .map(|(id, strategy)| {
            let mut r = test_runner(id, strategy);
            r.gate = i64::from(id);
            r
        })
        .collect();
        let mut tracker = FieldOrderTracker::new();

        // Opening frame: every position is 0.0, so the rail of the most forward
        // style present takes the role (recordings: 21 of 21).
        let snap = build_field_snapshot(&runners, &[], &mut tracker);
        assert_eq!(snap.pacer, Some(RunnerId(0)));
        assert_eq!(snap.pacer_strategy, Some(Strategy::PaceChaser));

        assert_eq!(tracker.opening_pacemaker, Some(RunnerId(0)));

        // A later frame with her style-mate in front, still inside her hold:
        // the role stays with her.
        runners[1].position = 12.0;
        let snap = build_field_snapshot(&runners, &[], &mut tracker);
        assert_eq!(snap.pacer, Some(RunnerId(0)));

        // Past her hold: the role moves with the lead, and every runner still
        // owns the style she started with.
        runners[0].pos_keep_checks_run = crate::pacing::PACEMAKER_HOLD_CHECKS;
        let snap = build_field_snapshot(&runners, &[], &mut tracker);
        assert_eq!(snap.pacer, Some(RunnerId(1)));
        assert_eq!(snap.pacer_strategy, Some(Strategy::PaceChaser));
        assert!(runners
            .iter()
            .all(|r| r.position_keep_strategy == r.strategy));
    }

    /// A fronted field records no hold: the exact-strategy pass answers on every
    /// frame there, so the opening election is never held and could never be
    /// consulted if it were.
    #[test]
    fn a_fronted_snapshot_records_no_opening_hold() {
        use crate::runner::test_support::test_runner;

        let runners: Vec<Runner> = [
            (0_u32, Strategy::PaceChaser),
            (1, Strategy::FrontRunner),
            (2, Strategy::LateSurger),
        ]
        .into_iter()
        .map(|(id, strategy)| {
            let mut r = test_runner(id, strategy);
            r.gate = i64::from(id);
            r
        })
        .collect();
        let mut tracker = FieldOrderTracker::new();

        let snap = build_field_snapshot(&runners, &[], &mut tracker);
        assert_eq!(snap.pacer, Some(RunnerId(1)));
        assert_eq!(tracker.opening_pacemaker, None);
    }

    #[test]
    fn enemy_strategy_targets_matching_others_only() {
        // R0 = caster (Late Surger), R1/R2 front runners, R3 pace chaser.
        let snap = snapshot(vec![
            entry(0, 100.0, Strategy::LateSurger),
            entry(1, 120.0, Strategy::FrontRunner),
            entry(2, 80.0, Strategy::Runaway), // Runaway counts as nige (front)
            entry(3, 90.0, Strategy::PaceChaser),
        ]);
        let got = resolve_debuff_targets(
            &snap,
            RunnerId(0),
            SkillTarget::EnemyStrategy,
            Some(Strategy::FrontRunner),
        );
        assert_eq!(ids(got), vec![RunnerId(1), RunnerId(2)]);
    }

    #[test]
    fn enemy_strategy_excludes_self_even_if_same_style() {
        // A front runner casting a nige-targeting debuff hits the *other* front
        // runner, never itself.
        let snap = snapshot(vec![
            entry(0, 100.0, Strategy::FrontRunner),
            entry(1, 120.0, Strategy::FrontRunner),
            entry(2, 90.0, Strategy::PaceChaser),
        ]);
        let got = resolve_debuff_targets(
            &snap,
            RunnerId(0),
            SkillTarget::EnemyStrategy,
            Some(Strategy::FrontRunner),
        );
        assert_eq!(got, vec![RunnerId(1)]);
    }

    #[test]
    fn enemy_strategy_without_derived_strategy_hits_nobody() {
        let snap = snapshot(vec![
            entry(0, 100.0, Strategy::LateSurger),
            entry(1, 120.0, Strategy::FrontRunner),
        ]);
        let got = resolve_debuff_targets(&snap, RunnerId(0), SkillTarget::EnemyStrategy, None);
        assert!(got.is_empty());
    }

    #[test]
    fn all_and_position_relative_targets() {
        let snap = snapshot(vec![
            entry(0, 100.0, Strategy::PaceChaser),  // caster
            entry(1, 120.0, Strategy::FrontRunner), // ahead
            entry(2, 80.0, Strategy::LateSurger),   // behind
        ]);
        assert_eq!(
            ids(resolve_debuff_targets(
                &snap,
                RunnerId(0),
                SkillTarget::All,
                None
            )),
            vec![RunnerId(1), RunnerId(2)]
        );
        assert_eq!(
            resolve_debuff_targets(&snap, RunnerId(0), SkillTarget::AheadOfSelf, None),
            vec![RunnerId(1)]
        );
        assert_eq!(
            resolve_debuff_targets(&snap, RunnerId(0), SkillTarget::BehindSelf, None),
            vec![RunnerId(2)]
        );
    }

    #[test]
    fn assign_gates_keeps_fixed_gates_and_deals_the_rest() {
        use crate::shared_kernel::rng::Xoshiro256StarStar;
        let fixed = [None, Some(0), None, Some(2)];
        let mut rng = Xoshiro256StarStar::from_u64_seed(1);
        let gates = assign_gates(&fixed, 4, &mut rng);
        assert_eq!(gates[1], 0);
        assert_eq!(gates[3], 2);
        let mut free = vec![gates[0], gates[2]];
        free.sort_unstable();
        assert_eq!(free, vec![1, 3]);
    }

    #[test]
    fn assign_gates_without_fixed_gates_is_a_permutation() {
        use crate::shared_kernel::rng::Xoshiro256StarStar;
        let mut rng = Xoshiro256StarStar::from_u64_seed(7);
        let mut gates = assign_gates(&[None; 9], 9, &mut rng);
        gates.sort_unstable();
        assert_eq!(gates, (0..9).collect::<Vec<i64>>());
    }

    // --- condition timers (GameTora's skill-condition viewer, 23 Sep 2026) ---

    const DT: f64 = 1.0 / 15.0;
    const LANE: f64 = 1.5;

    /// A two-runner field: `a` at 100 m, `b` behind it by `gap` metres and
    /// `lanes` lanes, with placements `order_a` / `order_b` this frame and the
    /// given previous placements.
    fn pair(gap: f64, lanes: f64, now: (i64, i64), before: (i64, i64)) -> FieldSnapshot {
        let mut a = entry(1, 100.0, Strategy::FrontRunner);
        let mut b = entry(2, 100.0 - gap, Strategy::PaceChaser);
        b.current_lane = lanes * LANE;
        a.current_lane = 0.0;
        let mut snap = snapshot(vec![a, b]);
        snap.order.insert(RunnerId(1), now.0);
        snap.order.insert(RunnerId(2), now.1);
        snap.previous_order.insert(RunnerId(1), before.0);
        snap.previous_order.insert(RunnerId(2), before.1);
        snap
    }

    fn tick(snap: &mut FieldSnapshot, tracker: &mut FieldOrderTracker) -> ConditionTimers {
        update_condition_timers(snap, tracker, &[], DT, LANE);
        snap.condition_timers[&RunnerId(1)]
    }

    #[test]
    fn near_behind_counts_seconds_held_not_the_race_clock() {
        // 2 m behind, same lane: right behind by GameTora's 2.5 m / 1 lane.
        let mut tracker = FieldOrderTracker::new();
        let mut t = ConditionTimers::default();
        for _ in 0..30 {
            t = tick(&mut pair(2.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        }
        assert!(
            (t.near_behind - 2.0).abs() < 1e-9,
            "30 frames = 2 s, got {}",
            t.near_behind
        );
        // Out of range (3 m) for one frame: back to zero.
        t = tick(&mut pair(3.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        assert_eq!(t.near_behind, 0.0);
        // But within the set1 window (5 m, 2.7 lanes).
        assert!(t.near_behind_set1 > 2.0);
    }

    #[test]
    fn near_lane_timers_reset_when_the_runners_own_placement_changes() {
        let mut tracker = FieldOrderTracker::new();
        for _ in 0..30 {
            tick(&mut pair(2.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        }
        let t = tick(&mut pair(2.0, 0.0, (1, 2), (2, 1)), &mut tracker);
        assert_eq!(t.near_behind, 0.0);
        assert_eq!(t.near_behind_set1, 0.0);
    }

    #[test]
    fn near_behind_needs_the_same_lane() {
        let mut tracker = FieldOrderTracker::new();
        let t = tick(&mut pair(2.0, 1.5, (1, 2), (1, 2)), &mut tracker);
        assert_eq!(t.near_behind, 0.0, "1.5 lanes is outside the 1-lane window");
        assert!(t.near_behind_set1 > 0.0, "but inside set1's 2.7 lanes");
    }

    #[test]
    fn is_behind_in_reads_the_runner_directly_behind() {
        let mut tracker = FieldOrderTracker::new();
        // b behind a, on a lane closer to the fence than a.
        let mut snap = pair(4.0, 0.0, (1, 2), (1, 2));
        snap.entries[0].current_lane = 3.0;
        snap.entries[1].current_lane = 1.0;
        assert!(tick(&mut snap, &mut tracker).behind_is_inner);
        let mut snap = pair(4.0, 0.0, (1, 2), (1, 2));
        snap.entries[0].current_lane = 1.0;
        snap.entries[1].current_lane = 3.0;
        assert!(!tick(&mut snap, &mut tracker).behind_is_inner);
    }

    #[test]
    fn overtake_target_is_up_to_20_m_ahead_and_caught_within_15_s() {
        let mut tracker = FieldOrderTracker::new();
        // b 10 m behind a and 1 m/s faster: catches in 10 s, a is b's target.
        let mut snap = pair(10.0, 0.0, (1, 2), (1, 2));
        snap.entries[1].current_speed = 21.0;
        update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        assert!(snap.condition_timers[&RunnerId(2)].has_overtake_target);
        assert!(snap.condition_timers[&RunnerId(1)].overtaken > 0.0);
        // 0.5 m/s faster: 20 s to catch, no target.
        let mut snap = pair(10.0, 0.0, (1, 2), (1, 2));
        snap.entries[1].current_speed = 20.5;
        update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        assert!(!snap.condition_timers[&RunnerId(2)].has_overtake_target);
        // 25 m ahead: out of range however fast.
        let mut snap = pair(25.0, 0.0, (1, 2), (1, 2));
        snap.entries[1].current_speed = 30.0;
        update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        assert!(!snap.condition_timers[&RunnerId(2)].has_overtake_target);
    }

    #[test]
    fn overtake_target_timer_resets_on_moving_up_a_place() {
        let mut tracker = FieldOrderTracker::new();
        for _ in 0..30 {
            let mut snap = pair(10.0, 0.0, (1, 2), (1, 2));
            snap.entries[1].current_speed = 21.0;
            update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        }
        let mut snap = pair(10.0, 0.0, (2, 1), (1, 2));
        snap.entries[1].current_speed = 21.0;
        update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        assert_eq!(
            snap.condition_timers[&RunnerId(2)].overtake_target_no_order_up,
            0.0
        );
    }
}
