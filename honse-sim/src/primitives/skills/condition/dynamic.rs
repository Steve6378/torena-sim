//! Runtime (dynamic) condition predicate + the live-race view trait it observes.
//!
//! In the TypeScript engine a dynamic condition is `(runner: Runner) => boolean`
//! evaluated each tick. Here it is a boxed closure over the [`RunnerView`] trait,
//! which is the anti-corruption seam: the skills context observes live race state
//! without depending on the `racing` module. `RunnerView` is intentionally empty
//! for now and is fleshed out by the full-sim condition work (t-009).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, LazyLock, Once, RwLock};

use crate::shared_kernel::language::Strategy;
use crate::skills::condition::operator::CmpKind;

/// A point-in-time snapshot of another runner, as observed during a tick.
///
/// Mirrors the TypeScript `RunnerSnapshot` (`position` / `currentLane` /
/// `currentSpeed`) used by the blocking and proximity conditions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunnerSnapshot {
    /// Longitudinal race position in meters.
    pub position: f64,
    /// Current lateral lane offset.
    pub current_lane: f64,
    /// Current speed in m/s.
    pub current_speed: f64,
}

/// Per-runner condition state the live field keeps frame by frame, for the
/// conditions whose value depends on history rather than on this tick alone.
/// Definitions follow GameTora's skill-condition viewer (read 23 Sep 2026):
///
/// * `near_behind` / `near_infront`: seconds with at least one uma no more than
///   2.5 m behind / ahead and no more than 1 lane (1/18 of the course width, the
///   engine's `horse_lane`) to either side; any uma counts, but the timer resets
///   whenever the runner's own placement changes.
/// * `near_behind_set1`: the same with 5 m and 2.7 lanes.
/// * `blocked_front` / `blocked_side` / `blocked_all`: seconds blocked in front,
///   on at least one side, and both at once, continuously.
/// * `overtake_target_no_order_up`: seconds with at least one overtake target
///   (an uma up to 20 m ahead that the runner catches within 15 s at the current
///   speeds), reset when the runner moves up a place.
/// * `overtaken`: seconds the runner has been someone else's overtake target.
/// * `has_overtake_target`: this tick, by the same definition.
/// * `behind_is_inner`: the uma directly behind in placement runs closer to the
///   inner fence.
/// * `in_band` / `out_band`: whether the runner has stayed within / outside the
///   top 20, 40, 50, 70 and 80 % (placement against `round(n * rate)`, as the
///   plain `order_rate`) at every tick after the first 5 s.
/// * `order_up_middle` / `order_up_end_after` / `order_up_finalcorner_after`:
///   how many times the runner has overtaken someone during the Mid-Race,
///   since entering the Late-Race, and since entering the final corner; a
///   runner passed is one ahead of her on the previous tick and behind her on
///   this one.
#[derive(Debug, Clone, Copy)]
pub struct ConditionTimers {
    /// Seconds with an uma right behind (2.5 m, 1 lane).
    pub near_behind: f64,
    /// Seconds with an uma behind (5 m, 2.7 lanes).
    pub near_behind_set1: f64,
    /// Seconds with an uma right ahead (2.5 m, 1 lane).
    pub near_infront: f64,
    /// Seconds blocked in front, continuously.
    pub blocked_front: f64,
    /// Seconds blocked on at least one side, continuously.
    pub blocked_side: f64,
    /// Seconds blocked in front and on a side at once, continuously.
    pub blocked_all: f64,
    /// Seconds with an overtake target, reset on moving up a place.
    pub overtake_target_no_order_up: f64,
    /// Seconds as someone's overtake target, continuously.
    pub overtaken: f64,
    /// Whether the runner has an overtake target this tick.
    pub has_overtake_target: bool,
    /// Whether the uma directly behind runs closer to the inner fence.
    pub behind_is_inner: bool,
    /// Still within the top 20/40/50/70/80 % since 5 s.
    pub in_band: [bool; 5],
    /// Still outside the top 20/40/50/70/80 % since 5 s.
    pub out_band: [bool; 5],
    /// Runners passed during the Mid-Race.
    pub order_up_middle: i64,
    /// Runners passed since entering the Late-Race.
    pub order_up_end_after: i64,
    /// Runners passed since entering the final corner.
    pub order_up_finalcorner_after: i64,
}

impl Default for ConditionTimers {
    fn default() -> Self {
        Self {
            near_behind: 0.0,
            near_behind_set1: 0.0,
            near_infront: 0.0,
            blocked_front: 0.0,
            blocked_side: 0.0,
            blocked_all: 0.0,
            overtake_target_no_order_up: 0.0,
            overtaken: 0.0,
            has_overtake_target: false,
            behind_is_inner: false,
            in_band: [true; 5],
            out_band: [true; 5],
            order_up_middle: 0,
            order_up_end_after: 0,
            order_up_finalcorner_after: 0,
        }
    }
}

/// The order-rate bands the `order_rate_{in,out}NN_continue` conditions use,
/// in the order of [`ConditionTimers::in_band`] / [`ConditionTimers::out_band`].
pub const ORDER_RATE_BANDS: [f64; 5] = [0.2, 0.4, 0.5, 0.7, 0.8];

/// Index of `rate` in [`ORDER_RATE_BANDS`].
pub fn order_rate_band_index(rate: f64) -> Option<usize> {
    ORDER_RATE_BANDS
        .iter()
        .position(|b| (b - rate).abs() < 1e-9)
}

/// Live state of an active (non-finished) runner, used by the state conditions
/// (temptation / dueling counts). Includes the observing runner itself, flagged
/// via [`is_self`](ActiveRunner::is_self) so `includeSelf=false` predicates can
/// skip it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveRunner {
    /// Whether this entry is the observing runner.
    pub is_self: bool,
    /// Longitudinal race position in meters.
    pub position: f64,
    /// Running style.
    pub strategy: Strategy,
    /// Gate (post) number, 0-based.
    pub gate: i64,
    /// Popularity rank (1 = most popular; `0` = unknown).
    pub popularity: i64,
    /// Whether the runner is currently rushed (temptation).
    pub is_rushed: bool,
    /// Whether the runner is currently in a duel.
    pub is_dueling: bool,
    /// Whether the runner's duel is already over (one Showdown per race).
    pub has_dueled: bool,
    /// Bitmask of positive self-applied effect types this runner has activated
    /// (bit `n` => SkillType id `n`). Read by
    /// `is_other_character_activate_advantage_skill`.
    pub activated_advantage_effect_types: u64,
}

/// Compare a numeric value against an argument under a [`CmpKind`].
///
/// Faithful port of the TypeScript `compare(value, arg, cmp)` helper; uses exact
/// floating comparison by design (the engine compares integral orders/counts and
/// derived ratios the same way).
#[allow(
    clippy::float_cmp,
    reason = "exact comparison is intentional: ports the TS `compare` oracle which \
               compares integral orders/counts and identically-derived ratios; \
               approximate comparison would diverge from the parity reference"
)]
pub fn compare(value: f64, arg: f64, cmp: CmpKind) -> bool {
    match cmp {
        CmpKind::Eq => value == arg,
        CmpKind::Neq => value != arg,
        CmpKind::Lt => value < arg,
        CmpKind::Lte => value <= arg,
        CmpKind::Gt => value > arg,
        CmpKind::Gte => value >= arg,
    }
}

/// Map a boolean predicate to the `0`/`1` numeric value the engine compares.
pub fn bool_num(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

/// Read-only view of a live runner that dynamic conditions evaluate against.
///
/// This is the anti-corruption contract the skills context reads live state
/// through; implemented by the `racing` context's `Runner` (t-013+). Methods
/// have defaults (neutral values) so lightweight test doubles can opt in to only
/// what they exercise; the real `Runner` overrides all of them.
pub trait RunnerView {
    /// Elapsed race time in seconds (`accumulateTime.t`).
    fn accumulate_time(&self) -> f64 {
        0.0
    }
    /// Total number of skills activated so far.
    fn skills_activated_count(&self) -> i64 {
        0
    }
    /// Skills activated on this tick so far and on the previous tick.
    fn recent_skill_activations(&self) -> i64 {
        0
    }
    /// Number of skills activated during the given phase index (0..=2).
    fn skills_activated_in_phase(&self, _phase: usize) -> i64 {
        0
    }
    /// Number of skills activated during the given half of the race (0 = first,
    /// 1 = second).
    fn skills_activated_half_race(&self, _half: usize) -> i64 {
        0
    }
    /// Number of recovery (heal) skills activated.
    fn heals_activated_count(&self) -> i64 {
        0
    }
    /// Fraction of HP remaining (0.0..=1.0).
    fn health_ratio_remaining(&self) -> f64 {
        1.0
    }
    /// Whether the runner still has HP left.
    fn has_remaining_health(&self) -> bool {
        true
    }
    /// Whether a skill with the given id has been used.
    fn has_used_skill(&self, _skill_id: &str) -> bool {
        false
    }
    /// The runner's start delay in seconds.
    fn start_delay(&self) -> f64 {
        0.0
    }
    /// Whether the runner is in last spurt.
    fn is_last_spurt(&self) -> bool {
        false
    }
    /// Last-spurt transition marker (`-1` when not transitioned).
    fn last_spurt_transition(&self) -> f64 {
        -1.0
    }
    /// The runner's gate (post) number.
    fn gate(&self) -> i64 {
        0
    }
    /// The runner's random-lot roll.
    fn random_lot(&self) -> i64 {
        0
    }

    // --- full-sim live state (t-009; implemented by the racing `Runner`) ---

    /// Longitudinal race position in meters.
    fn position(&self) -> f64 {
        0.0
    }
    /// Current lateral lane offset.
    fn current_lane(&self) -> f64 {
        0.0
    }
    /// Current speed in m/s.
    fn current_speed(&self) -> f64 {
        0.0
    }
    /// Rate of lateral lane change (non-zero while moving lanes).
    fn lane_change_speed(&self) -> f64 {
        0.0
    }
    /// Whether the lane move [`lane_change_speed`](RunnerView::lane_change_speed)
    /// describes is away from the inner fence.
    fn lane_move_outward(&self) -> bool {
        false
    }
    /// The course's per-horse lane width (`course.horseLane`).
    fn horse_lane(&self) -> f64 {
        0.0
    }
    /// The course section length (`course.distance / 24`).
    fn section_length(&self) -> f64 {
        0.0
    }
    /// The total course distance in meters.
    fn course_distance(&self) -> f64 {
        0.0
    }
    /// The current race phase index (0..=2).
    fn phase(&self) -> i64 {
        0
    }
    /// The runner's running style, if known.
    fn strategy(&self) -> Option<Strategy> {
        None
    }
    /// Whether the runner is currently rushed (temptation).
    fn is_rushed(&self) -> bool {
        false
    }
    /// How many times the runner has been rushed so far this race. Without a
    /// race history, the current spell only.
    fn temptation_count(&self) -> i64 {
        i64::from(self.is_rushed())
    }
    /// Whether the runner is currently dueling.
    fn is_dueling(&self) -> bool {
        false
    }
    /// Whether a runner blocks this one in front this tick (mechanics § Front
    /// Blocking). Resolved by the field producer with the same predicate the
    /// physics step uses, so the token conditions and the speed cap never
    /// disagree; `false` when no live field resolved it.
    fn is_front_blocked(&self) -> bool {
        false
    }
    /// The runner's current finishing order (1-based), if assigned.
    fn current_order(&self) -> Option<i64> {
        None
    }
    /// The runner's order on the previous tick, if assigned.
    fn previous_order(&self) -> Option<i64> {
        None
    }
    /// The number of runners in the race.
    fn num_umas(&self) -> i64 {
        0
    }
    /// The leader's (order-1) position in meters, if known.
    fn leader_position(&self) -> Option<f64> {
        None
    }
    /// The rearmost active runner's position in meters, if known.
    fn last_position(&self) -> Option<f64> {
        None
    }
    /// The live field's per-runner condition timers and latches, if a live
    /// field keeps them (the contested engine does; `None` elsewhere, where the
    /// conditions keep their older instantaneous reading).
    fn condition_timers(&self) -> Option<ConditionTimers> {
        None
    }
    /// Snapshots of every other active runner (excludes self).
    fn other_snapshots(&self) -> Vec<RunnerSnapshot> {
        Vec::new()
    }
    /// Live state of every active (non-finished) runner, including self.
    fn active_runners(&self) -> Vec<ActiveRunner> {
        Vec::new()
    }
}

/// A runtime predicate gating a skill's activation, evaluated each tick.
///
/// Cloning is cheap (shared `Arc`). There is deliberately no `PartialEq`: closure
/// identity is not meaningful. The "no extra condition" case is modeled as
/// `Option::<DynamicCondition>::None` rather than a sentinel value (see
/// [`ConditionResult`](super::ConditionResult)).
#[derive(Clone)]
pub struct DynamicCondition(Arc<DynCondFn>);

/// The boxed predicate type behind a [`DynamicCondition`].
type DynCondFn = dyn Fn(&dyn RunnerView) -> bool + Send + Sync;

impl DynamicCondition {
    /// Wrap a predicate closure.
    pub fn new(f: impl Fn(&dyn RunnerView) -> bool + Send + Sync + 'static) -> Self {
        DynamicCondition(Arc::new(f))
    }

    /// The trivial always-true condition (`kTrue`). Prefer representing "no
    /// condition" as `None`; this materializes it when a concrete value is
    /// required.
    pub fn k_true() -> Self {
        DynamicCondition::new(|_| true)
    }

    /// Evaluate the predicate against a live runner view.
    pub fn eval(&self, runner: &dyn RunnerView) -> bool {
        (self.0)(runner)
    }
}

impl fmt::Debug for DynamicCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DynamicCondition(..)")
    }
}

/// Evaluate an optional dynamic condition; `None` means always-true (`kTrue`).
pub fn eval_dynamic(cond: &Option<DynamicCondition>, runner: &dyn RunnerView) -> bool {
    cond.as_ref().is_none_or(|c| c.eval(runner))
}

/// Builds a [`DynamicCondition`] for a given comparison argument + operator.
/// Populated by the full-sim/approximate condition work (t-009).
pub type DynamicConditionFactory = fn(arg: i64, cmp: CmpKind) -> DynamicCondition;

static REGISTRY: LazyLock<RwLock<HashMap<&'static str, DynamicConditionFactory>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Register a dynamic-condition factory under `name` (called during setup).
pub fn register_dynamic_condition(name: &'static str, factory: DynamicConditionFactory) {
    if let Ok(mut guard) = REGISTRY.write() {
        guard.insert(name, factory);
    }
}

/// Look up a registered dynamic-condition factory.
pub fn get_dynamic_condition(name: &str) -> Option<DynamicConditionFactory> {
    REGISTRY
        .read()
        .ok()
        .and_then(|guard| guard.get(name).copied())
}

/// Whether a dynamic condition is registered for `name`.
pub fn has_dynamic_condition(name: &str) -> bool {
    get_dynamic_condition(name).is_some()
}

static REGISTER_ALL: Once = Once::new();

/// Populate the dynamic-condition registry with every full-sim factory.
///
/// Idempotent (guarded by [`Once`]); the catalog/application calls this once
/// before resolving conditions under `Dynamic` resolution. Mirrors the
/// TypeScript `registerAllDynamicConditions`.
pub fn register_all_dynamic_conditions() {
    REGISTER_ALL.call_once(|| {
        super::order::register_order_conditions();
        super::proximity::register_proximity_conditions();
        super::blocking::register_blocking_conditions();
        super::state::register_state_conditions();
    });
}
