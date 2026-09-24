//! Full-sim order / position dynamic conditions.
//!
//! Port of `full-sim/order-conditions.ts`. These read the runner's current and
//! previous finishing order, the field size, and the leader's position (all
//! through [`RunnerView`]) to gate order-relative skills.

use crate::skills::condition::dynamic::{
    bool_num, compare, order_rate_band_index, register_dynamic_condition, DynamicCondition,
    RunnerView,
};

use crate::skills::condition::operator::CmpKind;

const CONTINUE_GRACE_PERIOD_SECONDS: f64 = 5.0;

fn order_rate_continue(
    runner: &dyn RunnerView,
    rate: f64,
    is_in_rate: bool,
    arg: i64,
    cmp: CmpKind,
) -> bool {
    let Some(order) = runner.current_order() else {
        return false;
    };
    let threshold = (runner.num_umas() as f64 * rate).round() as i64;
    let within_rate = if is_in_rate {
        order <= threshold
    } else {
        order > threshold
    };
    // "For the entire race until now" (GameTora): the live field latches the
    // band from 5 s on, so one tick on the wrong side ends it for the race.
    // Without a live field, only the current tick can be read.
    let held = match (runner.condition_timers(), order_rate_band_index(rate)) {
        (Some(t), Some(i)) => {
            if is_in_rate {
                t.in_band[i]
            } else {
                t.out_band[i]
            }
        }
        _ => true,
    };
    let active = runner.accumulate_time() > CONTINUE_GRACE_PERIOD_SECONDS && within_rate && held;
    compare(bool_num(active), arg as f64, cmp)
}

/// The window a `change_order_up_*` token counts overtakes in.
#[derive(Clone, Copy)]
enum OrderUpWindow {
    Middle,
    EndAfter,
    FinalCornerAfter,
}

/// GameTora: how many times the runner has overtaken someone in the window, a
/// count compared with the argument (`>=2`, `>=3`). v0.13.0 compared the 0/1
/// "placing improved this tick" flag with it, so `>=2` never held: 100191 and
/// 900171 never fired against 79.5% and 51.8% on the 117 recordings, where
/// the runners passed since 2D/3 split firing 58/59 at 2 or more against 0/14
/// (100191) and 29/31 at 3 or more against 0/25 (900171). The live field
/// counts the runners passed; without one, only this tick's improvement can
/// be read.
fn change_order_up(runner: &dyn RunnerView, window: OrderUpWindow, arg: i64, cmp: CmpKind) -> bool {
    if let Some(t) = runner.condition_timers() {
        let passed = match window {
            OrderUpWindow::Middle => t.order_up_middle,
            OrderUpWindow::EndAfter => t.order_up_end_after,
            OrderUpWindow::FinalCornerAfter => t.order_up_finalcorner_after,
        };
        return compare(passed as f64, arg as f64, cmp);
    }
    let (Some(previous), Some(current)) = (runner.previous_order(), runner.current_order()) else {
        return false;
    };
    let improved = current < previous;
    compare(bool_num(improved), arg as f64, cmp)
}

/// Register every order / position dynamic condition.
pub fn register_order_conditions() {
    register_dynamic_condition("order", |arg, cmp| {
        DynamicCondition::new(move |r| match r.current_order() {
            Some(order) => compare(order as f64, arg as f64, cmp),
            None => false,
        })
    });

    register_dynamic_condition("order_rate", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let Some(order) = r.current_order() else {
                return false;
            };
            let order_as_position = (r.num_umas() as f64 * (arg as f64 / 100.0)).round();
            compare(order as f64, order_as_position, cmp)
        })
    });

    register_dynamic_condition("order_rate_in20_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.2, true, arg, cmp))
    });
    register_dynamic_condition("order_rate_in40_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.4, true, arg, cmp))
    });
    register_dynamic_condition("order_rate_in50_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.5, true, arg, cmp))
    });
    register_dynamic_condition("order_rate_in80_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.8, true, arg, cmp))
    });

    register_dynamic_condition("order_rate_out20_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.2, false, arg, cmp))
    });
    register_dynamic_condition("order_rate_out40_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.4, false, arg, cmp))
    });
    register_dynamic_condition("order_rate_out50_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.5, false, arg, cmp))
    });
    register_dynamic_condition("order_rate_out70_continue", |arg, cmp| {
        DynamicCondition::new(move |r| order_rate_continue(r, 0.7, false, arg, cmp))
    });

    register_dynamic_condition("change_order_onetime", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let (Some(previous), Some(current)) = (r.previous_order(), r.current_order()) else {
                return false;
            };
            let order_delta = current - previous;
            compare(order_delta as f64, arg as f64, cmp)
        })
    });

    register_dynamic_condition("change_order_up_end_after", |arg, cmp| {
        DynamicCondition::new(move |r| change_order_up(r, OrderUpWindow::EndAfter, arg, cmp))
    });
    register_dynamic_condition("change_order_up_finalcorner_after", |arg, cmp| {
        DynamicCondition::new(move |r| {
            change_order_up(r, OrderUpWindow::FinalCornerAfter, arg, cmp)
        })
    });
    register_dynamic_condition("change_order_up_middle", |arg, cmp| {
        DynamicCondition::new(move |r| change_order_up(r, OrderUpWindow::Middle, arg, cmp))
    });

    register_dynamic_condition("distance_diff_top", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let Some(leader) = r.leader_position() else {
                return false;
            };
            let diff_meters = (leader - r.position()).max(0.0);
            compare(diff_meters.floor(), arg as f64, cmp)
        })
    });

    register_dynamic_condition("distance_diff_top_float", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let Some(leader) = r.leader_position() else {
                return false;
            };
            let diff_decimeters = ((leader - r.position()) * 10.0).max(0.0);
            compare(diff_decimeters, arg as f64, cmp)
        })
    });

    // GameTora: "your position between the currently first and the currently
    // last girl as a percentage" -- the gap to the leader over the field's
    // spread. v0.13.0 divided by the course distance, under 1% for nearly
    // everyone, so `>=75` never held and `<=30` always did: on the 117
    // recordings the spread reading predicts 56 of 62 carriers fired where it
    // held in the window and 0 of 18 where it never did.
    register_dynamic_condition("distance_diff_rate", |arg, cmp| {
        DynamicCondition::new(move |r| {
            let (Some(leader), Some(last)) = (r.leader_position(), r.last_position()) else {
                return false;
            };
            let spread = leader - last;
            let rate = if spread > 0.0 {
                (leader - r.position()).max(0.0) / spread * 100.0
            } else {
                0.0
            };
            compare(rate, arg as f64, cmp)
        })
    });

    // GameTora: "the uma behind you is closer to the inner fence than you" --
    // the runner directly behind in placement. v0.13.0 compared the runner's own
    // placement with the argument, so `is_behind_in==1` held only for the
    // leader. Without a live field there is no one behind to read.
    register_dynamic_condition("is_behind_in", |arg, cmp| {
        DynamicCondition::new(move |r| match r.condition_timers() {
            Some(t) => compare(bool_num(t.behind_is_inner), arg as f64, cmp),
            None => false,
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::condition::dynamic::{get_dynamic_condition, ConditionTimers};

    #[derive(Default)]
    struct TestRunner {
        position: f64,
        course_distance: f64,
        accumulate_time: f64,
        num_umas: i64,
        current_order: Option<i64>,
        previous_order: Option<i64>,
        leader_position: Option<f64>,
        last_position: Option<f64>,
        timers: Option<ConditionTimers>,
    }

    impl RunnerView for TestRunner {
        fn position(&self) -> f64 {
            self.position
        }
        fn course_distance(&self) -> f64 {
            self.course_distance
        }
        fn accumulate_time(&self) -> f64 {
            self.accumulate_time
        }
        fn num_umas(&self) -> i64 {
            self.num_umas
        }
        fn current_order(&self) -> Option<i64> {
            self.current_order
        }
        fn previous_order(&self) -> Option<i64> {
            self.previous_order
        }
        fn leader_position(&self) -> Option<f64> {
            self.leader_position
        }
        fn last_position(&self) -> Option<f64> {
            self.last_position
        }
        fn condition_timers(&self) -> Option<ConditionTimers> {
            self.timers
        }
    }

    #[test]
    fn order_compares_current_order() {
        register_order_conditions();
        let factory = get_dynamic_condition("order").expect("registered");
        let cond = factory(3, CmpKind::Lte);

        let third = TestRunner {
            current_order: Some(3),
            ..Default::default()
        };
        assert!(cond.eval(&third));

        let fourth = TestRunner {
            current_order: Some(4),
            ..Default::default()
        };
        assert!(!cond.eval(&fourth));

        let unknown = TestRunner::default();
        assert!(!cond.eval(&unknown));
    }

    #[test]
    fn order_rate_in_continue_respects_grace_period() {
        register_order_conditions();
        let factory = get_dynamic_condition("order_rate_in50_continue").expect("registered");
        let cond = factory(1, CmpKind::Eq);

        // 12 runners, threshold = round(12*0.5)=6; order 3 <= 6 and time > 5s -> active.
        let active = TestRunner {
            num_umas: 12,
            current_order: Some(3),
            accumulate_time: 6.0,
            ..Default::default()
        };
        assert!(cond.eval(&active));

        // Same position but before the grace period -> inactive.
        let too_early = TestRunner {
            num_umas: 12,
            current_order: Some(3),
            accumulate_time: 4.0,
            ..Default::default()
        };
        assert!(!cond.eval(&too_early));
    }

    #[test]
    fn change_order_up_detects_improvement() {
        register_order_conditions();
        let factory = get_dynamic_condition("change_order_up_middle").expect("registered");
        let cond = factory(1, CmpKind::Eq);

        let improved = TestRunner {
            previous_order: Some(5),
            current_order: Some(3),
            ..Default::default()
        };
        assert!(cond.eval(&improved));

        let dropped = TestRunner {
            previous_order: Some(3),
            current_order: Some(5),
            ..Default::default()
        };
        assert!(!cond.eval(&dropped));
    }

    #[test]
    fn change_order_up_counts_the_runners_passed_in_its_window() {
        register_order_conditions();
        let end_after = get_dynamic_condition("change_order_up_end_after").expect("registered");
        let middle = get_dynamic_condition("change_order_up_middle").expect("registered");
        let final_corner =
            get_dynamic_condition("change_order_up_finalcorner_after").expect("registered");
        let with = |middle: i64, end_after: i64, final_corner: i64| TestRunner {
            // No improvement on this tick: the count is what is read.
            previous_order: Some(4),
            current_order: Some(4),
            timers: Some(ConditionTimers {
                order_up_middle: middle,
                order_up_end_after: end_after,
                order_up_finalcorner_after: final_corner,
                ..ConditionTimers::default()
            }),
            ..Default::default()
        };

        assert!(end_after(2, CmpKind::Gte).eval(&with(0, 2, 0)));
        assert!(!end_after(3, CmpKind::Gte).eval(&with(5, 2, 0)));
        assert!(middle(3, CmpKind::Gte).eval(&with(3, 0, 0)));
        assert!(!middle(1, CmpKind::Gte).eval(&with(0, 4, 4)));
        assert!(final_corner(2, CmpKind::Gte).eval(&with(0, 3, 2)));
        assert!(!final_corner(2, CmpKind::Gte).eval(&with(0, 3, 1)));
    }

    #[test]
    fn distance_diff_rate_is_a_share_of_the_fields_spread() {
        register_order_conditions();
        let factory = get_dynamic_condition("distance_diff_rate").expect("registered");
        // Leader at 1300 m, last at 1200 m: a 100 m spread on a 2400 m course.
        let at = |position: f64| TestRunner {
            position,
            course_distance: 2400.0,
            leader_position: Some(1300.0),
            last_position: Some(1200.0),
            ..Default::default()
        };
        assert!(factory(30, CmpKind::Lte).eval(&at(1275.0))); // 25 %
        assert!(!factory(30, CmpKind::Lte).eval(&at(1250.0))); // 50 %
        assert!(factory(75, CmpKind::Gte).eval(&at(1210.0))); // 90 %
        assert!(!factory(75, CmpKind::Gte).eval(&at(1250.0)));
        // No spread (one runner left): she is at the front.
        let alone = TestRunner {
            position: 1300.0,
            leader_position: Some(1300.0),
            last_position: Some(1300.0),
            ..Default::default()
        };
        assert!(factory(0, CmpKind::Eq).eval(&alone));
    }

    #[test]
    fn distance_diff_top_floors_meters() {
        register_order_conditions();
        let factory = get_dynamic_condition("distance_diff_top").expect("registered");
        let cond = factory(5, CmpKind::Gte);

        let behind = TestRunner {
            position: 100.0,
            leader_position: Some(106.7),
            ..Default::default()
        };
        assert!(cond.eval(&behind)); // floor(6.7) = 6 >= 5

        let close = TestRunner {
            position: 100.0,
            leader_position: Some(103.0),
            ..Default::default()
        };
        assert!(!cond.eval(&close)); // floor(3) = 3 < 5
    }
}
