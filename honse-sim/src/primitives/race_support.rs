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

use crate::course::model::CourseData;
use crate::course::phase::phase_start;
use crate::pacing::{select_pacer, PacerBranch};
use crate::runner::lane::is_overtake_target;
use crate::runner::physics::{FrontBlock, RunnerSnapshot};
use crate::runner::skills::FieldView;
use crate::runner::Runner;
use crate::shared_kernel::ids::RunnerId;
use crate::shared_kernel::language::{strategy_matches, Phase, Strategy};
use crate::shared_kernel::rng::Prng;
use crate::skills::condition::blocking::is_side_blocking;
use crate::skills::condition::dynamic::{
    order_rate_band_holds, ActiveRunner, ConditionTimers, RunnerSnapshot as DynRunnerSnapshot,
    ORDER_CONTINUE_GRACE_SECONDS, ORDER_RATE_BANDS,
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
    /// Position of the furthest-back active runner.
    pub last_position: Option<f64>,
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
    /// The runner that blocked this one in front last tick, if any.
    pub front_blocker: Option<RunnerId>,
    /// Immutable running style.
    pub strategy: Strategy,
    /// Starting gate.
    pub gate: i64,
    /// Whether the runner is rushed.
    pub is_rushed: bool,
    /// Whether the runner is dueling.
    pub is_dueling: bool,
    /// Whether the runner has already had its duel this race (Showdown over).
    pub has_dueled: bool,
    /// Bitmask of positive self-applied effect types activated so far.
    pub activated_advantage_effect_types: u64,
    /// Popularity rank (1 = most popular; `0` = unknown).
    pub popularity: i64,
}

impl SnapEntry {
    /// The runner as the proximity reads (lane movement, blocking, overtake
    /// targets) see her.
    pub fn proximity_snapshot(&self) -> RunnerSnapshot {
        RunnerSnapshot {
            id: self.id,
            position: self.position,
            current_lane: self.current_lane,
            current_speed: self.current_speed,
            target_speed: self.target_speed,
            is_front_blocked: self.front_blocker.is_some(),
        }
    }
}

/// Where on the course the `change_order_up_*` counters count passes: the
/// Mid-Race `[D/6, 2D/3)`, the Late-Race from 2D/3, and the final corner from
/// its start. A pass counts where the runner is at the end of the tick.
#[derive(Debug, Clone, Copy)]
pub struct OrderUpWindows {
    /// Start of the Mid-Race.
    pub middle_start: f64,
    /// Start of the Late-Race (the Mid-Race ends here).
    pub late_start: f64,
    /// Start of the final corner; `None` on a course without corners.
    pub final_corner_start: Option<f64>,
}

impl OrderUpWindows {
    /// The windows of `course`.
    pub fn for_course(course: &CourseData) -> Self {
        Self {
            middle_start: phase_start(course.distance, Phase::MidRace),
            late_start: phase_start(course.distance, Phase::LateRace),
            final_corner_start: course.corners.last().map(|c| c.start),
        }
    }
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
    /// The course windows the `change_order_up_*` counters count in, set by
    /// the race for its course; `None` counts nothing.
    pub order_up_windows: Option<OrderUpWindows>,
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
            front_blocker: r.front_blocker,
            strategy: r.strategy,
            gate: r.gate,
            is_rushed: r.is_rushed,
            is_dueling: r.is_dueling,
            has_dueled: r.has_dueled,
            activated_advantage_effect_types: r.activated_advantage_effect_types,
            popularity: r.popularity,
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
    let last_position = sorted.last().map(|e| e.position);

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
        last_position,
        condition_timers: HashMap::new(),
    }
}

/// `*_near_lane_time`: 2.5 m and 1 lane; `behind_near_lane_time_set1`: 5 m and
/// 2.7 lanes (GameTora), both read on the placement-adjacent uma only.
const NEAR_LANE_METERS: f64 = 2.5;
const NEAR_LANE_LANES: f64 = 1.0;
const NEAR_LANE_SET1_METERS: f64 = 5.0;
const NEAR_LANE_SET1_LANES: f64 = 2.7;
/// Slack on the near-lane windows' lane edges, in metres. A pair one lane
/// apart sits on the edge only up to rounding: when its two lanes lie on
/// either side of a power of two (1 m, 2 m, 4 m, 8 m), the spacing of doubles
/// differs between them and the gap comes out a few 1e-16 m off one lane
/// (up to 2.2e-16 m past it and 8.9e-16 m inside it over 2 rounds on the
/// 117 races; half the spacing above 16 m, on the widest course's 16.875 m,
/// is 1.8e-15 m).
/// The slack is over 500,000 times that and under a millionth of the
/// recordings' lane resolution (1/10000 of the course width, 1.125 mm), so
/// it only decides which side of the edge a pair that is on it falls.
const NEAR_LANE_EDGE_TOLERANCE: f64 = 1e-9;

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
            .map_or(0.0, |r| r.accumulate_time.seconds());
        // Runners she passed this tick: ahead of her on the previous tick's
        // order, behind her on this one.
        let passed = match (order, previous) {
            (Some(o), Some(p)) => snapshot
                .entries
                .iter()
                .filter(|e| {
                    matches!(
                        (snapshot.order.get(&e.id), snapshot.previous_order.get(&e.id)),
                        (Some(&eo), Some(&ep)) if ep < p && eo > o
                    )
                })
                .count() as i64,
            _ => 0,
        };

        // The umas directly behind and directly ahead in placement.
        let adjacent = |offset: i64| {
            order.and_then(|o| {
                by_order
                    .iter()
                    .find(|e| snapshot.order.get(&e.id) == Some(&(o + offset)))
                    .copied()
            })
        };
        let (behind, ahead) = (adjacent(1), adjacent(-1));
        // The near-lane timers read only those two (mechanics §
        // behind_near_lane_time: "performed to the uma 1 place ahead/behind"),
        // not any uma in the window as GameTora's note has it. On the 117
        // recordings, 113 See Ya Later! carriers meet the condition only
        // through another uma and 2 fired, where the wit roll expects 104;
        // 200492, 49 carriers and 1 fired against 45. set1 reads the same
        // way: 6 carriers of 900051 meet its precondition only through
        // another uma, against 5.6 expected fires; one fired, 1.1 s before
        // that reading would allow.
        // The edges are GameTora's "no more than", where the mechanics doc's
        // rule line reads abs(DistanceGap) < 2.5 m and abs(LaneGap) < 1
        // HorseLane. The recordings tell the lane edge. They floor lanes to
        // 1/10000 of the course width, so a pair one lane apart reads 555 or
        // 556: 62 carriers of the near-lane skills meet the condition only if
        // an adjacent uma at that offset counts, and 53 fired, where the wit
        // roll expects 57.0 and `<` none. They cannot separate `<=` from `<`
        // with the game's offset a hair inside one lane, but either way a pair
        // at it counts.
        // Here such pairs sit 0.625 m apart up to rounding (gates and the rail
        // are multiples of it). `<=` alone keeps the ones a rounding error
        // inside and drops the ones a rounding error past, so the lane edge
        // takes NEAR_LANE_EDGE_TOLERANCE. Over 2 rounds on the 117 races, of
        // 1,998,534 pair-ticks of umas adjacent by position within 2.5 m,
        // 80,745 sit exactly one lane apart, 2,003 up to 8.9e-16 m inside it
        // and 1,446 up to 2.2e-16 m past it, each of those 1,446 with its two
        // lanes on either side of 1 m or 2 m; the slack takes them in. The
        // 2.5 m edge takes no slack and cannot be told: no adjacent pair
        // holds within 1 mm of it over two recorded frames, no engine
        // pair-tick of those rounds comes within 1e-9 m of it, and a crossing
        // is placed no better than a frame's change (552 of the 1,280 fired
        // runs entered within one of it, median 0.62 m). set1's 2.7-lane edge
        // decides no carrier on the recordings, and no engine pair-tick of
        // those rounds within 5 m comes within 1e-9 m of it.
        // `sign` is -1 behind, +1 ahead.
        let near_lane = |other: Option<&SnapEntry>, sign: f64, meters: f64, lanes: f64| {
            other.is_some_and(|other| {
                let gap = sign * (other.position - me.position);
                let across = (other.current_lane - me.current_lane).abs();
                gap > 0.0
                    && gap <= meters
                    && across <= lanes * horse_lane + NEAR_LANE_EDGE_TOLERANCE
            })
        };
        let near_behind = near_lane(behind, -1.0, NEAR_LANE_METERS, NEAR_LANE_LANES);
        let near_behind_set1 = near_lane(behind, -1.0, NEAR_LANE_SET1_METERS, NEAR_LANE_SET1_LANES);
        let near_infront = near_lane(ahead, 1.0, NEAR_LANE_METERS, NEAR_LANE_LANES);
        let behind_is_inner = behind.is_some_and(|b| b.current_lane < me.current_lane);

        let mut side_blocked = false;
        let mut has_target = false;
        let mut is_target = false;
        let seen = me.proximity_snapshot();
        for other in snapshot.entries.iter().filter(|e| e.id != me.id) {
            let along = other.position - me.position;
            let across = (other.current_lane - me.current_lane).abs();
            if is_side_blocking(along, across, horse_lane) {
                side_blocked = true;
            }
            // The overtake targets lane movement reads (mechanics § Overtake
            // Targets), vision cone included. GameTora's note ("up to 20 m
            // ahead, caught within 15 s") leaves the cone out: applied to the
            // 117 recordings it predicts 210111 for 79.6% of its carriers,
            // where 54.1% fired; the cone predicts 49.8%.
            let other_seen = other.proximity_snapshot();
            if is_overtake_target(&seen, me.front_blocker, &other_seen, horse_lane) {
                has_target = true;
            }
            if is_overtake_target(&other_seen, other.front_blocker, &seen, horse_lane) {
                is_target = true;
            }
        }

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
        let front_blocked = me.front_blocker.is_some();
        t.blocked_front = step(front_blocked, t.blocked_front);
        t.blocked_side = step(side_blocked, t.blocked_side);
        t.blocked_all = step(front_blocked && side_blocked, t.blocked_all);
        t.overtake_target_no_order_up = if moved_up {
            0.0
        } else {
            step(has_target, t.overtake_target_no_order_up)
        };
        t.overtaken = step(is_target, t.overtaken);
        t.has_overtake_target = has_target;
        t.behind_is_inner = behind_is_inner;
        if let (true, Some(w)) = (passed > 0, tracker.order_up_windows) {
            if (w.middle_start..w.late_start).contains(&me.position) {
                t.order_up_middle += passed;
            }
            if me.position >= w.late_start {
                t.order_up_end_after += passed;
            }
            if w.final_corner_start
                .is_some_and(|start| me.position >= start)
            {
                t.order_up_finalcorner_after += passed;
            }
        }
        if let Some(o) = order {
            if elapsed > ORDER_CONTINUE_GRACE_SECONDS {
                for (i, &thr) in thresholds.iter().enumerate() {
                    t.in_band[i] &= order_rate_band_holds(o, thr, true);
                    t.out_band[i] &= order_rate_band_holds(o, thr, false);
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
        .map(SnapEntry::proximity_snapshot)
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
            has_dueled: e.has_dueled,
            activated_advantage_effect_types: e.activated_advantage_effect_types,
            popularity: e.popularity,
        })
        .collect();
    FieldView {
        self_order: snapshot.order.get(&self_id).copied(),
        self_previous_order: snapshot.previous_order.get(&self_id).copied(),
        num_umas: snapshot.num_total,
        leader_position: snapshot.leader_position,
        last_position: snapshot.last_position,
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
/// under 1.05 m ahead or behind and under two horse lanes across
/// ([`is_side_blocking`], the window the `blocked_side*` conditions read too).
pub fn has_side_blocking_runner(
    runner: &Runner,
    snapshots: &[RunnerSnapshot],
    horse_lane: f64,
) -> bool {
    snapshots.iter().any(|snapshot| {
        snapshot.id != runner.id
            && is_side_blocking(
                snapshot.position - runner.position,
                snapshot.current_lane - runner.current_lane,
                horse_lane,
            )
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
    use crate::shared_kernel::math::RaceClock;
    use std::collections::HashMap;

    fn entry(id: u32, position: f64, strategy: Strategy) -> SnapEntry {
        SnapEntry {
            id: RunnerId(id),
            position,
            current_lane: 0.0,
            current_speed: 20.0,
            target_speed: 20.0,
            front_blocker: None,
            strategy,
            gate: 0,
            is_rushed: false,
            is_dueling: false,
            has_dueled: false,
            activated_advantage_effect_types: 0,
            popularity: 0,
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
            last_position: None,
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

    const DT: f64 = crate::runner::FRAME_DT;
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
            (t.near_behind - 1.998).abs() < 1e-6,
            "30 ticks = 1.998 s, got {}",
            t.near_behind
        );
        // Out of range (3 m) for one frame: back to zero.
        t = tick(&mut pair(3.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        assert_eq!(t.near_behind, 0.0);
        // But within the set1 window (5 m, 2.7 lanes).
        assert!(t.near_behind_set1 > 2.0);
    }

    /// The near-lane timers are the doc's seconds, summed a tick at a time:
    /// on the game's 0.0666 s tick a pair in the window reads 3 s on its 46th
    /// tick there, not its 45th (2.997 s). The recordings fire
    /// `behind_near_lane_time>=3` skills 46 ticks after a steep along-gap
    /// entry (89 of 107 first firings; 12 at 47, 6 at 45). On a 1/15 s tick
    /// 45 ticks read 3.0000000000000027 s.
    #[test]
    fn near_lane_timers_reach_three_seconds_on_the_46th_tick() {
        let mut tracker = FieldOrderTracker::new();
        let mut t = ConditionTimers::default();
        for _ in 0..45 {
            t = tick(&mut pair(2.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        }
        assert!(t.near_behind < 3.0, "45 ticks: {}", t.near_behind);
        assert!((t.near_behind - 2.997).abs() < 1e-6, "{}", t.near_behind);
        t = tick(&mut pair(2.0, 0.0, (1, 2), (1, 2)), &mut tracker);
        assert!(t.near_behind >= 3.0, "46 ticks: {}", t.near_behind);
        assert!((t.near_behind - 3.0636).abs() < 1e-6, "{}", t.near_behind);
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

    /// The near-lane timers read the uma directly behind / directly ahead in
    /// placement, not any uma in the window: a third-placed uma 2 m behind in
    /// the leader's lane is not "behind" the leader while the second-placed
    /// uma between them runs three lanes out.
    #[test]
    fn near_lane_timers_read_only_the_placement_adjacent_uma() {
        // 1 at 100 m; 2 one metre behind, `lanes_2` lanes out; 3 two metres
        // behind in 1's lane. Placements 1, 2, 3, unchanged.
        let field = |lanes_2: f64| {
            let one = entry(1, 100.0, Strategy::FrontRunner);
            let mut two = entry(2, 99.0, Strategy::PaceChaser);
            let three = entry(3, 98.0, Strategy::PaceChaser);
            two.current_lane = lanes_2 * LANE;
            let mut snap = snapshot(vec![one, two, three]);
            for id in 1..=3 {
                snap.order.insert(RunnerId(id), i64::from(id));
                snap.previous_order.insert(RunnerId(id), i64::from(id));
            }
            snap
        };
        let timers = |snap: &mut FieldSnapshot, id: u32| {
            update_condition_timers(snap, &mut FieldOrderTracker::new(), &[], DT, LANE);
            snap.condition_timers[&RunnerId(id)]
        };

        // 2 runs three lanes out: 3 is in 1's windows, but not adjacent.
        let t = timers(&mut field(3.0), 1);
        assert_eq!(t.near_behind, 0.0, "3 is not directly behind");
        assert_eq!(t.near_behind_set1, 0.0, "nor for set1 (5 m, 2.7 lanes)");
        let t = timers(&mut field(3.0), 3);
        assert_eq!(t.near_infront, 0.0, "1 is not directly ahead");

        // 2 comes within a lane: now the adjacent uma is in the windows.
        let t = timers(&mut field(0.5), 1);
        assert!(t.near_behind > 0.0, "2 is directly behind");
        assert!(t.near_behind_set1 > 0.0);
        let t = timers(&mut field(0.5), 3);
        assert!(t.near_infront > 0.0, "2 is directly ahead");
    }

    /// An uma exactly one lane to the side is inside the near-lane window,
    /// GameTora's "no more than 1 lane" (the mechanics doc's rule line reads
    /// `<`). On the 117 recordings 62 carriers meet the condition only if an
    /// adjacent uma one lane off counts, and 53 fired, where the wit roll
    /// expects 57.0. Off the rail by one lane, as here, the offset is exact.
    #[test]
    fn near_lane_timers_count_an_uma_exactly_one_lane_off() {
        let timers = |lanes: f64| {
            // 1 on the rail, 2 two metres behind and `lanes` lanes out.
            let mut snap = pair(2.0, lanes, (1, 2), (1, 2));
            update_condition_timers(&mut snap, &mut FieldOrderTracker::new(), &[], DT, LANE);
            (
                snap.condition_timers[&RunnerId(1)].near_behind,
                snap.condition_timers[&RunnerId(2)].near_infront,
            )
        };
        let (behind, infront) = timers(1.0);
        assert!(behind > 0.0, "2 is right behind 1, one lane out");
        assert!(infront > 0.0, "1 is right in front of 2, one lane in");
        assert_eq!(timers(1.01), (0.0, 0.0), "past one lane");
    }

    /// One lane off counts up to rounding, on the real course's 0.625 m lane:
    /// one ulp past it, and one lane outside an uma at 1.7 m, which crosses
    /// 2 m and reads 0.6250000000000002 m from her. `<=` alone dropped both;
    /// a micrometre past one lane is still past it.
    #[test]
    fn near_lane_timers_count_one_lane_off_up_to_rounding() {
        const HORSE_LANE: f64 = 11.25 / 18.0;
        let timers = |lane_1: f64, lane_2: f64| {
            // 1 in `lane_1`, 2 two metres behind in `lane_2`.
            let mut snap = pair(2.0, 0.0, (1, 2), (1, 2));
            snap.entries[0].current_lane = lane_1;
            snap.entries[1].current_lane = lane_2;
            update_condition_timers(
                &mut snap,
                &mut FieldOrderTracker::new(),
                &[],
                DT,
                HORSE_LANE,
            );
            (
                snap.condition_timers[&RunnerId(1)].near_behind > 0.0,
                snap.condition_timers[&RunnerId(2)].near_infront > 0.0,
            )
        };
        assert_eq!(HORSE_LANE, 0.625);
        assert_eq!(
            timers(0.0, HORSE_LANE.next_up()),
            (true, true),
            "one ulp past"
        );
        let outside = 1.7 + HORSE_LANE;
        assert!(
            outside - 1.7 > HORSE_LANE,
            "1.7 + 0.625 reads past one lane"
        );
        assert_eq!(timers(1.7, outside), (true, true), "one lane outside 1.7 m");
        assert_eq!(
            timers(0.0, HORSE_LANE + 1e-6),
            (false, false),
            "a micrometre past"
        );
    }

    /// The side-block timers read the mechanics doc's window (§ Side
    /// Blocking): under 1.05 m ahead or behind and under two horse lanes
    /// across, the window the physics step's side block reads. They read 3 m
    /// and one lane, and left out a rival on the same line.
    #[test]
    fn side_block_timers_read_the_documented_window() {
        use crate::runner::test_support::test_runner;

        let blocked = |gap: f64, lanes: f64, front_blocked: bool| {
            let mut snap = pair(gap, lanes, (1, 2), (1, 2));
            // Blocked in front by a runner outside the pair.
            snap.entries[0].front_blocker = front_blocked.then_some(RunnerId(9));
            let t = tick(&mut snap, &mut FieldOrderTracker::new());
            (t.blocked_side > 0.0, t.blocked_all > 0.0)
        };
        // 2 m behind, half a lane across: inside 3 m, outside 1.05 m.
        assert_eq!(blocked(2.0, 0.5, true), (false, false));
        // Half a metre behind, a lane and a half across: outside one lane,
        // inside two.
        assert_eq!(blocked(0.5, 1.5, false), (true, false));
        assert_eq!(blocked(0.5, 1.5, true), (true, true));
        // On the same line.
        assert_eq!(blocked(0.5, 0.0, false), (true, false));
        // Just outside either edge.
        assert_eq!(blocked(1.1, 0.0, false), (false, false));
        assert_eq!(blocked(0.5, 2.1, false), (false, false));

        // Wherever the rival is, the timer holds exactly when the physics
        // step finds her side-blocking.
        let mut me = test_runner(1, Strategy::FrontRunner);
        me.position = 100.0;
        me.current_lane = 0.0;
        for gap in [0.0, 0.5, 1.0, 1.1, 2.0, 2.9, 3.5] {
            for lanes in [0.0, 0.5, 0.9, 1.5, 1.9, 2.5] {
                let snap = pair(gap, lanes, (1, 2), (1, 2));
                let physics = has_side_blocking_runner(&me, &proximity_snapshots(&snap), LANE);
                assert_eq!(
                    blocked(gap, lanes, false).0,
                    physics,
                    "{gap} m behind, {lanes} lanes across"
                );
            }
        }
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

    /// [`pair`] with `b` 1 m/s faster than `a`, by current and by target
    /// speed.
    fn chase(gap: f64, lanes: f64) -> FieldSnapshot {
        let mut snap = pair(gap, lanes, (1, 2), (1, 2));
        snap.entries[1].current_speed = 21.0;
        snap.entries[1].target_speed = 21.0;
        snap
    }

    /// One tick on a fresh tracker: whether `b` has an overtake target, and
    /// whether `a` is one.
    fn targets(mut snap: FieldSnapshot) -> (bool, bool) {
        update_condition_timers(&mut snap, &mut FieldOrderTracker::new(), &[], DT, LANE);
        (
            snap.condition_timers[&RunnerId(2)].has_overtake_target,
            snap.condition_timers[&RunnerId(1)].overtaken > 0.0,
        )
    }

    #[test]
    fn overtake_target_is_1_to_20_m_ahead_and_caught_within_15_s() {
        // b 10 m behind a and 1 m/s faster: catches in 10 s, a is b's target.
        assert_eq!(targets(chase(10.0, 0.0)), (true, true));
        // 0.5 m/s faster: 20 s to catch, no target.
        let mut snap = chase(10.0, 0.0);
        snap.entries[1].current_speed = 20.5;
        assert_eq!(targets(snap), (false, false));
        // 25 m ahead: out of range however fast.
        let mut snap = chase(25.0, 0.0);
        snap.entries[1].current_speed = 30.0;
        assert_eq!(targets(snap), (false, false));
        // Under 1 m ahead: not a target however fast (mechanics § Overtake
        // Targets: "between 1-20 m in front").
        assert_eq!(targets(chase(0.5, 0.0)), (false, false));
    }

    /// Overtake targets are the ones lane movement reads (mechanics § Overtake
    /// Targets), inside the vision cone (§ Vision): one horse lane either side
    /// at the runner, widening to 6.75 at 20 m. GameTora's note ("up to 20 m
    /// ahead, caught within 15 s") has no cone.
    #[test]
    fn overtake_target_is_inside_the_vision_cone() {
        // 5 m ahead the cone reaches 2.44 lanes either side.
        assert_eq!(targets(chase(5.0, 2.0)), (true, true));
        assert_eq!(targets(chase(5.0, 3.0)), (false, false));
        assert_eq!(targets(chase(5.0, -3.0)), (false, false));
        // 15 m ahead (7.5 s to catch at 2 m/s) it reaches 5.31 lanes.
        let wide = |lanes: f64| {
            let mut snap = chase(15.0, lanes);
            snap.entries[1].current_speed = 22.0;
            targets(snap)
        };
        assert_eq!(wide(5.0), (true, true));
        assert_eq!(wide(5.5), (false, false));
    }

    /// A target is slower by target speed, or blocked in front and slower than
    /// the chaser's target speed; the runner blocking the chaser in front is
    /// always one, however fast (mechanics § Overtake Targets).
    #[test]
    fn overtake_target_is_slower_by_target_speed_or_the_front_blocker() {
        // a aims faster than b and runs free: not a target, although b closes
        // on her at 1 m/s now.
        let mut snap = chase(10.0, 0.0);
        snap.entries[0].target_speed = 22.0;
        assert_eq!(targets(snap), (false, false));
        // The same a, blocked in front and slower than b's target: a target.
        let mut snap = chase(10.0, 0.0);
        snap.entries[0].target_speed = 22.0;
        snap.entries[0].front_blocker = Some(RunnerId(9));
        assert_eq!(targets(snap), (true, true));
        // Half a metre ahead at the same speed, but blocking b in front.
        let mut snap = pair(0.5, 0.0, (1, 2), (1, 2));
        snap.entries[1].front_blocker = Some(RunnerId(1));
        assert_eq!(targets(snap), (true, true));
    }

    /// `overtake_target_time` and `overtake_target_no_order_up_time` count the
    /// same targets as `is_overtake`: a runner outside the cone never starts
    /// either clock.
    #[test]
    fn overtake_timers_count_only_targets_in_the_vision_cone() {
        let held = |lanes: f64| {
            let mut tracker = FieldOrderTracker::new();
            let mut t = HashMap::new();
            for _ in 0..30 {
                let mut snap = chase(5.0, lanes);
                update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
                t = snap.condition_timers;
            }
            (
                t[&RunnerId(2)].overtake_target_no_order_up,
                t[&RunnerId(1)].overtaken,
            )
        };
        let (no_order_up, overtaken) = held(2.0);
        assert!(
            (no_order_up - 30.0 * DT).abs() < 1e-9,
            "30 ticks = 1.998 s, got {no_order_up}"
        );
        assert!(
            (overtaken - 30.0 * DT).abs() < 1e-9,
            "30 ticks = 1.998 s, got {overtaken}"
        );
        assert_eq!(held(3.0), (0.0, 0.0));
    }

    /// Runners passed, counted where the passer is: the Mid-Race `[D/6, 2D/3)`,
    /// the Late-Race from 2D/3, the final corner from its start (a 2400 m
    /// course with its final corner at 1800 m).
    #[test]
    fn order_up_counters_count_runners_passed_in_their_windows() {
        let mut tracker = FieldOrderTracker::new();
        tracker.order_up_windows = Some(OrderUpWindows {
            middle_start: 400.0,
            late_start: 1600.0,
            final_corner_start: Some(1800.0),
        });
        // Runner 1 at `at` goes from 3rd to 1st past runners 2 and 3.
        let pass_two = |tracker: &mut FieldOrderTracker, at: f64| {
            let mut snap = snapshot(vec![
                entry(1, at, Strategy::LateSurger),
                entry(2, at - 1.0, Strategy::FrontRunner),
                entry(3, at - 2.0, Strategy::PaceChaser),
            ]);
            for (id, now, before) in [(1, 1, 3), (2, 2, 1), (3, 3, 2)] {
                snap.order.insert(RunnerId(id), now);
                snap.previous_order.insert(RunnerId(id), before);
            }
            update_condition_timers(&mut snap, tracker, &[], DT, LANE);
            let t = snap.condition_timers[&RunnerId(1)];
            (
                t.order_up_middle,
                t.order_up_end_after,
                t.order_up_finalcorner_after,
            )
        };
        assert_eq!(pass_two(&mut tracker, 300.0), (0, 0, 0), "early race");
        assert_eq!(pass_two(&mut tracker, 1000.0), (2, 0, 0));
        assert_eq!(pass_two(&mut tracker, 1700.0), (2, 2, 0));
        assert_eq!(pass_two(&mut tracker, 1900.0), (2, 4, 2));
        // Being passed counts nothing for the runners she passed.
        let t = tracker.condition_timers[&RunnerId(2)];
        assert_eq!(t.order_up_middle + t.order_up_end_after, 0);

        // Without the course windows nothing is counted.
        let mut tracker = FieldOrderTracker::new();
        assert_eq!(pass_two(&mut tracker, 1900.0), (0, 0, 0));
    }

    /// The `*_continue` bands past the 5 s grace in a 12-runner field: the
    /// threshold's own place counts on both sides, so 8th stays outside the
    /// top 70 % (round(8.4) = 8) and 2nd within the top 20 % (round(2.4) = 2),
    /// while 7th and 3rd break them.
    #[test]
    fn continue_bands_keep_the_threshold_place() {
        use crate::runner::test_support::test_runner;
        use crate::skills::condition::dynamic::order_rate_band_index;

        let runners: Vec<Runner> = (1..=12_u32)
            .map(|id| {
                let mut r = test_runner(id, Strategy::PaceChaser);
                r.accumulate_time = RaceClock::after_ticks(90, DT); // 5.994 s
                r
            })
            .collect();
        let mut snap = snapshot(
            (1..=12_u32)
                .map(|id| entry(id, 1000.0 - 10.0 * f64::from(id), Strategy::PaceChaser))
                .collect(),
        );
        for id in 1..=12_u32 {
            snap.order.insert(RunnerId(id), i64::from(id));
            snap.previous_order.insert(RunnerId(id), i64::from(id));
        }
        let mut tracker = FieldOrderTracker::new();
        update_condition_timers(&mut snap, &mut tracker, &runners, DT, LANE);

        let in20 = order_rate_band_index(0.2).expect("a band");
        let out70 = order_rate_band_index(0.7).expect("a band");
        let timers = |id: u32| snap.condition_timers[&RunnerId(id)];
        assert!(timers(2).in_band[in20]);
        assert!(!timers(3).in_band[in20]);
        assert!(timers(8).out_band[out70]);
        assert!(!timers(7).out_band[out70]);
    }

    /// The band latch on GameTora's worked examples. Out bands, 9 umas: out70
    /// is 6th or worse, out50 5th or worse (round(4.5) = 5, a half rounding
    /// up), out40 4th or worse, out20 2nd or worse. In bands, 10 umas: in20 is
    /// 2nd or better, in40 4th, in50 5th, in80 8th. GameTora gives no in-band
    /// example for 9 umas; the same rounding puts in20 at 2nd, in40 4th, in50
    /// 5th and in80 7th.
    #[test]
    fn continue_bands_follow_gametoras_worked_examples() {
        use crate::runner::test_support::test_runner;
        use crate::skills::condition::dynamic::order_rate_band_index;

        // The first tick past the grace (tick 76, 5.0616 s), runner k in
        // place k of n.
        let latched = |n: u32| {
            let runners: Vec<Runner> = (1..=n)
                .map(|id| {
                    let mut r = test_runner(id, Strategy::PaceChaser);
                    r.accumulate_time = RaceClock::after_ticks(76, DT);
                    assert!(r.accumulate_time.seconds() > ORDER_CONTINUE_GRACE_SECONDS);
                    r
                })
                .collect();
            let mut snap = snapshot(
                (1..=n)
                    .map(|id| entry(id, 1000.0 - 10.0 * f64::from(id), Strategy::PaceChaser))
                    .collect(),
            );
            for id in 1..=n {
                snap.order.insert(RunnerId(id), i64::from(id));
                snap.previous_order.insert(RunnerId(id), i64::from(id));
            }
            update_condition_timers(&mut snap, &mut FieldOrderTracker::new(), &runners, DT, LANE);
            snap.condition_timers
        };
        let band = |rate: f64| order_rate_band_index(rate).expect("a band");

        let nine = latched(9);
        // (rate, the best place outside the top rate)
        for (rate, edge) in [(0.7, 6_u32), (0.5, 5), (0.4, 4), (0.2, 2)] {
            let out = |place: u32| nine[&RunnerId(place)].out_band[band(rate)];
            assert!(out(edge), "out {rate}: place {edge} of 9 holds");
            assert!(out(9), "out {rate}: place 9 of 9 holds");
            assert!(!out(edge - 1), "out {rate}: place {} of 9", edge - 1);
        }
        let ten = latched(10);
        // (field, rate, the worst place within the top rate)
        for (field, n, rate, edge) in [
            (&ten, 10, 0.2, 2_u32),
            (&ten, 10, 0.4, 4),
            (&ten, 10, 0.5, 5),
            (&ten, 10, 0.8, 8),
            (&nine, 9, 0.2, 2),
            (&nine, 9, 0.4, 4),
            (&nine, 9, 0.5, 5),
            (&nine, 9, 0.8, 7),
        ] {
            let within = |place: u32| field[&RunnerId(place)].in_band[band(rate)];
            assert!(within(1), "in {rate}: place 1 of {n} holds");
            assert!(within(edge), "in {rate}: place {edge} of {n} holds");
            assert!(!within(edge + 1), "in {rate}: place {} of {n}", edge + 1);
        }
    }

    /// The snapshot keeps the rearmost runner's position and every runner's
    /// popularity for the conditions that read them.
    #[test]
    fn the_field_view_carries_the_last_position_and_popularity() {
        use crate::runner::test_support::test_runner;

        let mut runners: Vec<Runner> = [
            (0_u32, Strategy::PaceChaser),
            (1, Strategy::FrontRunner),
            (2, Strategy::LateSurger),
        ]
        .into_iter()
        .map(|(id, strategy)| {
            let mut r = test_runner(id, strategy);
            r.gate = i64::from(id);
            r.popularity = i64::from(3 - id);
            r
        })
        .collect();
        runners[0].position = 120.0;
        runners[1].position = 150.0;
        runners[2].position = 90.0;
        let mut tracker = FieldOrderTracker::new();
        let snap = build_field_snapshot(&runners, &[], &mut tracker);
        assert_eq!(snap.leader_position, Some(150.0));
        assert_eq!(snap.last_position, Some(90.0));
        let view = build_field_view(RunnerId(0), &snap, false);
        assert_eq!(view.last_position, Some(90.0));
        let favourite = view
            .active_runners
            .iter()
            .find(|a| a.popularity == 1)
            .expect("popularity carried");
        assert_eq!(favourite.strategy, Strategy::LateSurger);
    }

    #[test]
    fn overtake_target_timer_resets_on_moving_up_a_place() {
        let mut tracker = FieldOrderTracker::new();
        for _ in 0..30 {
            let mut snap = chase(10.0, 0.0);
            update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        }
        assert!(tracker.condition_timers[&RunnerId(2)].overtake_target_no_order_up > 1.9);
        let mut snap = pair(10.0, 0.0, (2, 1), (1, 2));
        snap.entries[1].current_speed = 21.0;
        snap.entries[1].target_speed = 21.0;
        update_condition_timers(&mut snap, &mut tracker, &[], DT, LANE);
        assert_eq!(
            snap.condition_timers[&RunnerId(2)].overtake_target_no_order_up,
            0.0
        );
    }
}
