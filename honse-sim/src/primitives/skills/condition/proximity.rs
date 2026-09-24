//! Full-sim proximity dynamic conditions.
//!
//! Port of `full-sim/proximity-conditions.ts`. These count or detect nearby
//! runners (ahead, behind, in adjacent lanes) using the snapshots exposed by
//! [`RunnerView`].

use crate::skills::condition::dynamic::{
    bool_num, compare, register_dynamic_condition, DynamicCondition, RunnerSnapshot, RunnerView,
};

const NEAR_DISTANCE_METERS: f64 = 3.0;
const BASHIN_METERS: f64 = 2.5;
const NEAR_COUNT_LANE_MULTIPLIER: f64 = 3.0;
/// `is_surrounded` (mechanics § is_surrounded): a runner in front
/// (`0 < DistanceGap < 3 m`), one behind (`-3 m < DistanceGap < 0`), each
/// under 1.5 lanes across, and one outside (`abs(DistanceGap) < 1.5 m`,
/// `0 < LaneGap < 3` lanes).
const SURROUNDED_METERS: f64 = 3.0;
const SURROUNDED_LANE_MULTIPLIER: f64 = 1.5;
const SURROUNDED_OUT_METERS: f64 = 1.5;
const SURROUNDED_OUT_LANE_MULTIPLIER: f64 = 3.0;
/// Lanes closer than this are the same lane.
const SAME_LANE_EPSILON: f64 = 0.00001;
const NEAR_LANE_TIME_LANE_MULTIPLIER: f64 = 1.0;
const VISIBLE_DISTANCE_METERS: f64 = 20.0;
const VISIBLE_LANE_MULTIPLIER: f64 = 11.5;

/// Which side relative to the observing runner a predicate considers.
#[derive(Clone, Copy)]
enum Direction {
    Behind,
    Infront,
}

fn lane_threshold(runner: &dyn RunnerView, multiplier: f64) -> f64 {
    runner.horse_lane() * multiplier
}

fn within_distance(runner: &dyn RunnerView, other: &RunnerSnapshot, max_distance: f64) -> bool {
    (other.position - runner.position()).abs() <= max_distance
}

fn within_lane(runner: &dyn RunnerView, other: &RunnerSnapshot, lane_threshold: f64) -> bool {
    (other.current_lane - runner.current_lane()).abs() <= lane_threshold
}

fn is_ahead_of(runner: &dyn RunnerView, other: &RunnerSnapshot) -> bool {
    other.position > runner.position()
}

fn is_behind_of(runner: &dyn RunnerView, other: &RunnerSnapshot) -> bool {
    other.position < runner.position()
}

fn nearest_runner_behind(runner: &dyn RunnerView) -> Option<RunnerSnapshot> {
    let mut nearest: Option<RunnerSnapshot> = None;
    let mut nearest_distance = f64::INFINITY;
    for snapshot in runner.other_snapshots() {
        if !is_behind_of(runner, &snapshot) {
            continue;
        }
        let distance = runner.position() - snapshot.position;
        if distance < nearest_distance {
            nearest_distance = distance;
            nearest = Some(snapshot);
        }
    }
    nearest
}

fn nearest_runner_infront(runner: &dyn RunnerView) -> Option<RunnerSnapshot> {
    let mut nearest: Option<RunnerSnapshot> = None;
    let mut nearest_distance = f64::INFINITY;
    for snapshot in runner.other_snapshots() {
        if !is_ahead_of(runner, &snapshot) {
            continue;
        }
        let distance = snapshot.position - runner.position();
        if distance < nearest_distance {
            nearest_distance = distance;
            nearest = Some(snapshot);
        }
    }
    nearest
}

fn has_near_lane_runner(runner: &dyn RunnerView, direction: Direction) -> bool {
    let threshold = lane_threshold(runner, NEAR_LANE_TIME_LANE_MULTIPLIER);
    runner.other_snapshots().iter().any(|snapshot| {
        let in_direction = match direction {
            Direction::Behind => is_behind_of(runner, snapshot),
            Direction::Infront => is_ahead_of(runner, snapshot),
        };
        in_direction
            && within_distance(runner, snapshot, NEAR_DISTANCE_METERS)
            && within_lane(runner, snapshot, threshold)
    })
}

/// Seconds with an uma right behind / ahead. On the live field this is the
/// [`ConditionTimers`](crate::skills::condition::dynamic::ConditionTimers)
/// duration (the uma one place behind / ahead, 2.5 m and 1 lane, reset when
/// the runner's own placement changes). Without one, the older proxy: the
/// whole race clock while any uma is near (the snapshots carry no placement),
/// which makes `>= 3` true on the first near tick after 3 s.
fn near_lane_time(runner: &dyn RunnerView, direction: Direction) -> f64 {
    if let Some(t) = runner.condition_timers() {
        return match direction {
            Direction::Behind => t.near_behind,
            Direction::Infront => t.near_infront,
        };
    }
    if has_near_lane_runner(runner, direction) {
        runner.accumulate_time()
    } else {
        0.0
    }
}

/// Register every proximity dynamic condition.
pub fn register_proximity_conditions() {
    register_dynamic_condition("near_count", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let threshold = lane_threshold(r, NEAR_COUNT_LANE_MULTIPLIER);
            let count = r
                .other_snapshots()
                .iter()
                .filter(|s| {
                    within_distance(r, s, NEAR_DISTANCE_METERS) && within_lane(r, s, threshold)
                })
                .count();
            compare(count as f64, arg as f64, cmp)
        })
    });

    // Runners close in front of the observer (near_count filtered to ahead).
    register_dynamic_condition("near_infront_count", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let threshold = lane_threshold(r, NEAR_COUNT_LANE_MULTIPLIER);
            let position = r.position();
            let count = r
                .other_snapshots()
                .iter()
                .filter(|s| {
                    s.position > position
                        && within_distance(r, s, NEAR_DISTANCE_METERS)
                        && within_lane(r, s, threshold)
                })
                .count();
            compare(count as f64, arg as f64, cmp)
        })
    });

    // v0.13.0 read 1 lane in front and behind, up to 3 m inclusive, and had no
    // outside clause. One runner can meet two clauses (passing on the lane
    // outside), so two can surround.
    register_dynamic_condition("is_surrounded", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let lanes = lane_threshold(r, SURROUNDED_LANE_MULTIPLIER);
            let out_lanes = lane_threshold(r, SURROUNDED_OUT_LANE_MULTIPLIER);
            let (mut front, mut behind, mut out) = (false, false, false);
            for snapshot in r.other_snapshots() {
                let distance_gap = snapshot.position - r.position();
                let lane_gap = snapshot.current_lane - r.current_lane();
                if lane_gap.abs() < lanes {
                    front |= distance_gap > 0.0 && distance_gap < SURROUNDED_METERS;
                    behind |= distance_gap < 0.0 && distance_gap > -SURROUNDED_METERS;
                }
                out |= distance_gap.abs() < SURROUNDED_OUT_METERS
                    && lane_gap >= SAME_LANE_EPSILON
                    && lane_gap < out_lanes;
            }
            compare(bool_num(front && behind && out), arg as f64, cmp)
        })
    });

    register_dynamic_condition("bashin_diff_behind", |arg, cmp| {
        DynamicCondition::new(move |r| match nearest_runner_behind(r) {
            Some(nearest) => {
                let bashin_diff = (r.position() - nearest.position) / BASHIN_METERS;
                compare(bashin_diff, arg as f64, cmp)
            }
            None => false,
        })
    });

    register_dynamic_condition("bashin_diff_infront", |arg, cmp| {
        DynamicCondition::new(move |r| match nearest_runner_infront(r) {
            Some(nearest) => {
                let bashin_diff = (nearest.position - r.position()) / BASHIN_METERS;
                compare(bashin_diff, arg as f64, cmp)
            }
            None => false,
        })
    });

    register_dynamic_condition("behind_near_lane_time", |arg, cmp| {
        DynamicCondition::new(move |r| {
            compare(near_lane_time(r, Direction::Behind), arg as f64, cmp)
        })
    });
    // GameTora: like behind_near_lane_time with 5 m and 2.7 lanes.
    register_dynamic_condition("behind_near_lane_time_set1", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let secs = match r.condition_timers() {
                Some(t) => t.near_behind_set1,
                None => near_lane_time(r, Direction::Behind),
            };
            compare(secs, arg as f64, cmp)
        })
    });
    register_dynamic_condition("infront_near_lane_time", |arg, cmp| {
        DynamicCondition::new(move |r| {
            compare(near_lane_time(r, Direction::Infront), arg as f64, cmp)
        })
    });

    register_dynamic_condition("visiblehorse", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let threshold = lane_threshold(r, VISIBLE_LANE_MULTIPLIER);
            let count = r
                .other_snapshots()
                .iter()
                .filter(|s| {
                    let longitudinal = s.position - r.position();
                    (0.0..=VISIBLE_DISTANCE_METERS).contains(&longitudinal)
                        && within_lane(r, s, threshold)
                })
                .count();
            compare(count as f64, arg as f64, cmp)
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::condition::dynamic::get_dynamic_condition;
    use crate::skills::condition::operator::CmpKind;

    #[derive(Default)]
    struct TestRunner {
        position: f64,
        current_lane: f64,
        accumulate_time: f64,
        snapshots: Vec<RunnerSnapshot>,
    }

    impl RunnerView for TestRunner {
        fn position(&self) -> f64 {
            self.position
        }
        fn current_lane(&self) -> f64 {
            self.current_lane
        }
        fn horse_lane(&self) -> f64 {
            1.0
        }
        fn accumulate_time(&self) -> f64 {
            self.accumulate_time
        }
        fn other_snapshots(&self) -> Vec<RunnerSnapshot> {
            self.snapshots.clone()
        }
    }

    fn snap(position: f64, lane: f64) -> RunnerSnapshot {
        RunnerSnapshot {
            position,
            current_lane: lane,
            current_speed: 0.0,
        }
    }

    #[test]
    fn near_count_counts_runners_within_window() {
        register_proximity_conditions();
        let factory = get_dynamic_condition("near_count").expect("registered");
        let cond = factory(2, CmpKind::Gte);

        let crowded = TestRunner {
            position: 100.0,
            snapshots: vec![snap(101.0, 0.0), snap(99.0, 1.0), snap(120.0, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&crowded)); // two within 3m & lane window

        let sparse = TestRunner {
            position: 100.0,
            snapshots: vec![snap(101.0, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&sparse));
    }

    #[test]
    fn is_surrounded_needs_a_runner_in_front_behind_and_outside() {
        register_proximity_conditions();
        let factory = get_dynamic_condition("is_surrounded").expect("registered");
        let cond = factory(1, CmpKind::Eq);
        // She runs 2 lanes off the fence.
        let field = |snapshots: Vec<RunnerSnapshot>| TestRunner {
            position: 100.0,
            current_lane: 2.0,
            snapshots,
            ..Default::default()
        };

        // In front and behind in her lane, nobody outside: not surrounded.
        assert!(!cond.eval(&field(vec![snap(102.0, 2.0), snap(98.0, 2.0)])));
        // With a runner outside, two lanes out: surrounded.
        assert!(cond.eval(&field(vec![
            snap(102.0, 2.0),
            snap(98.0, 2.0),
            snap(100.5, 4.0)
        ])));
        // 1.2 lanes across still counts in front and behind (under 1.5).
        assert!(cond.eval(&field(vec![
            snap(102.0, 3.2),
            snap(98.0, 0.8),
            snap(100.5, 4.0)
        ])));
        // The runner outside must be under 1.5 m along, under 3 lanes out,
        // and out, not in.
        for beside in [snap(101.6, 4.0), snap(100.5, 5.0), snap(100.5, 1.0)] {
            assert!(!cond.eval(&field(vec![snap(102.0, 2.0), snap(98.0, 2.0), beside])));
        }
        // 3 m exactly is not in front.
        assert!(!cond.eval(&field(vec![
            snap(103.0, 2.0),
            snap(98.0, 2.0),
            snap(100.5, 4.0)
        ])));
        // One runner passing on the next lane out is both in front and outside.
        assert!(cond.eval(&field(vec![snap(101.0, 3.0), snap(98.0, 2.0)])));
    }

    #[test]
    fn bashin_diff_infront_uses_nearest_runner() {
        register_proximity_conditions();
        let factory = get_dynamic_condition("bashin_diff_infront").expect("registered");
        let cond = factory(1, CmpKind::Lte);

        // nearest infront is 2m ahead -> 2/2.5 = 0.8 <= 1
        let runner = TestRunner {
            position: 100.0,
            snapshots: vec![snap(102.0, 0.0), snap(110.0, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&runner));

        let none_infront = TestRunner {
            position: 100.0,
            snapshots: vec![snap(98.0, 0.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&none_infront));
    }

    #[test]
    fn infront_near_lane_time_uses_elapsed_time() {
        register_proximity_conditions();
        let factory = get_dynamic_condition("infront_near_lane_time").expect("registered");
        let cond = factory(1, CmpKind::Gte);

        let near = TestRunner {
            position: 100.0,
            accumulate_time: 3.0,
            snapshots: vec![snap(102.0, 0.0)],
            ..Default::default()
        };
        assert!(cond.eval(&near));

        let far = TestRunner {
            position: 100.0,
            accumulate_time: 3.0,
            snapshots: vec![snap(102.0, 5.0)],
            ..Default::default()
        };
        assert!(!cond.eval(&far));
    }
}
