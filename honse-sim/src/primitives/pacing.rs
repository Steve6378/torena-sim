//! Pacer-selection **domain service** (`select_pacer`).
//!
//! Port of `getPacer()` from `common/race.ts`. Extracted as its own domain
//! service so the `Race` aggregate stays focused on invariants. Selection is
//! pure: it reads positions and keep-strategies and answers with an id. The
//! caller writes nothing back into the field.
//!
//! # Front-less fields
//!
//! With neither a Runaway nor a Front Runner in the field the game still needs
//! a pacemaker, and the mechanics doc calls the modern rule unclear
//! (`docs/mechanics/README.md`, § Pacemaker) while recording the older one:
//! "pacemaker was the first place uma among the most forward strategy". That is
//! what 21 recorded front-less races show, and it is what the front-less
//! election below returns -- with the elected runner's own style left alone, on
//! the opening frame and then again at every frame once her opening hold
//! ([`PACEMAKER_HOLD_CHECKS`]) has run out. Her exemption from pace down lives
//! in [`position_keep`](crate::position_keep), which is the only place the role
//! changes what a runner does.
//!
//! Which fields those are is decided by the runners' **running styles**, not by
//! their position-keep strategies: the doc's rule is about "the most forward
//! strategy", and a runner's strategy is the one she was entered with. The
//! rushed override rewrites `position_keep_strategy` to Front Runner for the
//! length of a spell (§ Rushed State), and reading the election off that field
//! would make a rushed runner the field's pacemaker from wherever she happens
//! to be -- including from the back, with everyone ahead of her then keeping
//! position against a runner behind them.

use std::cmp::Ordering;

use crate::runner::Runner;
use crate::shared_kernel::ids::RunnerId;
use crate::shared_kernel::language::{strategy_matches, Strategy};

/// Position-keep checks for which the runner elected on the opening frame of a
/// front-less field keeps the role, before the election starts following the
/// leader of her style.
///
/// This is the **Count** parameter of `docs/mechanics/README.md` § Pacemaker:
/// "The only known information is that a range parameter 10.0 (meters?) and a
/// count parameter 2(.0?) are used." Read as a number of checks it lands exactly
/// where the recordings put the handover. Checks run every 2 s from the opening
/// frame (§ Position Keeping; `handle_none` re-arms the timer at -2.0 after a
/// check that enters no mode), so a hold over the first two of them - at 0 s and
/// at 2 s - makes the third check, 4 s in, the first at which the role can move
/// and therefore the first at which the opening pacemaker can be paced down.
/// Every measured handover is there: in the 9 of 21 front-less races whose
/// opening holder lost a frame at the start, her speed steps down to 0.915 of
/// base target between the 4.26 s and 5.33 s recorded frames, which
/// back-extrapolates to 4.26-4.27 s at pace down's -0.5 m/s^2, on all three
/// course lengths (1200, 1400 and 1600 m) and regardless of when she was
/// actually headed (0.60 s to 14.92 s). In the other 12 she never enters the
/// band at all.
///
/// The § Pacemaker **Range** parameter (10.0) is deliberately not implemented:
/// nothing in the recordings reads on it - the first runner to lead her style by
/// 10 m inside sections 1-10 is nobody at all in 21 of 21 races.
///
/// The hold expires on the holder's own tally, so with `positionKeepMode` set to
/// anything but the virtual mode no check ever runs and the opening electee
/// keeps the role for the whole round. That mode switches the whole position-keep
/// machine off -- [`apply_virtual_position_keep`](crate::position_keep::apply_virtual_position_keep)
/// exits before any mode, threshold or coefficient is touched -- so the only
/// thing the frozen role can still reach is the observation port's pacer
/// position. Nothing a runner does depends on it there.
pub const PACEMAKER_HOLD_CHECKS: i64 = 2;

/// Pick the furthest-forward runner whose `position_keep_strategy` exactly
/// equals `strategy`.
fn furthest_with_exact_strategy(runners: &[Runner], strategy: Strategy) -> Option<&Runner> {
    furthest(runners, |r| r.position_keep_strategy == strategy)
}

/// Pick the furthest-forward runner whose **running style** is `strategy` -- the
/// style she was entered with, which no mechanic rewrites during a round.
fn furthest_with_base_style(runners: &[Runner], strategy: Strategy) -> Option<&Runner> {
    furthest(runners, |r| r.strategy == strategy)
}

/// Whether the field holds a Front Runner or a Runaway, by running style.
///
/// This is the split the doc draws ("the most forward strategy"), and it is
/// fixed for the round: `strategy` is set when the runner is entered and only
/// `position_keep_strategy` moves underneath it.
fn is_fronted(runners: &[Runner]) -> bool {
    runners
        .iter()
        .any(|r| strategy_matches(r.strategy, Strategy::FrontRunner))
}

/// Furthest-forward runner satisfying `predicate`. An exact tie on position
/// breaks on the lower gate: the innermost runner of the tied set wins.
///
/// The tie is not a corner case. On the opening frame every runner is still at
/// position 0.0, so in a front-less field the whole most-forward style group
/// ties and this comparator alone decides who starts the race as the pacemaker.
/// The recordings put the innermost gate of that style in normal mode -- the
/// pacemaker's mode -- in 21 of 21 front-less races (7 tournament, 14 Hanshin;
/// 7.5e-16 under a uniform pick inside the style group). The old strict-`>`
/// walk kept the earliest runner in iteration order, which already lands on the
/// rail in a field listed in gate order and lands anywhere in a field that is
/// not; saying it here makes the rail the rule rather than the listing order.
fn furthest(runners: &[Runner], predicate: impl Fn(&Runner) -> bool) -> Option<&Runner> {
    runners
        .iter()
        .filter(|r| predicate(r))
        .reduce(|best, runner| {
            match runner
                .position
                .partial_cmp(&best.position)
                .unwrap_or(Ordering::Equal)
            {
                Ordering::Greater => runner,
                Ordering::Less => best,
                Ordering::Equal if runner.gate < best.gate => runner,
                Ordering::Equal => best,
            }
        })
}

/// Which pass answered, so a caller never has to infer it from the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacerBranch {
    /// A fronted field's exact-strategy pass, over `position_keep_strategy`.
    ExactStrategy,
    /// A front-less field's opening election, still inside its
    /// [`PACEMAKER_HOLD_CHECKS`] hold.
    OpeningHold,
    /// A front-less field's election: first place of the most forward style.
    FrontLessElection,
    /// The existing pacer override (an empty field, and nothing else).
    Override,
}

/// The chosen pacer and the pass that chose her.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacerChoice {
    /// The chosen pacer.
    pub runner_id: RunnerId,
    /// The pass that answered.
    pub branch: PacerBranch,
}

impl PacerChoice {
    fn new(runner_id: RunnerId, branch: PacerBranch) -> Self {
        Self { runner_id, branch }
    }
}

/// Select the pacer for the field, mirroring `getPacer()`.
///
/// The field is split first, by running style -- fronted when any runner's
/// `strategy` is Front Runner or Runaway -- and the two halves never share a
/// pass:
///
/// **Fronted** (some runner's style is Front Runner or Runaway) -- furthest
/// Runaway, else furthest Front Runner, read from `position_keep_strategy`
/// exactly as v0.13.0 read it, so a rushed convert or a `ChangeStrategy` skill
/// still competes for the role here. One of those always answers, on every
/// frame of the race.
///
/// **Front-less** -- first the `opening_pacemaker`'s
/// [`PACEMAKER_HOLD_CHECKS`] hold, then the election: the current first-place
/// runner of the most forward style present, furthest Pace Chaser, else Late
/// Surger, else End Closer, by running style. The rushed override cannot reach
/// this half at all: a rushed runner is neither elected from behind, which is
/// what reading `position_keep_strategy` here would do, nor elected out of a
/// style the doc does not put in front just because she is leading the field.
/// No recording decides the second case -- a rushed convert has the front of a
/// front-less field in none of the 21 -- so the doc's wording decides it:
/// "pacemaker was the first place uma among the most forward strategy"
/// (§ Pacemaker) is about the style she runs, and the override is a
/// position-keep mode, not a style.
///
/// The election runs ahead of the override, where v0.13.0 ran it after, because
/// once the hold is out the role is decided anew on every frame. The recordings
/// hand it over when the opening holder loses a frame at the start (9 of 9 races
/// where she does, Fisher p = 3.4e-06) and never when she does not (12 of 12),
/// so an override consulted first would freeze the opening frame's election for
/// the whole race -- which, together with the keep-strategy rewrite this
/// replaces, is exactly what v0.13.0 did.
///
/// `pacer_override` is therefore the tail of an **empty field** and nothing
/// else: a fronted field is answered by its exact-strategy pass, and every
/// runner of a front-less field is a Pace Chaser, a Late Surger or an End
/// Closer by style, so its election answers whenever anybody is racing.
pub fn select_pacer(
    runners: &[Runner],
    pacer_override: Option<RunnerId>,
    opening_pacemaker: Option<RunnerId>,
) -> Option<PacerChoice> {
    if is_fronted(runners) {
        for strategy in [Strategy::Runaway, Strategy::FrontRunner] {
            if let Some(runner) = furthest_with_exact_strategy(runners, strategy) {
                return Some(PacerChoice::new(runner.id, PacerBranch::ExactStrategy));
            }
        }
    } else {
        if let Some(runner) = opening_pacemaker.and_then(|id| runners.iter().find(|r| r.id == id)) {
            if runner.pos_keep_checks_run < PACEMAKER_HOLD_CHECKS {
                return Some(PacerChoice::new(runner.id, PacerBranch::OpeningHold));
            }
        }

        for strategy in [
            Strategy::PaceChaser,
            Strategy::LateSurger,
            Strategy::EndCloser,
        ] {
            if let Some(runner) = furthest_with_base_style(runners, strategy) {
                return Some(PacerChoice::new(runner.id, PacerBranch::FrontLessElection));
            }
        }
    }

    pacer_override.map(|runner_id| PacerChoice::new(runner_id, PacerBranch::Override))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::test_runner;

    /// `select_pacer`'s pick, for the assertions that only care who it is.
    fn picked(
        runners: &[Runner],
        pacer_override: Option<RunnerId>,
        opening_pacemaker: Option<RunnerId>,
    ) -> Option<RunnerId> {
        select_pacer(runners, pacer_override, opening_pacemaker).map(|c| c.runner_id)
    }

    fn runner(id: u32, strategy: Strategy, position: f64) -> Runner {
        let mut r = test_runner(id, strategy);
        r.position = position;
        r.gate = i64::from(id);
        r
    }

    /// A runner in a rushed spell: the override has written Front Runner over
    /// her `position_keep_strategy` and left her style alone (§ Rushed State).
    fn rushed(id: u32, strategy: Strategy, position: f64) -> Runner {
        let mut r = runner(id, strategy, position);
        r.is_rushed = true;
        r.position_keep_strategy = Strategy::FrontRunner;
        r
    }

    #[test]
    fn prefers_furthest_runaway_then_front_runner() {
        let runners = vec![
            runner(0, Strategy::FrontRunner, 300.0),
            runner(1, Strategy::Runaway, 250.0),
            runner(2, Strategy::Runaway, 280.0),
        ];
        // Runaway is preferred over Front Runner; furthest Runaway is id 2.
        assert_eq!(picked(&runners, None, None), Some(RunnerId(2)));
    }

    #[test]
    fn falls_back_to_front_runner_when_no_runaway() {
        let runners = vec![
            runner(0, Strategy::FrontRunner, 200.0),
            runner(1, Strategy::FrontRunner, 260.0),
            runner(2, Strategy::PaceChaser, 300.0),
        ];
        assert_eq!(picked(&runners, None, None), Some(RunnerId(1)));
    }

    /// A fronted field never reaches the front-less fallback: the front runner
    /// answers even when a more-backward style leads the race outright, and a
    /// stale override cannot displace her. The 110 fronted tournament fixtures
    /// and the 39 fronted Hanshin ones must stay bit-identical.
    #[test]
    fn fronted_field_ignores_the_front_less_fallback_and_the_override() {
        let runners = vec![
            runner(0, Strategy::PaceChaser, 400.0),
            runner(1, Strategy::FrontRunner, 260.0),
            runner(2, Strategy::LateSurger, 380.0),
        ];
        assert_eq!(picked(&runners, None, None), Some(RunnerId(1)));
        assert_eq!(picked(&runners, Some(RunnerId(0)), None), Some(RunnerId(1)));
        // Even a hold recorded by a round that started front-less loses to her.
        assert_eq!(picked(&runners, None, Some(RunnerId(0))), Some(RunnerId(1)));
    }

    /// The front-less election: first place of the most forward style present,
    /// with no rewrite of anybody's strategy (the caller cannot promote her --
    /// `select_pacer` hands back an id and nothing else).
    #[test]
    fn front_less_field_elects_first_place_of_the_most_forward_style() {
        let runners = vec![
            runner(0, Strategy::LateSurger, 300.0),
            runner(1, Strategy::PaceChaser, 180.0),
            runner(2, Strategy::PaceChaser, 220.0),
        ];
        // Pace Chaser outranks the Late Surger who is physically ahead.
        assert_eq!(picked(&runners, None, None), Some(RunnerId(2)));
        assert!(runners
            .iter()
            .all(|r| r.position_keep_strategy == r.strategy));
    }

    /// The role follows that style's leader: it moves as soon as somebody else
    /// leads, and a previous holder passed in as the override cannot keep it.
    /// v0.13.0 froze it on the opening frame instead -- by rewriting the
    /// winner's keep-strategy to Front Runner, and by answering from the
    /// override before this walk ran. The recordings show the handover in 9 of
    /// 9 races whose opening holder lost a frame at the start.
    #[test]
    fn front_less_role_moves_to_the_new_leader_every_frame() {
        let mut runners = vec![
            runner(0, Strategy::PaceChaser, 220.0),
            runner(1, Strategy::PaceChaser, 180.0),
        ];
        // Past her hold: PACEMAKER_HOLD_CHECKS checks have run.
        runners[0].pos_keep_checks_run = PACEMAKER_HOLD_CHECKS;
        assert_eq!(picked(&runners, None, None), Some(RunnerId(0)));

        runners[1].position = 260.0;
        assert_eq!(
            picked(&runners, Some(RunnerId(0)), Some(RunnerId(0))),
            Some(RunnerId(1)),
            "the previous holder must not keep the role once she is headed"
        );
    }

    /// The opening hold: the runner elected on the opening frame keeps the role
    /// for her first `PACEMAKER_HOLD_CHECKS` checks even when a style-mate
    /// leads, and loses it at the next one. With checks 2 s apart from the
    /// opening frame that makes the third check, at 4 s, the first at which the
    /// role can move -- where the recordings put the handover in 9 of 9 races
    /// that have one.
    #[test]
    fn the_opening_pacemaker_keeps_the_role_for_her_first_checks() {
        let mut runners = vec![
            runner(0, Strategy::PaceChaser, 100.0),
            runner(1, Strategy::PaceChaser, 140.0),
        ];

        for checks in 0..PACEMAKER_HOLD_CHECKS {
            runners[0].pos_keep_checks_run = checks;
            assert_eq!(
                picked(&runners, Some(RunnerId(0)), Some(RunnerId(0))),
                Some(RunnerId(0)),
                "held after {checks} checks, though runner 1 leads"
            );
        }

        runners[0].pos_keep_checks_run = PACEMAKER_HOLD_CHECKS;
        assert_eq!(
            picked(&runners, Some(RunnerId(0)), Some(RunnerId(0))),
            Some(RunnerId(1)),
            "the hold is over: the role follows the leader of the style"
        );
    }

    /// The hold is the opening runner's alone. A later holder, elected because
    /// she leads, does not get one of her own even when her own tally is short
    /// (she spent the 2 s check inside pace down, so it never ran for her).
    #[test]
    fn a_later_holder_does_not_inherit_the_opening_hold() {
        let mut runners = vec![
            runner(0, Strategy::PaceChaser, 100.0),
            runner(1, Strategy::PaceChaser, 140.0),
        ];
        runners[0].pos_keep_checks_run = PACEMAKER_HOLD_CHECKS;
        runners[1].pos_keep_checks_run = 1;

        // Runner 1 leads and takes the role...
        assert_eq!(
            picked(&runners, Some(RunnerId(0)), Some(RunnerId(0))),
            Some(RunnerId(1))
        );
        // ...and gives it straight back when runner 0 heads her again.
        runners[0].position = 180.0;
        assert_eq!(
            picked(&runners, Some(RunnerId(1)), Some(RunnerId(0))),
            Some(RunnerId(0))
        );
    }

    /// The election of a front-less field reads running styles, so a rushed
    /// runner cannot be elected out of the back of it. She is a Pace Chaser who
    /// is 200 m behind the leader of her own style; the override has written
    /// Front Runner over her keep-strategy, and reading the election off that
    /// field would hand her the role, with every runner ahead of her then
    /// measuring a negative distance to the pacemaker -- below every minimum
    /// threshold -- and pacing down against a runner behind them.
    #[test]
    fn a_rushed_runner_is_not_elected_from_the_back_of_a_front_less_field() {
        let runners = vec![
            runner(0, Strategy::PaceChaser, 300.0),
            runner(1, Strategy::PaceChaser, 250.0),
            rushed(2, Strategy::PaceChaser, 100.0),
        ];
        let choice = select_pacer(&runners, None, None).expect("pacer");
        assert_eq!(choice.runner_id, RunnerId(0));
        assert_eq!(choice.branch, PacerBranch::FrontLessElection);
    }

    /// And not out of the front of it either. A rushed End Closer leading the
    /// whole field is still an End Closer, and Pace Chaser is the most forward
    /// style present, so the role stays with the Pace Chasers' leader. No
    /// recording decides this case -- no rushed convert has the front of a
    /// front-less race in any of the 21 -- so the doc decides it: the pacemaker
    /// is "the first place uma among the most forward strategy", and the
    /// override is a position-keep mode, not a strategy.
    #[test]
    fn a_rushed_runner_leading_a_front_less_field_is_not_elected_either() {
        let runners = vec![
            runner(0, Strategy::PaceChaser, 200.0),
            runner(1, Strategy::PaceChaser, 260.0),
            rushed(2, Strategy::EndCloser, 400.0),
        ];
        assert_eq!(picked(&runners, None, None), Some(RunnerId(1)));
    }

    /// A fronted field is untouched: its exact-strategy pass still reads
    /// `position_keep_strategy`, so a rushed convert ahead of the real front
    /// runner still takes the role, exactly as v0.13.0 has her take it.
    #[test]
    fn a_fronted_field_still_elects_a_rushed_convert_ahead_of_its_front_runner() {
        let runners = vec![
            runner(0, Strategy::FrontRunner, 200.0),
            rushed(1, Strategy::PaceChaser, 400.0),
            runner(2, Strategy::LateSurger, 300.0),
        ];
        let choice = select_pacer(&runners, None, None).expect("pacer");
        assert_eq!(choice.runner_id, RunnerId(1));
        assert_eq!(choice.branch, PacerBranch::ExactStrategy);
    }

    /// Every pass says which one it was, so no caller has to read it back off
    /// the field.
    #[test]
    fn every_pass_reports_itself() {
        let fronted = vec![
            runner(0, Strategy::FrontRunner, 200.0),
            runner(1, Strategy::PaceChaser, 300.0),
        ];
        assert_eq!(
            select_pacer(&fronted, None, None).map(|c| c.branch),
            Some(PacerBranch::ExactStrategy)
        );
        // A hold recorded by another round cannot reach a fronted field.
        assert_eq!(
            select_pacer(&fronted, None, Some(RunnerId(1))).map(|c| c.branch),
            Some(PacerBranch::ExactStrategy)
        );

        let mut front_less = vec![
            runner(0, Strategy::PaceChaser, 200.0),
            runner(1, Strategy::PaceChaser, 300.0),
        ];
        assert_eq!(
            select_pacer(&front_less, None, None).map(|c| c.branch),
            Some(PacerBranch::FrontLessElection)
        );
        assert_eq!(
            select_pacer(&front_less, None, Some(RunnerId(0))).map(|c| c.branch),
            Some(PacerBranch::OpeningHold)
        );
        front_less[0].pos_keep_checks_run = PACEMAKER_HOLD_CHECKS;
        assert_eq!(
            select_pacer(&front_less, None, Some(RunnerId(0))).map(|c| c.branch),
            Some(PacerBranch::FrontLessElection)
        );

        assert_eq!(
            select_pacer(&[], Some(RunnerId(7)), None).map(|c| c.branch),
            Some(PacerBranch::Override)
        );
    }

    /// An exact position tie breaks on the lower gate, not on the order the
    /// field happens to be listed in. On the opening frame every position is
    /// 0.0, so this is the rule that starts the race: the recordings put the
    /// innermost gate of the most forward style in normal mode in 21 of 21
    /// front-less races.
    #[test]
    fn exact_position_tie_breaks_on_the_inner_gate() {
        let mut runners = vec![
            runner(0, Strategy::PaceChaser, 0.0),
            runner(1, Strategy::PaceChaser, 0.0),
            runner(2, Strategy::PaceChaser, 0.0),
        ];
        // Listed innermost-first, the rail wins.
        assert_eq!(picked(&runners, None, None), Some(RunnerId(0)));

        // Listed in any other order, the rail still wins.
        runners[0].gate = 5;
        runners[1].gate = 3;
        runners[2].gate = 1;
        assert_eq!(picked(&runners, None, None), Some(RunnerId(2)));

        // A runner who is genuinely ahead beats a lower gate.
        runners[0].position = 0.5;
        assert_eq!(picked(&runners, None, None), Some(RunnerId(0)));
    }

    /// The override is the tail of the port, reached only when no runner can be
    /// elected at all.
    #[test]
    fn override_is_the_empty_field_fallback() {
        assert_eq!(picked(&[], Some(RunnerId(7)), None), Some(RunnerId(7)));
    }

    #[test]
    fn empty_field_without_override_is_none() {
        assert_eq!(picked(&[], None, None), None);
    }
}
