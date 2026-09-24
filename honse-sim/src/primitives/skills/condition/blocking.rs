//! Full-sim blocking / overtake dynamic conditions.
//!
//! Port of `full-sim/blocking-conditions.ts`. Each predicate inspects the
//! observing runner plus the snapshots of every other active runner (read
//! through [`RunnerView`]) to decide whether the runner is blocked to the side
//! or actively overtaking.
//!
//! Front blocking is **not** recomputed here: the predicates read
//! [`RunnerView::is_front_blocked`], the per-tick front-blocker the field
//! producer already resolved with the documented rule (0 < DistanceGap < 2 m and
//! `abs(LaneGap) <= (1.0 - 0.6 * DistanceGap / 2 m) * 0.75 HorseLane`, closest
//! gap wins — mechanics § Front Blocking). This is the same state the physics
//! speed cap and the replay's blocker column read, so a skill gated on being
//! blocked in front cannot disagree with the blocking the race actually applied.

use crate::skills::condition::dynamic::{
    bool_num, compare, register_dynamic_condition, DynamicCondition, RunnerView,
};

/// Side blocking reach along the course, either way (mechanics § Side Blocking).
const SIDE_BLOCK_DISTANCE_METERS: f64 = 1.05;
/// Side blocking reach across, in horse lanes either way (same section).
const SIDE_BLOCK_LANE_MULTIPLIER: f64 = 2.0;
const OVERTAKE_DISTANCE_METERS: f64 = 5.0;
const OVERTAKE_LANE_MULTIPLIER: f64 = 2.0;
const MOVING_LANE_EPSILON: f64 = 0.00001;

fn lane_threshold(runner: &dyn RunnerView, multiplier: f64) -> f64 {
    runner.horse_lane() * multiplier
}

/// Whether a runner `distance_gap` metres along the course and `lane_gap`
/// metres across from another blocks her side (mechanics § Side Blocking):
/// `abs(DistanceGap) < 1.05 m` and `abs(LaneGap) < 2 HorseLane`. The one
/// window for the physics step's side block and every `blocked_side*` /
/// `blocked_all*` condition, live field or not, so the two cannot disagree.
pub fn is_side_blocking(distance_gap: f64, lane_gap: f64, horse_lane: f64) -> bool {
    distance_gap.abs() < SIDE_BLOCK_DISTANCE_METERS
        && lane_gap.abs() < SIDE_BLOCK_LANE_MULTIPLIER * horse_lane
}

fn has_side_blocking_runner(runner: &dyn RunnerView) -> bool {
    runner.other_snapshots().iter().any(|snapshot| {
        is_side_blocking(
            snapshot.position - runner.position(),
            snapshot.current_lane - runner.current_lane(),
            runner.horse_lane(),
        )
    })
}

fn is_overtaking_runner(runner: &dyn RunnerView) -> bool {
    let threshold = lane_threshold(runner, OVERTAKE_LANE_MULTIPLIER);
    runner.other_snapshots().iter().any(|snapshot| {
        let is_faster = runner.current_speed() > snapshot.current_speed;
        let distance_gap = (snapshot.position - runner.position()).abs();
        let lane_delta = (snapshot.current_lane - runner.current_lane()).abs();
        is_faster && distance_gap <= OVERTAKE_DISTANCE_METERS && lane_delta <= threshold
    })
}

/// The "continuous time" proxy for a runner without a live field: while
/// `predicate` holds, the runner's elapsed race time is compared against `arg`;
/// otherwise zero. On the live field the conditions read
/// [`ConditionTimers`](crate::skills::condition::dynamic::ConditionTimers)
/// instead -- how long the state has actually held -- because this proxy makes
/// `...time >= 2` true on the first tick the state holds anywhere after 2 s.
fn continuous_time(runner: &dyn RunnerView, active: bool) -> f64 {
    if active {
        runner.accumulate_time()
    } else {
        0.0
    }
}

/// Register every blocking / overtake dynamic condition.
pub fn register_blocking_conditions() {
    register_dynamic_condition("blocked_front", |arg, cmp| {
        DynamicCondition::new(move |r| compare(bool_num(r.is_front_blocked()), arg as f64, cmp))
    });

    register_dynamic_condition("blocked_front_continuetime", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let secs = match r.condition_timers() {
                Some(t) => t.blocked_front,
                None => continuous_time(r, r.is_front_blocked()),
            };
            compare(secs, arg as f64, cmp)
        })
    });

    register_dynamic_condition("blocked_all_continuetime", |arg, cmp| {
        DynamicCondition::new(move |r| {
            // "Blocked on all sides" means blocked in front and on at least
            // one side (mechanics § Side Blocking), not on both flanks.
            let active = r.is_front_blocked() && has_side_blocking_runner(r);
            let secs = match r.condition_timers() {
                Some(t) => t.blocked_all,
                None => continuous_time(r, active),
            };
            compare(secs, arg as f64, cmp)
        })
    });

    // Mechanics § Side Blocking: a uma is blocked on either side while another
    // uma sits inside the side-block window. This token is the instantaneous
    // state; `blocked_side_continuetime` is its duration proxy. Both read the
    // same predicate, so the pair can never disagree.
    register_dynamic_condition("blocked_side", |arg, cmp| {
        DynamicCondition::new(move |r| {
            compare(bool_num(has_side_blocking_runner(r)), arg as f64, cmp)
        })
    });

    register_dynamic_condition("blocked_side_continuetime", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let secs = match r.condition_timers() {
                Some(t) => t.blocked_side,
                None => continuous_time(r, has_side_blocking_runner(r)),
            };
            compare(secs, arg as f64, cmp)
        })
    });

    // GameTora: an overtake target is an uma up to 20 m ahead that the runner
    // catches within 15 s at the current speeds. The live field resolves it;
    // without one, the older 5 m proxy stands.
    register_dynamic_condition("is_overtake", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let has = match r.condition_timers() {
                Some(t) => t.has_overtake_target,
                None => is_overtaking_runner(r),
            };
            compare(bool_num(has), arg as f64, cmp)
        })
    });

    // GameTora: 1 = just moved toward the inner fence, 2 = away from it.
    // v0.13.0 read any move as 1, so `is_move_lane==2` never held. Every
    // carried skill takes either direction, so their rates do not move.
    register_dynamic_condition("is_move_lane", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let direction = if r.lane_change_speed().abs() <= MOVING_LANE_EPSILON {
                0.0
            } else if r.lane_move_outward() {
                2.0
            } else {
                1.0
            };
            compare(direction, arg as f64, cmp)
        })
    });

    // GameTora: seconds the runner has been someone else's overtake target.
    register_dynamic_condition("overtake_target_time", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let secs = match r.condition_timers() {
                Some(t) => t.overtaken,
                None => continuous_time(r, is_overtaking_runner(r)),
            };
            compare(secs, arg as f64, cmp)
        })
    });

    // GameTora: seconds the runner has had an overtake target; the name says the
    // time runs while she has not moved up a place, so a pass resets it.
    register_dynamic_condition("overtake_target_no_order_up_time", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let secs = match r.condition_timers() {
                Some(t) => t.overtake_target_no_order_up,
                None => continuous_time(r, is_overtaking_runner(r)),
            };
            compare(secs, arg as f64, cmp)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared_kernel::language::Strategy;
    use crate::skills::condition::dynamic::{
        get_dynamic_condition, register_all_dynamic_conditions, ActiveRunner, RunnerSnapshot,
    };
    use crate::skills::condition::operator::CmpKind;

    #[derive(Default)]
    struct TestRunner {
        position: f64,
        current_lane: f64,
        current_speed: f64,
        lane_change_speed: f64,
        lane_move_outward: bool,
        accumulate_time: f64,
        is_front_blocked: bool,
        snapshots: Vec<RunnerSnapshot>,
    }

    impl RunnerView for TestRunner {
        fn position(&self) -> f64 {
            self.position
        }
        fn current_lane(&self) -> f64 {
            self.current_lane
        }
        fn current_speed(&self) -> f64 {
            self.current_speed
        }
        fn lane_change_speed(&self) -> f64 {
            self.lane_change_speed
        }
        fn lane_move_outward(&self) -> bool {
            self.lane_move_outward
        }
        fn horse_lane(&self) -> f64 {
            1.0
        }
        fn accumulate_time(&self) -> f64 {
            self.accumulate_time
        }
        fn is_front_blocked(&self) -> bool {
            self.is_front_blocked
        }
        fn other_snapshots(&self) -> Vec<RunnerSnapshot> {
            self.snapshots.clone()
        }
        fn active_runners(&self) -> Vec<ActiveRunner> {
            vec![ActiveRunner {
                is_self: true,
                position: self.position,
                strategy: Strategy::FrontRunner,
                gate: 0,
                is_rushed: false,
                is_dueling: false,
                has_dueled: false,
                activated_advantage_effect_types: 0,
                popularity: 0,
            }]
        }
    }

    fn snap(position: f64, lane: f64, speed: f64) -> RunnerSnapshot {
        RunnerSnapshot {
            position,
            current_lane: lane,
            current_speed: speed,
        }
    }

    /// The token reads the producer's per-tick front-blocker (mechanics § Front
    /// Blocking), not a geometry of its own. It used to recompute the window at
    /// 5 m / one full horse lane with no taper, so a runner 3 m ahead read as
    /// blocked while the physics (and the game) had nobody blocking.
    #[test]
    fn blocked_front_reads_the_resolved_front_blocker() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("blocked_front").expect("registered");
        let cond = factory(1, CmpKind::Eq);

        let blocked = TestRunner {
            position: 100.0,
            is_front_blocked: true,
            ..Default::default()
        };
        assert!(cond.eval(&blocked));

        // 3 m ahead, dead in lane: inside the old 5 m window, outside the
        // documented 2 m one. The producer resolved no blocker, so neither
        // does the token.
        let outside_documented_window = TestRunner {
            position: 100.0,
            is_front_blocked: false,
            snapshots: vec![snap(103.0, 0.0, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&outside_documented_window));

        let clear = TestRunner {
            position: 100.0,
            ..Default::default()
        };
        assert!(!cond.eval(&clear));
    }

    #[test]
    fn blocked_front_continuetime_reads_the_resolved_front_blocker() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("blocked_front_continuetime").expect("registered");
        let cond = factory(2, CmpKind::Gte);

        let blocked = TestRunner {
            position: 100.0,
            accumulate_time: 9.0,
            is_front_blocked: true,
            ..Default::default()
        };
        assert!(cond.eval(&blocked));

        let outside_documented_window = TestRunner {
            position: 100.0,
            accumulate_time: 9.0,
            is_front_blocked: false,
            snapshots: vec![snap(103.0, 0.0, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&outside_documented_window));
    }

    /// "Blocked on all sides" is blocked in front and on **at least one** side
    /// (mechanics § Side Blocking); it used to demand both flanks.
    #[test]
    fn blocked_all_continuetime_needs_front_and_one_side() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("blocked_all_continuetime").expect("registered");
        let cond = factory(2, CmpKind::Gte);

        let front_and_left = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            accumulate_time: 9.0,
            is_front_blocked: true,
            snapshots: vec![snap(101.0, 0.5, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&front_and_left));

        let front_and_right = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            accumulate_time: 9.0,
            is_front_blocked: true,
            snapshots: vec![snap(101.0, 1.5, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&front_and_right));

        let side_only = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            accumulate_time: 9.0,
            is_front_blocked: false,
            snapshots: vec![snap(101.0, 0.5, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&side_only));

        let front_only = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            accumulate_time: 9.0,
            is_front_blocked: true,
            ..Default::default()
        };
        assert!(!cond.eval(&front_only));
    }

    #[test]
    fn blocked_side_reports_the_instantaneous_side_block_state() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("blocked_side").expect("registered");
        let cond = factory(1, CmpKind::Eq);

        // Rival one meter back and half a horse lane inside: blocked.
        let blocked_left = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            snapshots: vec![snap(99.0, 0.5, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&blocked_left));

        // Rival half a horse lane outside: also blocked (either side counts).
        let blocked_right = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            snapshots: vec![snap(101.0, 1.5, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&blocked_right));

        // Beyond the side-block reach longitudinally.
        let too_far = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            snapshots: vec![snap(104.0, 0.5, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&too_far));

        // Beyond the side-block reach laterally.
        let off_lane = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            snapshots: vec![snap(101.0, 5.0, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&off_lane));

        // Nobody else on the track.
        let clear = TestRunner {
            position: 100.0,
            ..Default::default()
        };
        assert!(!cond.eval(&clear));
        assert!(factory(0, CmpKind::Eq).eval(&clear));
    }

    /// Without a live field the tokens read the same window as the timers and
    /// the physics step (mechanics § Side Blocking): under 1.05 m along and
    /// under two horse lanes across, a rival on the same line included. They
    /// read 3 m and one lane.
    #[test]
    fn blocked_side_reads_the_documented_window() {
        register_blocking_conditions();
        let cond = get_dynamic_condition("blocked_side").expect("registered")(1, CmpKind::Eq);
        let rival_at = |along: f64, across: f64| TestRunner {
            position: 0.0,
            current_lane: 3.0,
            accumulate_time: 9.0,
            snapshots: vec![snap(along, 3.0 + across, 0.0)],
            ..Default::default()
        };
        // 2 m along, half a lane across: inside 3 m, outside 1.05 m.
        assert!(!cond.eval(&rival_at(2.0, 0.5)));
        assert!(!cond.eval(&rival_at(-2.0, -0.5)));
        // Half a metre along, a lane and a half across: outside one lane,
        // inside two.
        assert!(cond.eval(&rival_at(0.5, 1.5)));
        assert!(cond.eval(&rival_at(-0.5, -1.5)));
        // On the same line.
        assert!(cond.eval(&rival_at(0.5, 0.0)));
        // Both edges are open.
        assert!(!cond.eval(&rival_at(1.05, 0.0)));
        assert!(!cond.eval(&rival_at(-1.05, 0.5)));
        assert!(!cond.eval(&rival_at(0.5, 2.0)));

        let continued = get_dynamic_condition("blocked_side_continuetime").expect("registered")(
            2,
            CmpKind::Gte,
        );
        assert!(!continued.eval(&rival_at(2.0, 0.5)));
        assert!(continued.eval(&rival_at(0.5, 1.5)));
    }

    #[test]
    fn blocked_side_continuetime_uses_elapsed_time_when_blocked() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("blocked_side_continuetime").expect("registered");
        let cond = factory(2, CmpKind::Gte);

        let blocked = TestRunner {
            position: 100.0,
            current_lane: 1.0,
            accumulate_time: 9.0,
            snapshots: vec![snap(101.0, 0.5, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&blocked));

        let unblocked = TestRunner {
            position: 100.0,
            accumulate_time: 9.0,
            snapshots: vec![],
            ..Default::default()
        };
        assert!(!cond.eval(&unblocked));
    }

    #[test]
    fn overtake_requires_faster_speed_and_proximity() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("is_overtake").expect("registered");
        let cond = factory(1, CmpKind::Eq);

        let overtaking = TestRunner {
            position: 100.0,
            current_speed: 20.0,
            snapshots: vec![snap(103.0, 0.0, 18.0)],
            ..Default::default()
        };
        assert!(cond.eval(&overtaking));

        let slower = TestRunner {
            position: 100.0,
            current_speed: 15.0,
            snapshots: vec![snap(103.0, 0.0, 18.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&slower));
    }

    #[test]
    fn is_move_lane_reads_the_direction_of_the_move() {
        register_blocking_conditions();
        let factory = get_dynamic_condition("is_move_lane").expect("registered");
        let inward = factory(1, CmpKind::Eq);
        let outward = factory(2, CmpKind::Eq);

        let moving_in = TestRunner {
            lane_change_speed: 0.5,
            ..Default::default()
        };
        assert!(inward.eval(&moving_in));
        assert!(!outward.eval(&moving_in));

        let moving_out = TestRunner {
            lane_change_speed: 0.5,
            lane_move_outward: true,
            ..Default::default()
        };
        assert!(outward.eval(&moving_out));
        assert!(!inward.eval(&moving_out));

        let still = TestRunner {
            lane_move_outward: true,
            ..Default::default()
        };
        assert!(!inward.eval(&still));
        assert!(!outward.eval(&still));
    }

    #[test]
    fn register_all_populates_blocking() {
        register_all_dynamic_conditions();
        assert!(get_dynamic_condition("blocked_front").is_some());
        assert!(get_dynamic_condition("overtake_target_time").is_some());
        assert!(get_dynamic_condition("blocked_side").is_some());
    }
}
