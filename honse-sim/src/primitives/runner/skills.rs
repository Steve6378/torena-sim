//! Skill activation & effect application (t-015).
//!
//! Ports the skill half of `common/runner.ts` (`processSkillActivations`,
//! `activateSkill`, `activateRandomGoldSkill`, the wit check, targeted-effect
//! application) plus `buildSkillData` from `runner/runner.utils.ts` (adapted to
//! take a pre-resolved [`Skill`] instead of a service lookup).
//!
//! Dynamic `extra_condition` gates are evaluated through a
//! [`RunnerConditionView`] that combines the runner's self-state with the
//! per-frame [`FieldView`] (snapshot-derived field data the aggregate builds in
//! t-017). The view is constructed immutably and released before the `&mut self`
//! activation call, so the borrow checker is satisfied and resolution order is
//! irrelevant.

use std::collections::HashSet;

use crate::runner::lifecycle::PrepareContext;
use crate::runner::{Runner, UsedTargetedSkill, FRAME_DT};
use crate::shared_kernel::ids::SkillId;
use crate::shared_kernel::language::Strategy;
use crate::shared_kernel::math::Timer;
use crate::shared_kernel::params::{RaceParameters, StatLine};
use crate::shared_kernel::region::{Region, RegionList};
use crate::skills::activation::ActivationSamplePolicy;
use crate::skills::condition::dynamic::{
    eval_dynamic, ActiveRunner, DynamicCondition, RunnerSnapshot as DynRunnerSnapshot, RunnerView,
};
use crate::skills::condition::language::ConditionParser;
use crate::skills::condition::{ApplyParams, ConditionResolution, SkillEvalRunner};
use crate::skills::debuff::{get_external_debuff_effects, is_emittable_external_effect};
use crate::skills::effect::{SkillRarity, SkillType};
use crate::skills::model::{
    build_skill_effects, duration_scaling_multiplier, ActiveSkill, ActiveTargetedSkill,
    DynamicPrecondition, EmittedDebuff, HeldAdditionalEffect, OrderUpExtension, PendingSkill,
    PendingTargetedSkill, ResolvedSkillEffect, Skill, SkillEffectSpec, SkillTrigger,
    TargetedSkillOrigin,
};
use crate::skills::recovery::resolve_effect_modifier;

/// Snapshot-derived field data dynamic skill conditions read through the
/// [`RunnerView`] seam. Built once per frame by the aggregate (t-017).
#[derive(Debug, Clone, Default)]
pub struct FieldView {
    /// This runner's current finishing order (1-based), if assigned.
    pub self_order: Option<i64>,
    /// This runner's previous-tick order, if assigned.
    pub self_previous_order: Option<i64>,
    /// Number of active runners in the field.
    pub num_umas: i64,
    /// The leader's position in meters, if known.
    pub leader_position: Option<f64>,
    /// The rearmost active runner's position in meters, if known.
    pub last_position: Option<f64>,
    /// This runner's condition timers and latches (live field only).
    pub condition_timers: Option<crate::skills::condition::dynamic::ConditionTimers>,
    /// Whether a runner blocks this one in front this tick (mechanics § Front
    /// Blocking). The field producer resolves it with the same predicate the
    /// physics step's speed cap reads, so the `blocked_front*` token conditions
    /// cannot disagree with the physics.
    pub is_front_blocked: bool,
    /// Snapshots of every *other* active runner.
    pub other_snapshots: Vec<DynRunnerSnapshot>,
    /// Live state of every active runner (including self).
    pub active_runners: Vec<ActiveRunner>,
}

impl FieldView {
    /// The trivial field view used at the gate (no field resolved yet).
    pub fn at_gate() -> Self {
        FieldView::default()
    }
}

/// A read-only [`RunnerView`] combining a runner's self-state with the per-frame
/// [`FieldView`]. The anti-corruption bridge between the racing `Runner` and the
/// skills condition language.
pub struct RunnerConditionView<'a> {
    runner: &'a Runner,
    field: &'a FieldView,
}

impl RunnerView for RunnerConditionView<'_> {
    fn accumulate_time(&self) -> f64 {
        self.runner.accumulate_time.t
    }
    fn skills_activated_count(&self) -> i64 {
        self.runner.skills_activated_count
    }
    fn recent_skill_activations(&self) -> i64 {
        self.runner.skills_activated_count - self.runner.activations_at_last_tick_start
    }
    fn skills_activated_in_phase(&self, phase: usize) -> i64 {
        self.runner
            .skills_activated_phase_map
            .get(phase)
            .copied()
            .unwrap_or(0)
    }
    fn skills_activated_half_race(&self, half: usize) -> i64 {
        self.runner
            .skills_activated_half_race_map
            .get(half)
            .copied()
            .unwrap_or(0)
    }
    fn heals_activated_count(&self) -> i64 {
        self.runner.heals_activated_count
    }
    fn health_ratio_remaining(&self) -> f64 {
        self.runner.health_policy.health_ratio_remaining()
    }
    fn has_remaining_health(&self) -> bool {
        self.runner.health_policy.has_remaining_health()
    }
    fn has_used_skill(&self, skill_id: &str) -> bool {
        self.runner.used_skills.contains(skill_id)
    }
    fn start_delay(&self) -> f64 {
        self.runner.start_delay
    }
    fn is_last_spurt(&self) -> bool {
        self.runner.is_last_spurt
    }
    fn last_spurt_transition(&self) -> f64 {
        self.runner.last_spurt_transition
    }
    fn gate(&self) -> i64 {
        self.runner.gate
    }
    fn random_lot(&self) -> i64 {
        self.runner.random_lot
    }
    fn position(&self) -> f64 {
        self.runner.position
    }
    fn current_lane(&self) -> f64 {
        self.runner.current_lane
    }
    fn current_speed(&self) -> f64 {
        self.runner.current_speed
    }
    fn lane_change_speed(&self) -> f64 {
        self.runner.lane_change_speed
    }
    fn lane_move_outward(&self) -> bool {
        self.runner.lane_move_outward
    }
    fn horse_lane(&self) -> f64 {
        self.runner.horse_lane
    }
    fn section_length(&self) -> f64 {
        self.runner.section_length
    }
    fn course_distance(&self) -> f64 {
        self.runner.course_distance
    }
    fn phase(&self) -> i64 {
        self.runner.phase.index() as i64
    }
    fn strategy(&self) -> Option<Strategy> {
        Some(self.runner.strategy)
    }
    fn is_rushed(&self) -> bool {
        self.runner.is_rushed
    }
    fn temptation_count(&self) -> i64 {
        self.runner.rushed_activations.len() as i64
    }
    fn is_dueling(&self) -> bool {
        self.runner.is_dueling
    }
    fn is_front_blocked(&self) -> bool {
        self.field.is_front_blocked
    }
    fn current_order(&self) -> Option<i64> {
        self.field.self_order
    }
    fn previous_order(&self) -> Option<i64> {
        self.field.self_previous_order
    }
    fn num_umas(&self) -> i64 {
        self.field.num_umas
    }
    fn leader_position(&self) -> Option<f64> {
        self.field.leader_position
    }
    fn last_position(&self) -> Option<f64> {
        self.field.last_position
    }
    fn condition_timers(&self) -> Option<crate::skills::condition::dynamic::ConditionTimers> {
        self.field.condition_timers
    }
    fn other_snapshots(&self) -> Vec<DynRunnerSnapshot> {
        self.field.other_snapshots.clone()
    }
    fn active_runners(&self) -> Vec<ActiveRunner> {
        self.field.active_runners.clone()
    }
}

/// Inputs to [`build_skill_data`]: a pre-resolved skill plus the static
/// condition-evaluation context.
pub struct BuildSkillDataParams<'a> {
    /// Static view of the runner (base stats / strategy / mood).
    pub runner: &'a SkillEvalRunner,
    /// Race-wide parameters.
    pub race_params: &'a RaceParameters,
    /// The course being raced.
    pub course: &'a crate::course::model::CourseData,
    /// The whole course as a region list.
    pub whole_course: &'a RegionList,
    /// The condition parser (bound to the static catalog).
    pub parser: &'a ConditionParser<'a>,
    /// The pre-resolved skill.
    pub skill: &'a Skill,
    /// Whether to keep triggers whose effect list is empty.
    pub ignore_null_effects: bool,
    /// Engine-supplied condition-resolution strategy (dynamic vs static).
    pub resolution: ConditionResolution,
}

/// Build the [`SkillTrigger`]s for a pre-resolved skill.
///
/// Port of `buildSkillData` (minus the service lookup / simulatable guard, which
/// the data layer performs upstream). Unparseable conditions yield an empty
/// result rather than panicking.
pub fn build_skill_data(params: &BuildSkillDataParams<'_>) -> Vec<SkillTrigger> {
    let skill = params.skill;
    let mut extra = params.race_params.clone();
    extra.skill_id = Some(skill.skill_id.clone());

    let mut triggers: Vec<SkillTrigger> = Vec::new();
    // The data index of the alternative behind `triggers[0]`.
    let mut first_alternative = 0;

    for (index, alt) in skill.alternatives.iter().enumerate() {
        if alt.condition.is_empty() {
            continue;
        }

        let mut full = params.whole_course.clone();
        let mut runtime_pre: Option<DynamicPrecondition> = None;

        // An empty precondition string means "no precondition" (TS treats it as
        // falsy in `if (skillAlternative.precondition)`). Skipping it is required
        // for skills whose data carries `precondition: ""` (e.g. all_corner_random
        // / rotation greens), which otherwise fail to parse and never activate.
        if let Some(precondition) = alt.precondition.as_deref().filter(|p| !p.is_empty()) {
            let Ok(parsed_pre) = params.parser.parse(precondition) else {
                return Vec::new();
            };
            let pre_params = ApplyParams {
                regions: params.whole_course.clone(),
                course: params.course,
                runner: params.runner,
                extra: &extra,
                resolution: params.resolution,
            };
            let Ok((pre_regions, pre_check)) = parsed_pre.apply(&pre_params) else {
                return Vec::new();
            };
            if pre_regions.0.is_empty() {
                continue;
            }
            let Some(last) = params.whole_course.last() else {
                continue;
            };
            let bounds = Region::new(pre_regions.0[0].start, last.end);
            full = full.rmap(|r| r.intersect(&bounds));
            runtime_pre = pre_check.map(|check| DynamicPrecondition {
                regions: pre_regions,
                check,
                met: false,
            });
        }

        let Ok(parsed_op) = params.parser.parse(&alt.condition) else {
            return Vec::new();
        };
        let apply_params = ApplyParams {
            regions: full,
            course: params.course,
            runner: params.runner,
            extra: &extra,
            resolution: params.resolution,
        };
        let Ok((regions, extra_condition)) = parsed_op.apply(&apply_params) else {
            return Vec::new();
        };
        if regions.0.is_empty() {
            continue;
        }

        // A later alternative naming the multi-trigger tokens triggers on its
        // own. Any other is the same skill under another condition: the game
        // checks the alternatives in order and fires the first that holds,
        // once. Such alternatives used to be dropped; the recordings' skill
        // events log the alternative that fired (params[3]), and 117 of them
        // went through the second, over 23 skills (110101: 10 of its 25).
        let exclusive = !triggers.is_empty() && !condition_allows_second_trigger(&alt.condition);

        let effects = build_skill_effects(alt);
        if !effects.is_empty() || params.ignore_null_effects {
            if triggers.is_empty() {
                first_alternative = index;
            } else if exclusive {
                triggers[0]
                    .exclusive_alternative
                    .get_or_insert(first_alternative);
            }
            triggers.push(SkillTrigger {
                skill_id: skill.skill_id.clone(),
                rarity: skill.rarity,
                tags: skill.tags.clone(),
                sample_policy: parsed_op.sample_policy(),
                regions,
                effects,
                extra_condition,
                target_strategy: derive_target_strategy(&alt.condition),
                duration_scaling: alt.duration_scaling,
                cooldown_time: alt.cooldown_time,
                precondition: runtime_pre,
                exclusive_alternative: exclusive.then_some(index),
            });
        }
    }

    if !triggers.is_empty() {
        return triggers;
    }

    // Fallback: place the first alternative after the course end with a
    // constantly-false dynamic condition (summer Goldship unique edge case).
    let Some(first) = skill.alternatives.first() else {
        return Vec::new();
    };
    let effects = build_skill_effects(first);
    if effects.is_empty() && !params.ignore_null_effects {
        return Vec::new();
    }
    let mut after_end = RegionList::new();
    after_end.push(Region::new(9999.0, 9999.0));
    vec![SkillTrigger {
        skill_id: skill.skill_id.clone(),
        rarity: skill.rarity,
        tags: skill.tags.clone(),
        sample_policy: ActivationSamplePolicy::Immediate,
        regions: after_end,
        effects,
        extra_condition: Some(DynamicCondition::new(|_| false)),
        target_strategy: None,
        duration_scaling: first.duration_scaling,
        cooldown_time: first.cooldown_time,
        precondition: None,
        exclusive_alternative: None,
    }]
}

/// Whether a later alternative triggers on its own rather than as an exclusive
/// alternative of the first (only when its condition explicitly references the
/// multi-trigger tokens).
fn condition_allows_second_trigger(condition: &str) -> bool {
    condition.contains("is_activate_other_skill_detail") || condition.contains("is_used_skill_id")
}

/// Derive the running style a strategy-targeted external debuff hits from its
/// activation condition. The effect data is identical across each family, so the
/// only signal for *which* strategy is hit is a running-style token in the
/// condition: `running_style_count_<style>_otherself` for the *Hesitant*
/// (`EnemyStrategy`) family, or `running_style_temptation_opponent_count_<style>`
/// for the *Frenzied* (`KakariStrategy`) family. Returns `None` when the
/// condition names no such style.
fn derive_target_strategy(condition: &str) -> Option<Strategy> {
    use crate::shared_kernel::language::Strategy;
    if condition.contains("running_style_count_nige_otherself")
        || condition.contains("running_style_temptation_opponent_count_nige")
    {
        Some(Strategy::FrontRunner)
    } else if condition.contains("running_style_count_senko_otherself")
        || condition.contains("running_style_temptation_opponent_count_senko")
    {
        Some(Strategy::PaceChaser)
    } else if condition.contains("running_style_count_sashi_otherself")
        || condition.contains("running_style_temptation_opponent_count_sashi")
    {
        Some(Strategy::LateSurger)
    } else if condition.contains("running_style_count_oikomi_otherself")
        || condition.contains("running_style_temptation_opponent_count_oikomi")
    {
        Some(Strategy::EndCloser)
    } else {
        None
    }
}

impl Runner {
    /// Reset and rebuild the skill-tracking state for a fresh round.
    ///
    /// Port of `initializeSkillTracking` (+ `initializeTargetedSkillTracking`).
    pub(crate) fn initialize_skill_tracking(&mut self, ctx: &PrepareContext<'_>) {
        self.target_speed_skills_active.clear();
        self.current_speed_skills_active.clear();
        self.acceleration_skills_active.clear();
        self.lane_movement_skills_active.clear();
        self.change_lane_skills_active.clear();
        self.targeted_target_speed_active.clear();
        self.targeted_current_speed_active.clear();
        self.targeted_acceleration_active.clear();
        self.targeted_lane_movement_skills_active.clear();
        self.targeted_change_lane_skills_active.clear();

        self.skills_activated_count = 0;
        self.activations_at_tick_start = 0;
        self.activations_at_last_tick_start = 0;
        self.skills_activated_phase_map = [0; 4];
        self.skills_activated_half_race_map = [0; 2];
        self.heals_activated_count = 0;
        self.used_skills.clear();
        self.activated_ledger.clear();
        self.activated_advantage_effect_types = 0;
        self.used_targeted_skills.clear();
        self.emitted_debuffs.clear();
        self.pending_skill_removal.clear();
        self.pending_skills.clear();
        self.held_additional_effects.clear();
        self.order_up_extensions.clear();
        self.pending_targeted_skills.clear();

        let eval_runner = self.skill_eval_runner();
        let skills = std::mem::take(&mut self.skills);
        let mut pending: Vec<PendingSkill> = Vec::new();
        let mut forced_bypass_granted: HashSet<String> = HashSet::new();
        for skill in &skills {
            let triggers = build_skill_data(&BuildSkillDataParams {
                runner: &eval_runner,
                race_params: ctx.race_params,
                course: ctx.course,
                whole_course: ctx.whole_course,
                parser: ctx.parser,
                skill,
                ignore_null_effects: false,
                resolution: ctx.condition_resolution,
            });
            for trigger in triggers {
                let base = trigger.skill_id.base().to_owned();
                let forced_pos = self.forced_positions.get(&base).copied();
                // A forced skill activates unconditionally at its position:
                // only the first alternative's trigger gets the bypass so
                // mutually exclusive alternatives cannot both fire there.
                let forced = forced_pos.is_some() && forced_bypass_granted.insert(base);
                // The scripted activation stands for the skill: its exclusive
                // alternatives stay out.
                if forced_pos.is_some() && !forced && trigger.exclusive_alternative.is_some() {
                    continue;
                }
                let policy = match forced_pos {
                    Some(pos) => ActivationSamplePolicy::Fixed(pos),
                    None => trigger.sample_policy,
                };
                let sets =
                    policy.sample_sets(&trigger.regions, ctx.skill_samples, &mut *self.skill_rng);
                if sets.is_empty() {
                    continue;
                }
                let set = &sets[ctx.round_iteration % sets.len()];
                let trigger_region = set[0];
                let cooldown = if forced {
                    0.0
                } else {
                    trigger
                        .cooldown_time
                        .map_or(0.0, |c| c / 10000.0 * ctx.course.distance / 1000.0)
                };
                pending.push(PendingSkill {
                    skill_id: trigger.skill_id,
                    rarity: trigger.rarity,
                    tags: trigger.tags,
                    trigger: trigger_region,
                    effects: trigger.effects,
                    extra_condition: if forced {
                        None
                    } else {
                        trigger.extra_condition
                    },
                    target_strategy: trigger.target_strategy,
                    duration_scaling: trigger.duration_scaling,
                    cooldown,
                    later_triggers: if forced {
                        Vec::new()
                    } else {
                        set[1..].to_vec()
                    },
                    ready_at: f64::NEG_INFINITY,
                    wit_passed: false,
                    forced,
                    precondition: if forced { None } else { trigger.precondition },
                    exclusive_alternative: trigger.exclusive_alternative,
                });
            }
        }

        // A forced skill whose static conditions produced no trigger on this
        // course/run must still fire: the user scripted "this skill activates
        // here", the same contract as injected debuffs. Synthesize a pending
        // entry from the first alternative that yields modeled effects.
        for skill in &skills {
            let base = skill.skill_id.base().to_owned();
            let Some(&pos) = self.forced_positions.get(&base) else {
                continue;
            };
            if forced_bypass_granted.contains(&base) {
                continue;
            }
            for alt in &skill.alternatives {
                let effects = build_skill_effects(alt);
                if effects.is_empty() {
                    continue;
                }
                forced_bypass_granted.insert(base);
                pending.push(PendingSkill {
                    skill_id: skill.skill_id.clone(),
                    rarity: skill.rarity,
                    tags: skill.tags.clone(),
                    trigger: Region::new(pos, pos + 10.0),
                    effects,
                    extra_condition: None,
                    target_strategy: derive_target_strategy(&alt.condition),
                    duration_scaling: alt.duration_scaling,
                    cooldown: 0.0,
                    later_triggers: Vec::new(),
                    ready_at: f64::NEG_INFINITY,
                    wit_passed: false,
                    forced: true,
                    precondition: None,
                    exclusive_alternative: None,
                });
                break;
            }
        }
        self.skills = skills;
        self.pending_skills = pending;

        self.initialize_targeted_skill_tracking(&eval_runner, ctx);
    }

    /// Port of `initializeTargetedSkillTracking`: resolve each injected debuff to
    /// its external-debuff effects and queue a fixed-position
    /// [`PendingTargetedSkill`]. Injected debuffs are pre-resolved [`Skill`]s
    /// (the data layer performs the service lookup upstream).
    fn initialize_targeted_skill_tracking(
        &mut self,
        eval_runner: &SkillEvalRunner,
        ctx: &PrepareContext<'_>,
    ) {
        if self.injected_debuffs.is_empty() {
            return;
        }
        let debuffs = std::mem::take(&mut self.injected_debuffs);
        for debuff in &debuffs {
            let triggers = build_skill_data(&BuildSkillDataParams {
                runner: eval_runner,
                race_params: ctx.race_params,
                course: ctx.course,
                whole_course: ctx.whole_course,
                parser: ctx.parser,
                skill: &debuff.skill,
                ignore_null_effects: false,
                resolution: ctx.condition_resolution,
            });
            for trigger in triggers {
                let external: Vec<SkillEffectSpec> = get_external_debuff_effects(&trigger.effects)
                    .into_iter()
                    .copied()
                    .collect();
                if external.is_empty() {
                    continue;
                }
                let policy = ActivationSamplePolicy::Fixed(debuff.position);
                let samples =
                    policy.sample(&trigger.regions, ctx.skill_samples, &mut *self.skill_rng);
                if samples.is_empty() {
                    continue;
                }
                let trigger_region = samples[ctx.round_iteration % samples.len()];
                self.pending_targeted_skills.push(PendingTargetedSkill {
                    skill_id: trigger.skill_id,
                    origin: TargetedSkillOrigin::Injection,
                    source_runner_id: None,
                    trigger: trigger_region,
                    effects: external,
                });
            }
        }
        self.injected_debuffs = debuffs;
    }

    fn skill_eval_runner(&self) -> SkillEvalRunner {
        SkillEvalRunner {
            base_stats: StatLine {
                speed: self.base_stats.speed.round() as i32,
                stamina: self.base_stats.stamina.round() as i32,
                power: self.base_stats.power.round() as i32,
                guts: self.base_stats.guts.round() as i32,
                wit: self.base_stats.wit.round() as i32,
            },
            strategy: self.strategy,
            mood: self.mood,
            popularity: self.popularity,
        }
    }

    /// Activate green (gate) skills at the start of the round.
    pub(crate) fn activate_gate_skills(&mut self, course_distance: f64) {
        let field = FieldView::at_gate();
        self.process_skill_activations(&field, course_distance);
    }

    /// Process self-skill activations for this tick.
    pub(crate) fn process_skill_activations(&mut self, field: &FieldView, course_distance: f64) {
        self.activations_at_last_tick_start = self.activations_at_tick_start;
        self.activations_at_tick_start = self.skills_activated_count;
        self.cleanup_expired_self_skills();
        self.activation_distance_from_top = field
            .leader_position
            .map_or(0.0, |leader| (leader - self.position).max(0.0));
        // An overtake this tick (order improved on the previous tick's; one per
        // tick however many runners were passed -- the doc does not say) acts on
        // skills already running, before any new activation this tick.
        if let (Some(previous), Some(current)) = (field.self_previous_order, field.self_order) {
            if current < previous {
                self.on_order_up();
            }
        }

        // Latch preconditions: each is checked wherever its static part holds,
        // whether or not the skill's own window has opened yet.
        let view = RunnerConditionView {
            runner: self,
            field,
        };
        let latched: Vec<usize> = self
            .pending_skills
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.precondition
                    .as_ref()
                    .is_some_and(|p| !p.met && p.covers(self.position) && p.check.eval(&view))
            })
            .map(|(at, _)| at)
            .collect();
        for at in latched {
            if let Some(p) = self.pending_skills[at].precondition.as_mut() {
                p.met = true;
            }
        }

        // Skills are checked in ascending id order (the pending list is built
        // from the id-sorted skills): a lower-id skill can trigger a higher-id
        // one on the same tick, not the reverse (mechanics doc §
        // activate_count_x; the recordings, 15 of 15 is_activate_any_skill
        // activations). A removal leaves `next` on the entry that moves up.
        let mut next = 0;
        while next < self.pending_skills.len() {
            let i = next;
            next += 1;
            let (trigger, skill_id, forced) = {
                let s = &self.pending_skills[i];
                (s.trigger, s.skill_id.0.clone(), s.forced)
            };

            let removal = self.pending_skill_removal.contains(&skill_id);
            if self.position >= trigger.end && !removal {
                // Passed this window: move on to the next placed trigger, if any
                // (all_corner_random), else the skill is done.
                let pending = &mut self.pending_skills[i];
                if !pending.later_triggers.is_empty() {
                    pending.trigger = pending.later_triggers.remove(0);
                    continue;
                }
            }
            if self.position >= trigger.end || removal {
                self.pending_skills.remove(i);
                self.pending_skill_removal.remove(&skill_id);
                next = i;
                continue;
            }

            // A skill's alternatives are pending in data order, so the first
            // that holds is checked first: it fires or spends the skill's wit
            // roll, and either way holds the later ones (the game fires the
            // first alternative that holds, once).
            if self.pending_holds(i, field) {
                let skip =
                    forced || self.pending_skills[i].wit_passed || self.should_skip_wit_check_at(i);
                let passed = skip || self.do_wit_check();
                let skill = self.pending_skills[i].clone();
                if passed {
                    let duration = self.activate_skill(&skill, course_distance);
                    // The cooldown starts on the tick the effect ends, both
                    // counted in whole ticks (the recordings; see below).
                    let ready_at = self.accumulate_time.t
                        + ready_ticks(duration, skill.cooldown, FRAME_DT) * FRAME_DT;
                    // One skill, one cooldown: its other alternatives wait it
                    // out too, and without a cooldown the skill is spent.
                    let until = if skill.cooldown > 0.0 {
                        ready_at
                    } else {
                        f64::INFINITY
                    };
                    self.hold_exclusive_alternatives(&skill, until, true);
                    // Mechanics doc § Skill Cooldown: a skill with a cooldown may
                    // activate again once it has elapsed, in its own window or at
                    // a later placed trigger (all_corner_random). The wit check
                    // is once per race. The doc does not say when the cooldown
                    // starts. On the 117 recordings all 43 repeats (201662,
                    // 201651, 200331, 200332, 200342) come after activation +
                    // duration + cooldown, none between activation + cooldown
                    // and that point, and 13 of the 27 near-lane repeats fire in
                    // a spell already running there: from its ready tick the
                    // skill fires on the first tick its condition holds, not on
                    // a fresh spell. The ready tick is counted in whole ticks
                    // (`ready_ticks`): the effect ends on the first tick its
                    // duration has run, and the cooldown counts whole ticks from
                    // that one. On the game's 0.0666 s tick, n1 the first
                    // firing's, that is n1 + ceil(duration / tick) +
                    // ceil(cooldown / tick), and none of the 43 comes before it.
                    // Three See Ya Later! (201662) repeats whose condition held
                    // before it fired exactly on it: 10908-r0039 runner 2
                    // (2600 m, 1290 = 118 + 1172 ticks after the first firing),
                    // 10611-r0077 runner 0 (1600 m, 794 = 73 + 721) and
                    // 10104-r0045 runner 2 (2000 m, 992 = 91 + 901): one tick
                    // after the tick on which duration + cooldown has elapsed,
                    // where a comparison of times fires. 10504-r0065 runner 6
                    // (2000 m) fired on it, 992 ticks after, whatever her
                    // condition did, so the ready tick comes no later. One
                    // other form fits all 43: the elapsed tick + 1, n1 +
                    // ceil((duration + cooldown) / tick) + 1. It comes a tick
                    // later where the two remainders sum past a tick, for the
                    // 3 s skills at 1700 m, the 1.8 s skills 200461 and 200462
                    // at 1400, 1700 and 1800 m, which never repeat on the
                    // recordings, and the 2.4 s corner skills (200331, 200332)
                    // at 1400 to 1800 m; no recorded repeat decides
                    // there (the one at 1700 m fired 8 ticks late, on its own
                    // timer). On the engine's 1/15 s tick the 30 s base
                    // cooldown is whole ticks on every course that is a
                    // multiple of 20 m, so there this count is the elapsed tick
                    // itself: under this form the recordings' extra tick is the
                    // game's tick rounding.
                    if skill.cooldown > 0.0 {
                        let same = |p: &PendingSkill| {
                            p.skill_id == skill.skill_id
                                && p.trigger == skill.trigger
                                && p.exclusive_alternative == skill.exclusive_alternative
                        };
                        let at = if self.pending_skills.get(i).is_some_and(same) {
                            Some(i)
                        } else {
                            self.pending_skills.iter().position(same)
                        };
                        if let Some(at) = at {
                            let pending = &mut self.pending_skills[at];
                            pending.ready_at = ready_at;
                            pending.wit_passed = true;
                        }
                        continue;
                    }
                } else {
                    // One wit check per skill, whichever alternative asked.
                    self.hold_exclusive_alternatives(&skill, f64::INFINITY, false);
                }
                if i < self.pending_skills.len() {
                    self.pending_skills.remove(i);
                    next = i;
                }
            }
        }
    }

    fn cleanup_expired_self_skills(&mut self) {
        for modifier in drain_expired(&mut self.target_speed_skills_active) {
            self.modifiers.target_speed.add(-modifier);
        }
        let mut one_frame = 0.0;
        let mut removed: Vec<(f64, bool)> = Vec::new();
        self.current_speed_skills_active.retain(|s| {
            if s.duration_timer.t >= 0.0 {
                removed.push((s.modifier, s.natural_deceleration));
                false
            } else {
                true
            }
        });
        for (modifier, natural) in removed {
            self.modifiers.current_speed.add(-modifier);
            if natural {
                one_frame += modifier;
            }
        }
        self.modifiers.one_frame_accel += one_frame;
        for modifier in drain_expired(&mut self.acceleration_skills_active) {
            self.modifiers.accel.add(-modifier);
        }
        self.lane_movement_skills_active
            .retain(|s| s.duration_timer.t < 0.0);
        self.change_lane_skills_active
            .retain(|s| s.duration_timer.t < 0.0);
        self.held_additional_effects.retain(|h| h.timer.t < 0.0);
        self.order_up_extensions.retain(|e| e.timer.t < 0.0);
    }

    /// An overtake: lengthen code-4 skills (mechanics doc § IncrementOrderUp,
    /// +1 s x distance / 1000 to every modifier the skill applied, up to 3
    /// times), then fire OrderUp additional activations.
    fn on_order_up(&mut self) {
        let mut extend: Vec<(SkillId, f64)> = Vec::new();
        for extension in &mut self.order_up_extensions {
            if extension.remaining > 0 && extension.timer.t < 0.0 {
                extension.remaining -= 1;
                extend.push((extension.skill_id.clone(), extension.seconds));
            }
        }
        for (skill_id, seconds) in extend {
            for list in [
                &mut self.target_speed_skills_active,
                &mut self.current_speed_skills_active,
                &mut self.acceleration_skills_active,
                &mut self.lane_movement_skills_active,
                &mut self.change_lane_skills_active,
            ] {
                for active in list.iter_mut().filter(|a| a.skill_id == skill_id) {
                    active.duration_timer.t -= seconds;
                }
            }
            for held in self
                .held_additional_effects
                .iter_mut()
                .filter(|h| h.skill.skill_id == skill_id)
            {
                held.timer.t -= seconds;
            }
            for extension in self
                .order_up_extensions
                .iter_mut()
                .filter(|e| e.skill_id == skill_id)
            {
                extension.timer.t -= seconds;
            }
        }
        self.fire_held_additional_effects(|held| held.trigger == 1);
    }

    /// Apply every held additional effect whose trigger matches, once each, for
    /// its skill's remaining duration (mechanics doc § Additional Activate).
    fn fire_held_additional_effects(&mut self, matches: impl Fn(&HeldAdditionalEffect) -> bool) {
        let mut fired: Vec<(PendingSkill, SkillEffectSpec, f64)> = Vec::new();
        for held in &mut self.held_additional_effects {
            if held.remaining > 0 && held.timer.t < 0.0 && matches(held) {
                held.remaining -= 1;
                fired.push((held.skill.clone(), held.spec, -held.timer.t));
            }
        }
        for (skill, spec, remaining) in fired {
            let resolved = self.resolve_effect(skill.skill_id.base(), &spec);
            if is_emittable_external_effect(&resolved) {
                continue; // additional activations in the data are self-buffs
            }
            self.apply_self_effect(&skill, &resolved, remaining);
        }
    }

    /// Whether pending skill `idx` may activate this tick: inside its window,
    /// cooled down, and (unless forced) its precondition latched and its
    /// condition holding.
    fn pending_holds(&self, idx: usize, field: &FieldView) -> bool {
        let skill = &self.pending_skills[idx];
        self.position >= skill.trigger.start
            && self.position < skill.trigger.end
            && self.tick_reached(skill.ready_at)
            && (skill.forced
                || (DynamicPrecondition::is_met(skill.precondition.as_ref())
                    && self.pending_extra_passes(idx, field)))
    }

    /// Whether this tick is the one at race time `at` or later. `at` is a tick
    /// of the runner's clock (or infinite), and the two are compared in whole
    /// ticks of [`FRAME_DT`], so float rounding in either sum cannot move the
    /// answer by a tick.
    fn tick_reached(&self, at: f64) -> bool {
        (self.accumulate_time.t / FRAME_DT).round() >= (at / FRAME_DT).round()
    }

    /// Hold the skill's other exclusive alternatives until race time `until`
    /// (`INFINITY`: for the rest of the race), their wit check passed if
    /// `wit_passed`.
    fn hold_exclusive_alternatives(&mut self, skill: &PendingSkill, until: f64, wit_passed: bool) {
        let Some(k) = skill.exclusive_alternative else {
            return;
        };
        for other in &mut self.pending_skills {
            if other.skill_id == skill.skill_id
                && other.exclusive_alternative.is_some_and(|m| m != k)
            {
                other.ready_at = other.ready_at.max(until);
                other.wit_passed |= wit_passed;
            }
        }
    }

    fn pending_extra_passes(&self, idx: usize, field: &FieldView) -> bool {
        let skill = &self.pending_skills[idx];
        let view = RunnerConditionView {
            runner: self,
            field,
        };
        eval_dynamic(&skill.extra_condition, &view)
    }

    fn should_skip_wit_check_at(&self, idx: usize) -> bool {
        self.should_skip_wit_check(&self.pending_skills[idx])
    }

    fn should_skip_wit_check(&self, skill: &PendingSkill) -> bool {
        if !self.wit_checks_enabled {
            return true;
        }
        if let Some(first) = skill.effects.first() {
            let type_id = first.effect_type as i32;
            if (1..=6).contains(&type_id) {
                return true;
            }
        }
        skill.rarity == SkillRarity::Unique
    }

    fn do_wit_check(&mut self) -> bool {
        let wit = self.base_stats.wit;
        let roll = self.wit_rng.random();
        let threshold = crate::readouts::skill_activation_chance(wit);
        roll <= threshold
    }

    /// Resolve one effect spec into a concrete [`ResolvedSkillEffect`] against
    /// this runner's state, applying its value-scaling policy exactly once. The
    /// Recovery drain override is a Recovery-specific pre-step that consumes no
    /// RNG roll (see [`resolve_effect_modifier`]).
    fn resolve_effect(
        &mut self,
        base_skill_id: &str,
        spec: &SkillEffectSpec,
    ) -> ResolvedSkillEffect {
        let override_value = self.stamina_drain_overrides.get(base_skill_id).copied();
        let activated_green_count = self.activated_ledger.activated_green_count();
        let modifier = resolve_effect_modifier(
            spec,
            Some(&mut *self.skill_rng),
            override_value,
            activated_green_count,
        )
        .unwrap_or(0.0);
        ResolvedSkillEffect {
            target: spec.target,
            effect_type: spec.effect_type,
            base_duration: spec.base_duration,
            modifier,
        }
    }

    /// Apply the skill's effects; returns how long the skill runs, in seconds
    /// (its longest effect as applied; 0 when every effect is instant).
    ///
    /// The cooldown starts when that duration ends. Which duration the game
    /// counts is not determined where a skill's effects differ: the longest
    /// here includes held additional-activation effects, which may never
    /// fire, and effects routed to other runners, and an overtake that
    /// lengthens a code-4 skill (`on_order_up`) does not move `ready_at`. The
    /// recorded repeats cover only plain self-buffs: in the skill data the
    /// only cooldown that comes back within a race is the 30 s base one (20
    /// skills, 16 of them carried on the 117 recordings), and every one of
    /// those applies its effects to the runner herself, none held, none with
    /// a duration scaling.
    fn activate_skill(&mut self, skill: &PendingSkill, course_distance: f64) -> f64 {
        let mut specs = skill.effects.clone();
        specs.sort_by_key(|e| i32::from(e.effect_type as i32 == 42));
        let base_skill_id = skill.skill_id.base().to_owned();

        let time_scaling = duration_scaling_multiplier(
            skill.duration_scaling,
            self.health_policy.current_health(),
            self.activation_distance_from_top,
        );
        let mut skill_duration: f64 = 0.0;
        for spec in &specs {
            let scaling = if skill.rarity == SkillRarity::Evolution {
                self.modifiers.special_skill_duration_scaling
            } else {
                1.0
            };
            let scaled_duration =
                spec.base_duration * (course_distance / 1000.0) * scaling * time_scaling;
            skill_duration = skill_duration.max(scaled_duration);

            // An additional-activation effect does nothing now: it waits on its
            // trigger for as long as the skill runs.
            if let Some(trigger @ 1..=3) = spec.additional_activate_type {
                self.held_additional_effects.push(HeldAdditionalEffect {
                    skill: skill.clone(),
                    spec: *spec,
                    trigger,
                    remaining: HeldAdditionalEffect::limit(trigger),
                    timer: Timer::new(-scaled_duration),
                });
                continue;
            }

            // Resolve the effect's value-scaling policy exactly once against the
            // caster's state, before self-application or external-debuff routing.
            let resolved = self.resolve_effect(&base_skill_id, spec);

            // External debuffs target other runners (e.g. Wild Wind / Speed
            // Eater bundle a self-buff with an opponent-facing Current Speed
            // debuff; the Hesitant family debuffs a whole enemy strategy). They
            // must never land on the caster: emit the already-resolved effect to
            // the per-frame outbox so the race aggregate's
            // `coordinate_external_debuffs` pass routes it onto the target
            // runners via `receive_targeted_effect`. The caster resolves the
            // value here so the receiver never re-resolves it.
            if is_emittable_external_effect(&resolved) {
                self.emitted_debuffs.push(EmittedDebuff {
                    skill_id: skill.skill_id.clone(),
                    effect: resolved,
                    target: resolved.target,
                    target_strategy: skill.target_strategy,
                });
                continue;
            }
            // Record positive self-buffs (resolved value) so opponents can react
            // via is_other_character_activate_advantage_skill (arg = SkillType).
            if resolved.modifier > 0.0 {
                let t = resolved.effect_type as i64;
                if (0..64).contains(&t) {
                    self.activated_advantage_effect_types |= 1u64 << t;
                }
            }
            // ActivateRandomGold fires other skills here, where the course
            // distance is known: through apply_self_effect it was handed this
            // effect's own duration in the distance's place, so every gold it
            // forced ran for base x duration / 1000 -- about one tick.
            if resolved.effect_type == SkillType::ActivateRandomGold {
                self.activate_random_gold_skill(resolved.modifier as usize, course_distance);
                continue;
            }
            self.apply_self_effect(skill, &resolved, scaled_duration);
        }
        if skill.duration_scaling == Some(4) && skill_duration > 0.0 {
            self.order_up_extensions.push(OrderUpExtension {
                skill_id: skill.skill_id.clone(),
                remaining: 3,
                seconds: course_distance / 1000.0,
                timer: Timer::new(-skill_duration),
            });
        }

        let half_race = usize::from(self.position >= course_distance / 2.0);
        self.skills_activated_half_race_map[half_race] += 1;
        self.skills_activated_phase_map[self.phase.index()] += 1;
        self.skills_activated_count += 1;
        self.used_skills.insert(skill.skill_id.0.clone());
        // Record only after a successful activation so caster-context scaling
        // (usage 14) counts greens that actually fired this round.
        self.activated_ledger.record(&base_skill_id, &skill.tags);
        // ActivateAnySkill: running skills' held effects fire on this one.
        let activated = skill.skill_id.clone();
        self.fire_held_additional_effects(|held| {
            (held.trigger == 2 || held.trigger == 3) && held.skill.skill_id != activated
        });
        skill_duration
    }

    fn apply_self_effect(
        &mut self,
        skill: &PendingSkill,
        effect: &ResolvedSkillEffect,
        duration: f64,
    ) {
        match effect.effect_type {
            SkillType::Noop => {}
            SkillType::SpeedUp => {
                self.adjusted_stats.speed = (self.adjusted_stats.speed + effect.modifier).max(1.0);
            }
            SkillType::StaminaUp => {
                self.adjusted_stats.stamina =
                    (self.adjusted_stats.stamina + effect.modifier).max(1.0);
                self.base_stats.stamina = (self.base_stats.stamina + effect.modifier).max(1.0);
            }
            SkillType::PowerUp => {
                self.adjusted_stats.power = (self.adjusted_stats.power + effect.modifier).max(1.0);
            }
            SkillType::GutsUp => {
                self.adjusted_stats.guts = (self.adjusted_stats.guts + effect.modifier).max(1.0);
            }
            SkillType::WisdomUp => {
                self.adjusted_stats.wit = (self.adjusted_stats.wit + effect.modifier).max(1.0);
            }
            SkillType::ChangeStrategy => self.position_keep_strategy = Strategy::Runaway,
            // Read pre-race off the pending queue by `rushed_chance` (the rushed
            // roll runs before gate skills fire), so activation is a no-op.
            SkillType::RushedChance => {}
            // Frenzied (type 13) only ever targets opponents (KakariStrategy);
            // a self-application is a no-op.
            SkillType::RushedDuration => {}
            SkillType::MultiplyStartDelay => self.start_delay *= effect.modifier,
            SkillType::SetStartDelay => self.start_delay = effect.modifier,
            SkillType::TargetSpeed => {
                self.modifiers.target_speed.add(effect.modifier);
                self.target_speed_skills_active
                    .push(active_skill(skill, effect, duration, false));
            }
            SkillType::Accel => {
                self.modifiers.accel.add(effect.modifier);
                self.acceleration_skills_active
                    .push(active_skill(skill, effect, duration, false));
            }
            SkillType::LaneMovementSpeed => {
                self.lane_movement_skills_active
                    .push(active_skill(skill, effect, duration, false));
            }
            SkillType::CurrentSpeed | SkillType::CurrentSpeedWithNaturalDeceleration => {
                self.modifiers.current_speed.add(effect.modifier);
                let natural = effect.effect_type == SkillType::CurrentSpeedWithNaturalDeceleration;
                self.current_speed_skills_active
                    .push(active_skill(skill, effect, duration, natural));
            }
            SkillType::Recovery => {
                if effect.modifier > 0.0 {
                    self.heals_activated_count += 1;
                }
                self.health_policy.recover(effect.modifier);
                if self.phase.index() >= 2 && !self.is_last_spurt {
                    self.force_last_spurt_check();
                }
            }
            // Handled in activate_skill, which has the course distance.
            SkillType::ActivateRandomGold => {}
            SkillType::ExtendEvolvedDuration => {
                self.modifiers.special_skill_duration_scaling = effect.modifier;
            }
            SkillType::ChangeLane => {
                self.change_lane_skills_active
                    .push(active_skill(skill, effect, duration, false));
            }
        }
    }

    fn activate_random_gold_skill(&mut self, count: usize, course_distance: f64) {
        // A skill still cooling down after firing (kept pending for a later
        // trigger, patch 0019) is not a candidate: it has already activated.
        let mut gold_indices: Vec<usize> = self
            .pending_skills
            .iter()
            .enumerate()
            .filter(|(_, skill)| {
                let gold = matches!(skill.rarity, SkillRarity::Gold | SkillRarity::Evolution);
                gold && self.tick_reached(skill.ready_at)
                    && skill.effects.iter().all(|e| (e.effect_type as i32) > 5)
            })
            .map(|(idx, _)| idx)
            .collect();
        // A skill is one candidate however many of its entries are pending,
        // exclusive alternatives or a second stage naming
        // is_activate_other_skill_detail: the first stands for it, as for a
        // scripted position (which alternative the game applies here is not
        // determined).
        let mut seen: HashSet<&str> = HashSet::new();
        gold_indices.retain(|&idx| seen.insert(self.pending_skills[idx].skill_id.as_str()));

        let mut i = gold_indices.len();
        while i > 0 {
            i -= 1;
            let j = self.force_skill_activator_rng.uniform(i as u32 + 1) as usize;
            gold_indices.swap(i, j);
        }

        for &idx in gold_indices.iter().take(count) {
            let skill = self.pending_skills[idx].clone();
            self.activate_skill(&skill, course_distance);
            // The gold is spent, every alternative of it: the removal below
            // drops only the first entry the pass meets, so hold them all for
            // the race (nor are they candidates for another forced gold).
            for pending in &mut self.pending_skills {
                if pending.skill_id == skill.skill_id {
                    pending.ready_at = f64::INFINITY;
                }
            }
            self.pending_skill_removal.insert(skill.skill_id.0.clone());
        }
    }

    /// Process targeted (injected / cross-runner) skill activations this tick.
    pub(crate) fn process_targeted_skill_activations(&mut self, course_distance: f64) {
        self.cleanup_expired_targeted_skills();

        let mut i = self.pending_targeted_skills.len();
        while i > 0 {
            i -= 1;
            if i >= self.pending_targeted_skills.len() {
                continue;
            }
            let trigger = self.pending_targeted_skills[i].trigger;
            if self.position >= trigger.end {
                self.pending_targeted_skills.remove(i);
                continue;
            }
            if self.position >= trigger.start {
                let skill = self.pending_targeted_skills[i].clone();
                self.apply_targeted_effect(&skill, course_distance);
                self.pending_targeted_skills.remove(i);
            }
        }
    }

    fn cleanup_expired_targeted_skills(&mut self) {
        for modifier in drain_expired_targeted(&mut self.targeted_target_speed_active) {
            self.modifiers.target_speed.add(-modifier);
        }
        let mut one_frame = 0.0;
        let mut removed: Vec<(f64, bool)> = Vec::new();
        self.targeted_current_speed_active.retain(|s| {
            if s.skill.duration_timer.t >= 0.0 {
                removed.push((s.skill.modifier, s.skill.natural_deceleration));
                false
            } else {
                true
            }
        });
        for (modifier, natural) in removed {
            self.modifiers.current_speed.add(-modifier);
            if natural {
                one_frame += modifier;
            }
        }
        self.modifiers.one_frame_accel += one_frame;
        for modifier in drain_expired_targeted(&mut self.targeted_acceleration_active) {
            self.modifiers.accel.add(-modifier);
        }
        self.targeted_lane_movement_skills_active
            .retain(|s| s.skill.duration_timer.t < 0.0);
        self.targeted_change_lane_skills_active
            .retain(|s| s.skill.duration_timer.t < 0.0);
    }

    /// Apply an **injected** targeted skill (from the debuff test harness, which
    /// has no caster). Injected effects are unresolved specs, so they are
    /// resolved receiver-locally here. Only caster-context policies (usage 14)
    /// are unsafe without a caster, and those are rejected at the injection DTO
    /// boundary before the race.
    fn apply_targeted_effect(&mut self, skill: &PendingTargetedSkill, course_distance: f64) {
        let mut specs = skill.effects.clone();
        specs.sort_by_key(|e| i32::from(e.effect_type as i32 == 42));
        let base_skill_id = skill.skill_id.base().to_owned();
        let meta = TargetedEffectMeta {
            skill_id: skill.skill_id.clone(),
            origin: skill.origin,
            source_runner_id: skill.source_runner_id,
        };

        for spec in &specs {
            let resolved = self.resolve_effect(&base_skill_id, spec);
            self.apply_resolved_targeted_effect(&meta, &resolved, course_distance);
        }
    }

    /// Record and apply one already-resolved targeted effect. Shared by the
    /// injected path (which resolves receiver-locally) and the cross-runner path
    /// (which receives values already resolved by the caster).
    fn apply_resolved_targeted_effect(
        &mut self,
        meta: &TargetedEffectMeta,
        effect: &ResolvedSkillEffect,
        course_distance: f64,
    ) {
        let scaled_duration = effect.base_duration * (course_distance / 1000.0);
        self.used_targeted_skills.push(UsedTargetedSkill {
            skill_id: meta.skill_id.clone(),
            position: self.position,
            effect_type: effect.effect_type,
            effect_target: effect.target,
        });
        self.apply_targeted_effect_kind(meta, effect, scaled_duration);
    }

    fn apply_targeted_effect_kind(
        &mut self,
        meta: &TargetedEffectMeta,
        effect: &ResolvedSkillEffect,
        duration: f64,
    ) {
        match effect.effect_type {
            SkillType::Noop | SkillType::ChangeStrategy | SkillType::RushedChance => {}
            // Frenzied family: worsen a rushed opponent by extending its
            // remaining rushed duration (modifier is +5.0s, already scaled).
            // Only meaningful while the target is currently rushed; the 12s
            // cap-based exit is delayed by the added time.
            SkillType::RushedDuration => {
                if self.is_rushed {
                    self.rushed_max_duration += effect.modifier;
                }
            }
            SkillType::SpeedUp => {
                self.adjusted_stats.speed = (self.adjusted_stats.speed + effect.modifier).max(1.0);
            }
            SkillType::StaminaUp => {
                self.adjusted_stats.stamina =
                    (self.adjusted_stats.stamina + effect.modifier).max(1.0);
                self.base_stats.stamina = (self.base_stats.stamina + effect.modifier).max(1.0);
            }
            SkillType::PowerUp => {
                self.adjusted_stats.power = (self.adjusted_stats.power + effect.modifier).max(1.0);
            }
            SkillType::GutsUp => {
                self.adjusted_stats.guts = (self.adjusted_stats.guts + effect.modifier).max(1.0);
            }
            SkillType::WisdomUp => {
                self.adjusted_stats.wit = (self.adjusted_stats.wit + effect.modifier).max(1.0);
            }
            SkillType::MultiplyStartDelay => self.start_delay *= effect.modifier,
            SkillType::SetStartDelay => self.start_delay = effect.modifier,
            SkillType::TargetSpeed => {
                self.modifiers.target_speed.add(effect.modifier);
                self.targeted_target_speed_active
                    .push(active_targeted(meta, effect, duration, false));
            }
            SkillType::Accel => {
                self.modifiers.accel.add(effect.modifier);
                self.targeted_acceleration_active
                    .push(active_targeted(meta, effect, duration, false));
            }
            SkillType::LaneMovementSpeed => {
                self.targeted_lane_movement_skills_active
                    .push(active_targeted(meta, effect, duration, false));
            }
            SkillType::CurrentSpeed | SkillType::CurrentSpeedWithNaturalDeceleration => {
                self.modifiers.current_speed.add(effect.modifier);
                let natural = effect.effect_type == SkillType::CurrentSpeedWithNaturalDeceleration;
                self.targeted_current_speed_active
                    .push(active_targeted(meta, effect, duration, natural));
            }
            SkillType::Recovery => {
                self.health_policy.recover(effect.modifier);
                if self.phase.index() >= 2 && !self.is_last_spurt {
                    self.force_last_spurt_check();
                }
            }
            SkillType::ActivateRandomGold | SkillType::ExtendEvolvedDuration => {}
            SkillType::ChangeLane => {
                self.targeted_change_lane_skills_active
                    .push(active_targeted(meta, effect, duration, false));
            }
        }
    }

    /// Entry point for a cross-runner targeted effect (routed by the aggregate).
    ///
    /// The effects arrive **already resolved by the caster** (see
    /// `Self::activate_skill`); the receiver must not re-resolve them, which is
    /// enforced by the [`ResolvedSkillEffect`] type.
    pub fn receive_targeted_effect(
        &mut self,
        skill_id: SkillId,
        effects: Vec<ResolvedSkillEffect>,
        source_runner_id: crate::shared_kernel::ids::RunnerId,
        course_distance: f64,
    ) {
        let meta = TargetedEffectMeta {
            skill_id,
            origin: TargetedSkillOrigin::Runner,
            source_runner_id: Some(source_runner_id),
        };
        let mut effects = effects;
        effects.sort_by_key(|e| i32::from(e.effect_type as i32 == 42));
        for resolved in &effects {
            self.apply_resolved_targeted_effect(&meta, resolved, course_distance);
        }
    }
}

/// Identity of a targeted-effect application, shared by the injected and
/// cross-runner paths so the per-effect application logic is written once.
struct TargetedEffectMeta {
    skill_id: SkillId,
    origin: TargetedSkillOrigin,
    source_runner_id: Option<crate::shared_kernel::ids::RunnerId>,
}

fn active_skill(
    skill: &PendingSkill,
    effect: &ResolvedSkillEffect,
    duration: f64,
    natural_deceleration: bool,
) -> ActiveSkill {
    ActiveSkill {
        skill_id: skill.skill_id.clone(),
        duration_timer: Timer::new(-duration),
        modifier: effect.modifier,
        effect_target: effect.target,
        effect_type: effect.effect_type,
        natural_deceleration,
    }
}

fn active_targeted(
    meta: &TargetedEffectMeta,
    effect: &ResolvedSkillEffect,
    duration: f64,
    natural_deceleration: bool,
) -> ActiveTargetedSkill {
    ActiveTargetedSkill {
        skill: ActiveSkill {
            skill_id: meta.skill_id.clone(),
            duration_timer: Timer::new(-duration),
            modifier: effect.modifier,
            effect_target: effect.target,
            effect_type: effect.effect_type,
            natural_deceleration,
        },
        origin: meta.origin,
        source_runner_id: meta.source_runner_id,
    }
}

/// Ticks of `tick` seconds from a skill's activation to the first tick it may
/// activate again: its effect ends on the first whole tick its `duration` has
/// run, and the `cooldown` counts whole ticks from that one.
fn ready_ticks(duration: f64, cooldown: f64, tick: f64) -> f64 {
    // A span of whole ticks up to float rounding (3.0 x 2600 / 1000 s is
    // 7.800000000000001, 117 ticks of 1/15 s) is that many ticks, not one more.
    let whole = |seconds: f64| (seconds / tick - 1e-9).ceil();
    whole(duration) + whole(cooldown)
}

/// Drain expired (timer ≥ 0) self active skills, returning their modifiers so the
/// caller can reverse each on the runner's Kahan accumulator.
fn drain_expired(skills: &mut Vec<ActiveSkill>) -> Vec<f64> {
    let mut removed = Vec::new();
    skills.retain(|s| {
        if s.duration_timer.t >= 0.0 {
            removed.push(s.modifier);
            false
        } else {
            true
        }
    });
    removed
}

/// Drain expired targeted active skills, returning their modifiers.
fn drain_expired_targeted(skills: &mut Vec<ActiveTargetedSkill>) -> Vec<f64> {
    let mut removed = Vec::new();
    skills.retain(|s| {
        if s.skill.duration_timer.t >= 0.0 {
            removed.push(s.skill.modifier);
            false
        } else {
            true
        }
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::lifecycle::{CreateRunner, RunnerAptitudes};
    use crate::runner::test_support::{test_course, test_race_params, test_whole_course};
    use crate::shared_kernel::ids::{RunnerId, SkillId};
    use crate::shared_kernel::language::{Aptitude, GroundCondition, Mood, Strategy};
    use crate::shared_kernel::params::StatLine;
    use crate::shared_kernel::rng::{Prng, Xoshiro256StarStar};
    use crate::skills::condition::catalog::build_catalog;
    use crate::skills::condition::language::ConditionParser;
    use crate::skills::effect::{SkillRarity, SkillTarget, SkillType};
    use crate::skills::model::{RawSkillEffect, Skill, SkillAlternative};
    use crate::stamina::policy::NoopStaminaPolicy;
    use std::collections::HashMap;

    fn eval_runner() -> SkillEvalRunner {
        SkillEvalRunner {
            base_stats: StatLine {
                speed: 1000,
                stamina: 1000,
                power: 1000,
                guts: 1000,
                wit: 800,
            },
            strategy: Strategy::PaceChaser,
            mood: Mood::Normal,
            popularity: 0,
        }
    }

    fn target_speed_skill(id: &str, rarity: SkillRarity, condition: &str) -> Skill {
        Skill {
            skill_id: SkillId::new(id),
            rarity,
            tags: vec![],
            alternatives: vec![SkillAlternative {
                base_duration: 30000.0,
                cooldown_time: None,
                duration_scaling: None,
                condition: condition.to_owned(),
                precondition: None,
                effects: vec![RawSkillEffect {
                    modifier: 4500.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 27, // TargetSpeed
                    value_usage: None,
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        }
    }

    /// A Savvy-shaped skill: Wisdom Up (type 5, modeled) bundled with a vision effect (type 8, unmodeled), gated on the Pace Chaser running style — the exact shape of real skill 201531 (Pace Chaser Savvy ◎).
    fn savvy_skill(id: &str) -> Skill {
        Skill {
            skill_id: SkillId::new(id),
            rarity: SkillRarity::White,
            tags: vec![],
            alternatives: vec![SkillAlternative {
                base_duration: -10000.0,
                cooldown_time: None,
                duration_scaling: None,
                condition: "running_style==2".to_owned(),
                precondition: None,
                effects: vec![
                    RawSkillEffect {
                        modifier: 600000.0,
                        target: SkillTarget::SelfTarget,
                        effect_type: 5, // Wisdom Up (modeled)
                        value_usage: Some(1),
                        value_level_usage: Some(1),
                        pre_applied_multiplier: None,
                        additional_activate_type: None,
                    },
                    RawSkillEffect {
                        modifier: 100000.0,
                        target: SkillTarget::SelfTarget,
                        effect_type: 8, // vision (unmodeled)
                        value_usage: Some(1),
                        value_level_usage: Some(1),
                        pre_applied_multiplier: None,
                        additional_activate_type: None,
                    },
                ],
            }],
        }
    }

    fn runaway_skill(condition: &str) -> Skill {
        Skill {
            skill_id: SkillId::new("202051"),
            rarity: SkillRarity::Gold,
            tags: vec![101, 612],
            alternatives: vec![SkillAlternative {
                base_duration: -1.0,
                cooldown_time: Some(0.0),
                duration_scaling: None,
                condition: condition.to_owned(),
                precondition: Some(String::new()),
                effects: vec![RawSkillEffect {
                    modifier: 0.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 6,
                    value_usage: Some(1),
                    value_level_usage: Some(1),
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        }
    }

    fn build(skill: &Skill) -> Vec<SkillTrigger> {
        let course = test_course();
        let catalog = build_catalog();
        let parser = ConditionParser::new(&catalog);
        let rp = test_race_params();
        let wc = test_whole_course(&course);
        let eval = eval_runner();
        build_skill_data(&BuildSkillDataParams {
            runner: &eval,
            race_params: &rp,
            course: &course,
            whole_course: &wc,
            parser: &parser,
            skill,
            ignore_null_effects: false,
            resolution: ConditionResolution::Dynamic,
        })
    }

    #[test]
    fn build_skill_data_produces_trigger_for_phase_condition() {
        let skill = target_speed_skill("100001", SkillRarity::Gold, "phase>=2");
        let triggers = build(&skill);
        assert_eq!(triggers.len(), 1);
        assert!(!triggers[0].regions.0.is_empty());
        assert_eq!(triggers[0].effects[0].effect_type, SkillType::TargetSpeed);
        assert!(triggers[0].regions.0[0].start >= 1200.0);
    }

    #[test]
    fn empty_precondition_is_treated_as_none_and_still_activates() {
        // Regression (ADR-0004 Option-B bug #2): skills whose data carries
        // `precondition: ""` (e.g. all_corner_random / rotation greens) must treat
        // the empty string as "no precondition" and still produce a trigger — not
        // try to parse the empty string, fail, and silently never activate.
        let none_pre = target_speed_skill("200012", SkillRarity::Gold, "phase>=1");
        assert_eq!(
            build(&none_pre).len(),
            1,
            "baseline: no precondition activates"
        );

        let mut empty_pre = target_speed_skill("200012", SkillRarity::Gold, "phase>=1");
        empty_pre.alternatives[0].precondition = Some(String::new());
        let triggers = build(&empty_pre);
        assert_eq!(
            triggers.len(),
            1,
            "empty precondition must behave like no precondition, not suppress the trigger"
        );
        assert!(!triggers[0].regions.0.is_empty());
    }

    #[test]
    fn a_precondition_keeps_its_runtime_half() {
        // Certain Victory's shape: a static precondition part (phase) and a
        // dynamic one (order). The static part narrows the window as before;
        // the dynamic part is carried, unmet, for the race to latch.
        let mut skill = target_speed_skill("910031", SkillRarity::Gold, "phase>=2");
        skill.alternatives[0].precondition = Some("phase>=1&order<=5".to_owned());
        let triggers = build(&skill);
        assert_eq!(triggers.len(), 1);
        let pre = triggers[0]
            .precondition
            .as_ref()
            .expect("the order check is carried");
        assert!(!pre.met);
        assert!(!DynamicPrecondition::is_met(Some(pre)));
        assert!(DynamicPrecondition::is_met(None));

        // A purely static precondition carries nothing to latch.
        skill.alternatives[0].precondition = Some("phase>=1".to_owned());
        assert!(build(&skill)[0].precondition.is_none());
    }

    #[test]
    fn build_skill_data_keeps_full_regions_for_dynamic_condition() {
        // `is_lastspurt` does not narrow regions; it yields the whole course plus
        // a runtime gate (extra_condition).
        let skill = target_speed_skill("100002", SkillRarity::Gold, "is_lastspurt==1");
        let triggers = build(&skill);
        assert_eq!(triggers.len(), 1);
        assert!(!triggers[0].regions.0.is_empty());
        // Last-spurt portion of the course (>= half distance).
        assert!(triggers[0].regions.0[0].start >= 1200.0);
    }

    fn runner_with_skills(skills: Vec<Skill>) -> Runner {
        runner_with_skills_forced(skills, HashMap::new())
    }

    fn runner_with_skills_forced(
        skills: Vec<Skill>,
        forced_positions: HashMap<String, f64>,
    ) -> Runner {
        let props = CreateRunner {
            outfit_id: "100302".to_owned(),
            name: "Test".to_owned(),
            mood: Mood::Normal,
            strategy: Strategy::PaceChaser,
            popularity: 0,
            team: None,
            aptitudes: RunnerAptitudes {
                distance: Aptitude::A,
                strategy: Aptitude::A,
                surface: Aptitude::A,
            },
            stats: StatLine {
                speed: 1000,
                stamina: 1000,
                power: 1000,
                guts: 1000,
                wit: 800,
            },
            skills,
            forced_positions,
            injected_debuffs: vec![],
            forced_rushed_regions: vec![],
            forced_dueling_regions: vec![],
            forced_spot_struggle_regions: vec![],
            forced_downhill_regions: vec![],
            forced_rank: vec![],
            gate: None,
            forced_start_delay: None,
            forced_last_spurt_distance: None,
        };
        Runner::create(
            RunnerId(0),
            &test_course(),
            GroundCondition::Firm,
            props,
            Box::new(NoopStaminaPolicy),
            Box::new(Xoshiro256StarStar::from_u32_seed(1)),
        )
    }

    fn prepare(r: &mut Runner) {
        prepare_on(r, test_course().distance);
    }

    /// `prepare` on a course of `distance` metres (cooldowns scale with it).
    fn prepare_on(r: &mut Runner, distance: f64) {
        let mut course = test_course();
        course.distance = distance;
        let catalog = build_catalog();
        let parser = ConditionParser::new(&catalog);
        let rp = test_race_params();
        let wc = test_whole_course(&course);
        let ctx = PrepareContext {
            course: &course,
            base_speed: 19.6,
            condition_resolution: ConditionResolution::Dynamic,
            pos_keep_end_multiplier: 3.0,
            race_params: &rp,
            whole_course: &wc,
            parser: &parser,
            skill_samples: 4,
            round_iteration: 0,
        };
        r.on_prepare(Box::new(Xoshiro256StarStar::from_u64_seed(7)), &ctx);
    }

    /// The Copano Rickey (109801) matchup from the saved contested-compare
    /// scenario: `100981` (Luck Runs My Way, usage-14) plus three greens
    /// — Pace Chaser Savvy ○ (201532, tag 612), Collaborative Graded Races ○
    /// (202252, tag 606), Wet Conditions ○ (200162, tag 601).
    fn copano_rickey_full_skills() -> Vec<Skill> {
        let green = |id: &str, tags: Vec<i32>, cond: &str, effect_type: i32| Skill {
            skill_id: SkillId::new(id),
            rarity: SkillRarity::White,
            tags,
            alternatives: vec![SkillAlternative {
                base_duration: -10000.0,
                cooldown_time: Some(0.0),
                duration_scaling: None,
                condition: cond.to_owned(),
                precondition: Some(String::new()),
                effects: vec![RawSkillEffect {
                    modifier: 400000.0,
                    target: SkillTarget::SelfTarget,
                    effect_type,
                    value_usage: Some(1),
                    value_level_usage: Some(1),
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        };
        vec![
            // 100981 Luck Runs My Way: Direct Target Speed + usage-14 Target
            // Speed + usage-14 Acceleration.
            Skill {
                skill_id: SkillId::new("100981"),
                rarity: SkillRarity::Unique,
                tags: vec![401, 403],
                alternatives: vec![SkillAlternative {
                    base_duration: 50000.0,
                    cooldown_time: None,
                    duration_scaling: None,
                    condition: "phase_laterhalf_random==1".to_owned(),
                    precondition: Some(String::new()),
                    effects: vec![
                        RawSkillEffect {
                            modifier: 2500.0,
                            target: SkillTarget::SelfTarget,
                            effect_type: 27,
                            value_usage: Some(1),
                            value_level_usage: None,
                            pre_applied_multiplier: None,
                            additional_activate_type: None,
                        },
                        RawSkillEffect {
                            modifier: 500.0,
                            target: SkillTarget::SelfTarget,
                            effect_type: 27,
                            value_usage: Some(14),
                            value_level_usage: None,
                            pre_applied_multiplier: None,
                            additional_activate_type: None,
                        },
                        RawSkillEffect {
                            modifier: 500.0,
                            target: SkillTarget::SelfTarget,
                            effect_type: 31,
                            value_usage: Some(14),
                            value_level_usage: None,
                            pre_applied_multiplier: None,
                            additional_activate_type: None,
                        },
                    ],
                }],
            },
            // 201532 Pace Chaser Savvy ○: Wisdom Up (green) + vision (unmodeled).
            Skill {
                skill_id: SkillId::new("201532"),
                rarity: SkillRarity::White,
                tags: vec![102, 405, 612],
                alternatives: vec![SkillAlternative {
                    base_duration: -10000.0,
                    cooldown_time: Some(0.0),
                    duration_scaling: None,
                    condition: "running_style==2".to_owned(),
                    precondition: Some(String::new()),
                    effects: vec![
                        RawSkillEffect {
                            modifier: 400000.0,
                            target: SkillTarget::SelfTarget,
                            effect_type: 5,
                            value_usage: Some(1),
                            value_level_usage: Some(1),
                            pre_applied_multiplier: None,
                            additional_activate_type: None,
                        },
                        RawSkillEffect {
                            modifier: 50000.0,
                            target: SkillTarget::SelfTarget,
                            effect_type: 8,
                            value_usage: Some(1),
                            value_level_usage: Some(1),
                            pre_applied_multiplier: None,
                            additional_activate_type: None,
                        },
                    ],
                }],
            },
            green("202252", vec![401, 606], "is_dirtgrade==1", 1),
            green(
                "200162",
                vec![403, 601],
                "ground_condition==2@ground_condition==3@ground_condition==4",
                3,
            ),
        ]
    }

    /// The three extra greens from the second saved scenario, all
    /// gate-deterministic on this config: Fall Runner ○ (200192/603,
    /// `season==3`), Right-Handed ○ (200012/608, `rotation==1`), Sunny Days ○
    /// (200212/602, `weather==1`).
    fn copano_rickey_extra_greens() -> Vec<Skill> {
        let green = |id: &str, tags: Vec<i32>, cond: &str, effect_type: i32| Skill {
            skill_id: SkillId::new(id),
            rarity: SkillRarity::White,
            tags,
            alternatives: vec![SkillAlternative {
                base_duration: -10000.0,
                cooldown_time: Some(0.0),
                duration_scaling: None,
                condition: cond.to_owned(),
                precondition: Some(String::new()),
                effects: vec![RawSkillEffect {
                    modifier: 400000.0,
                    target: SkillTarget::SelfTarget,
                    effect_type,
                    value_usage: Some(1),
                    value_level_usage: Some(1),
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        };
        vec![
            green("200192", vec![401, 603], "season==3", 1),
            green("200012", vec![401, 608], "rotation==1", 1),
            green("200212", vec![404, 602], "weather==1", 4),
        ]
    }

    /// Build a Copano Rickey Pace Chaser on a dirt-grade (track 10101) course
    /// with the given skills, prepared under Good ground so all three greens'
    /// conditions hold at the gate.
    fn copano_on_dirtgrade(skills: Vec<Skill>) -> Runner {
        use crate::course::model::CourseData;
        use crate::shared_kernel::language::{DistanceType, Orientation, Surface};

        let course = CourseData {
            course_id: 11103,
            race_track_id: 10101, // in DIRT_GRADE_TRACK_IDS -> is_dirtgrade==1
            distance: 2000.0,
            distance_type: DistanceType::Mid,
            surface: Surface::Dirt,
            turn: Orientation::Clockwise,
            course_set_status: vec![],
            corners: vec![],
            straights: vec![],
            slopes: vec![],
            lane_max: 10.0,
            course_width: 30.0,
            horse_lane: 1.5,
            lane_change_acceleration: 0.0,
            lane_change_acceleration_per_frame: 0.0,
            max_lane_distance: 0.0,
            move_lane_point: 0.0,
            is_abroad: false,
        };
        use crate::shared_kernel::language::{Season, Weather};
        let mut rp = test_race_params();
        rp.ground = GroundCondition::Good; // ground_condition==2
        rp.season = Season::Autumn; // season==3 (Fall Runner)
        rp.weather = Weather::Sunny; // weather==1 (Sunny Days)

        let props = CreateRunner {
            outfit_id: "109801".to_owned(),
            name: "Copano Rickey".to_owned(),
            mood: Mood::Normal,
            strategy: Strategy::PaceChaser,
            popularity: 0,
            team: None,
            aptitudes: RunnerAptitudes {
                distance: Aptitude::S,
                strategy: Aptitude::A,
                surface: Aptitude::A,
            },
            stats: StatLine {
                speed: 1300,
                stamina: 1000,
                power: 1200,
                guts: 600,
                wit: 1100,
            },
            skills,
            forced_positions: HashMap::new(),
            injected_debuffs: vec![],
            forced_rushed_regions: vec![],
            forced_dueling_regions: vec![],
            forced_spot_struggle_regions: vec![],
            forced_downhill_regions: vec![],
            forced_rank: vec![],
            gate: None,
            forced_start_delay: None,
            forced_last_spurt_distance: None,
        };
        let mut r = Runner::create(
            RunnerId(0),
            &course,
            GroundCondition::Good,
            props,
            Box::new(NoopStaminaPolicy),
            Box::new(Xoshiro256StarStar::from_u32_seed(1)),
        );

        let catalog = build_catalog();
        let parser = ConditionParser::new(&catalog);
        let wc = test_whole_course(&course);
        let ctx = PrepareContext {
            course: &course,
            base_speed: 20.0,
            condition_resolution: ConditionResolution::Dynamic,
            pos_keep_end_multiplier: 3.0,
            race_params: &rp,
            whole_course: &wc,
            parser: &parser,
            skill_samples: 4,
            round_iteration: 0,
        };
        r.on_prepare(Box::new(Xoshiro256StarStar::from_u64_seed(7)), &ctx);
        r
    }

    /// Force `100981` to activate and return the runner post-proc.
    fn proc_luck_runs_my_way(r: &mut Runner) {
        r.wit_checks_enabled = false;
        let idx = r
            .pending_skills
            .iter()
            .position(|s| s.skill_id.as_str() == "100981")
            .expect("100981 must be pending after prepare");
        let trigger = r.pending_skills[idx].trigger;
        r.position = trigger.start + 0.5;
        r.process_skill_activations(&FieldView::at_gate(), 2000.0);
    }

    #[test]
    fn copano_rickey_usage_14_benefits_from_activated_greens() {
        // Uma 1: 100981 + three greens (201532/612, 202252/606, 200162/601).
        let mut uma1 = copano_on_dirtgrade(copano_rickey_full_skills());
        // All three greens fire at the gate and are recorded.
        assert_eq!(
            uma1.activated_ledger.activated_green_count(),
            3,
            "the three green skills must activate on a dirt-grade Good-ground course"
        );

        proc_luck_runs_my_way(&mut uma1);
        assert!(uma1.used_skills.contains("100981"), "100981 must proc");

        // Uma 2: only 100981 -> no greens -> tier 0x baseline.
        let mut uma2 = copano_on_dirtgrade(vec![copano_rickey_full_skills()
            .into_iter()
            .next()
            .expect("100981 is the first skill")]);
        assert_eq!(uma2.activated_ledger.activated_green_count(), 0);
        proc_luck_runs_my_way(&mut uma2);
        assert!(uma2.used_skills.contains("100981"));

        // Target speed carries no start-dash term, so absolute values are clean:
        // green count 3 -> tier 1x adds the usage-14 0.05 on top of the shared
        // Direct 0.25; the no-green runner stays at 0.25.
        assert!(
            (uma1.modifiers.target_speed.total() - 0.30).abs() < 1e-9,
            "uma1 target speed was {}",
            uma1.modifiers.target_speed.total()
        );
        assert!(
            (uma2.modifiers.target_speed.total() - 0.25).abs() < 1e-9,
            "uma2 target speed was {}",
            uma2.modifiers.target_speed.total()
        );

        // Acceleration shares the +24.0 start-dash baseline on both runners, so
        // the usage-14 contribution is the delta: uma1 gets +0.05, uma2 +0.0.
        let accel_delta = uma1.modifiers.accel.total() - uma2.modifiers.accel.total();
        assert!(
            (accel_delta - 0.05).abs() < 1e-9,
            "usage-14 accel delta was {accel_delta} (uma1 {}, uma2 {})",
            uma1.modifiers.accel.total(),
            uma2.modifiers.accel.total()
        );
    }

    #[test]
    fn copano_rickey_usage_14_reaches_tier_3_with_six_greens() {
        // Second saved scenario: 100981 + six greens (201532/612, 202252/606,
        // 200162/601, 200192/603, 200012/608, 200212/602). All six are
        // gate-deterministic on course 11103 (dirt grade, Good, Autumn, Sunny,
        // clockwise, Pace Chaser).
        let mut skills = copano_rickey_full_skills();
        skills.extend(copano_rickey_extra_greens());
        let mut uma1 = copano_on_dirtgrade(skills);
        assert_eq!(
            uma1.activated_ledger.activated_green_count(),
            6,
            "all six greens must activate"
        );
        proc_luck_runs_my_way(&mut uma1);
        assert!(uma1.used_skills.contains("100981"));

        // No-green baseline.
        let mut uma2 = copano_on_dirtgrade(vec![copano_rickey_full_skills()
            .into_iter()
            .next()
            .expect("100981 is the first skill")]);
        proc_luck_runs_my_way(&mut uma2);

        // 6 greens -> tier 3x: usage-14 Target Speed 0.05*3 = 0.15 on top of the
        // Direct 0.25 -> 0.40; usage-14 accel delta 0.15.
        assert!(
            (uma1.modifiers.target_speed.total() - 0.40).abs() < 1e-9,
            "uma1 target speed was {}",
            uma1.modifiers.target_speed.total()
        );
        let accel_delta = uma1.modifiers.accel.total() - uma2.modifiers.accel.total();
        assert!(
            (accel_delta - 0.15).abs() < 1e-9,
            "usage-14 accel delta was {accel_delta}"
        );
    }

    #[test]
    fn pending_skills_built_on_prepare() {
        let mut r = runner_with_skills(vec![target_speed_skill(
            "100001",
            SkillRarity::Gold,
            "phase>=2",
        )]);
        prepare(&mut r);
        assert_eq!(r.pending_skills.len(), 1);
        assert_eq!(r.pending_skills[0].skill_id.as_str(), "100001");
    }

    fn debuff_skill(id: &str) -> Skill {
        // Negative current-speed targeting all (other) runners: an injectable
        // external debuff.
        Skill {
            skill_id: SkillId::new(id),
            rarity: SkillRarity::White,
            tags: vec![],
            alternatives: vec![SkillAlternative {
                base_duration: 30000.0,
                cooldown_time: None,
                duration_scaling: None,
                condition: "phase>=0".to_owned(),
                precondition: None,
                effects: vec![RawSkillEffect {
                    modifier: -5000.0,
                    target: SkillTarget::All,
                    effect_type: 31, // CurrentSpeed
                    value_usage: None,
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        }
    }

    #[test]
    fn injected_debuff_queues_fixed_position_targeted_skill() {
        let mut r = runner_with_skills(vec![]);
        r.injected_debuffs = vec![crate::runner::InjectedDebuff {
            skill: debuff_skill("700001"),
            position: 800.0,
        }];
        prepare(&mut r);
        assert_eq!(r.pending_targeted_skills.len(), 1);
        let pending = &r.pending_targeted_skills[0];
        assert_eq!(pending.skill_id.as_str(), "700001");
        assert!(matches!(pending.origin, TargetedSkillOrigin::Injection));
        // Fixed-position policy clips the trigger window around position 800.
        assert!(pending.trigger.start <= 800.0 && pending.trigger.end >= 800.0);
        assert_eq!(pending.effects.len(), 1);
        assert!(pending.effects[0].modifier < 0.0);
    }

    #[test]
    fn injected_non_debuff_effect_is_ignored() {
        // A self-targeted positive effect is not an external debuff.
        let mut skill = debuff_skill("700002");
        skill.alternatives[0].effects[0].target = SkillTarget::SelfTarget;
        skill.alternatives[0].effects[0].modifier = 5000.0;
        let mut r = runner_with_skills(vec![]);
        r.injected_debuffs = vec![crate::runner::InjectedDebuff {
            skill,
            position: 800.0,
        }];
        prepare(&mut r);
        assert!(r.pending_targeted_skills.is_empty());
    }

    #[test]
    fn target_speed_skill_activates_and_applies_modifier() {
        let mut r = runner_with_skills(vec![target_speed_skill(
            "100001",
            SkillRarity::Gold,
            "phase>=2",
        )]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;

        let field = FieldView::at_gate();
        r.process_skill_activations(&field, 2400.0);

        assert_eq!(r.target_speed_skills_active.len(), 1);
        assert!(r.modifiers.target_speed.total() > 0.0);
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("100001"));
        assert!(r.pending_skills.is_empty());
    }

    /// `is_activate_any_skill`: another skill "has just been activated", on
    /// this tick or the last, not at any time before.
    #[test]
    fn is_activate_any_skill_reads_an_activation_on_this_tick_or_the_last() {
        // An early skill fires in the Mid-Race; 120011 waits in the Late-Race
        // for another activation; `other`, if given, fires on entering it.
        let fire = |other: Option<&str>| {
            let mut skills = vec![
                target_speed_skill("100001", SkillRarity::Gold, "phase>=1"),
                target_speed_skill(
                    "120011",
                    SkillRarity::Unique,
                    "phase>=2&is_activate_any_skill==1",
                ),
            ];
            if let Some(id) = other {
                skills.push(target_speed_skill(id, SkillRarity::White, "phase>=2"));
            }
            let mut r = runner_with_skills(skills);
            prepare(&mut r);
            r.wit_checks_enabled = false;
            let late = r
                .pending_skills
                .iter()
                .find(|p| p.skill_id.0 == "120011")
                .expect("pending")
                .trigger
                .start;
            r.position = r.pending_skills[0].trigger.start + 1.0;
            r.process_skill_activations(&FieldView::at_gate(), 2400.0);
            assert_eq!(r.skills_activated_count, 1, "the early skill");
            // A tick later, still in the Mid-Race.
            r.position += 1.0;
            r.process_skill_activations(&FieldView::at_gate(), 2400.0);
            let mut ticks = Vec::new();
            for tick in 0..3 {
                r.position = late + 1.0 + f64::from(tick);
                r.process_skill_activations(&FieldView::at_gate(), 2400.0);
                if r.used_skills.contains("120011") {
                    ticks.push(tick);
                }
            }
            ticks.first().copied()
        };
        // The early activation is two ticks old: it never fires.
        assert_eq!(fire(None), None);
        // Another skill fires on entering the Late-Race. The pass checks skills
        // in ascending id order (mechanics doc § activate_count_x): a lower id
        // fires first and 120011 follows on the same tick; a higher id fires
        // after 120011 was checked, so 120011 follows on the next.
        assert_eq!(fire(Some("110001")), Some(0), "a lower id");
        assert_eq!(fire(Some("200331")), Some(1), "a higher id");
    }

    /// `temptation_count` is the runner's own rushed spells, past ones
    /// included, whoever else is rushed.
    #[test]
    fn temptation_count_reads_the_runners_own_spells() {
        let fires = |spells: Vec<(f64, f64)>| {
            let mut r = runner_with_skills(vec![target_speed_skill(
                "900591",
                SkillRarity::Gold,
                "phase>=2&temptation_count==0",
            )]);
            prepare(&mut r);
            r.wit_checks_enabled = false;
            r.rushed_activations = spells;
            activate_first(&mut r, &FieldView::at_gate());
            r.used_skills.contains("900591")
        };
        assert!(fires(Vec::new()));
        // A spell in the Mid-Race, long over.
        assert!(!fires(vec![(700.0, 900.0)]));
    }

    /// Activation duration (seconds) of a target-speed skill carrying
    /// `duration_scaling`, activated with the leader `ahead` metres in front.
    fn activated_duration(duration_scaling: Option<i32>, ahead: f64) -> f64 {
        let mut skill = target_speed_skill("100001", SkillRarity::Gold, "phase>=2");
        skill.alternatives[0].duration_scaling = duration_scaling;
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        let field = FieldView {
            leader_position: Some(r.position + ahead),
            ..FieldView::default()
        };
        r.process_skill_activations(&field, 2400.0);
        assert_eq!(r.target_speed_skills_active.len(), 1);
        -r.target_speed_skills_active[0].duration_timer.t
    }

    /// A target-speed skill (4500) with a second target-speed effect (500)
    /// held as an additional activation of `trigger`.
    fn skill_with_held_effect(id: &str, trigger: i32) -> Skill {
        let mut skill = target_speed_skill(id, SkillRarity::Gold, "phase>=2");
        let mut held = skill.alternatives[0].effects[0];
        held.modifier = 500.0;
        held.additional_activate_type = Some(trigger);
        skill.alternatives[0].effects.push(held);
        skill
    }

    /// Prepare `skills`, then place the runner inside the first pending
    /// trigger and run one activation pass with `field`.
    /// Put the runner inside the 110071 carrier's window, ahead of the gold's
    /// own (phase 2), and run one activation pass.
    fn fire_carrier(r: &mut Runner) {
        let carrier = r
            .pending_skills
            .iter()
            .find(|p| p.skill_id.as_str() == "110071")
            .expect("carrier pending")
            .trigger;
        let gold = r
            .pending_skills
            .iter()
            .find(|p| p.skill_id.as_str() == "200002")
            .expect("gold pending")
            .trigger;
        r.position = carrier.start + 0.5;
        assert!(r.position < gold.start, "the gold's own window is later");
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
    }

    #[test]
    fn a_forced_gold_runs_its_full_duration() {
        // Sirius Symboli's unique shape (110071): a target-speed effect plus
        // ActivateRandomGold, here forcing one gold. Rarity White so the
        // carrier is neither dropped as another outfit's unique nor a gold
        // candidate itself. The gold must run base x
        // course distance / 1000 (3 s x 2.4), not base x the carrier's own
        // duration / 1000.
        let mut carrier = target_speed_skill("110071", SkillRarity::White, "phase==1");
        carrier.alternatives[0].effects.push(RawSkillEffect {
            modifier: 10000.0,
            target: SkillTarget::SelfTarget,
            effect_type: 37, // ActivateRandomGold
            value_usage: None,
            value_level_usage: None,
            pre_applied_multiplier: None,
            additional_activate_type: None,
        });
        let gold = target_speed_skill("200002", SkillRarity::Gold, "phase>=2");
        let mut r = runner_with_skills(vec![carrier, gold]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        fire_carrier(&mut r);
        let forced = r
            .target_speed_skills_active
            .iter()
            .find(|a| a.skill_id.as_str() == "200002")
            .expect("the gold was forced");
        assert!(
            (-forced.duration_timer.t - 3.0 * 2.4).abs() < 1e-9,
            "{}",
            -forced.duration_timer.t
        );
    }

    #[test]
    fn a_gold_cooling_down_is_not_forced_again() {
        let mut carrier = target_speed_skill("110071", SkillRarity::White, "phase==1");
        carrier.alternatives[0].effects.push(RawSkillEffect {
            modifier: 10000.0,
            target: SkillTarget::SelfTarget,
            effect_type: 37,
            value_usage: None,
            value_level_usage: None,
            pre_applied_multiplier: None,
            additional_activate_type: None,
        });
        let gold = target_speed_skill("200002", SkillRarity::Gold, "phase>=2");
        let mut r = runner_with_skills(vec![carrier, gold]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        // The gold has fired and waits out its cooldown (patch 0019 keeps it).
        let at = r
            .pending_skills
            .iter()
            .position(|p| p.skill_id.as_str() == "200002")
            .expect("gold pending");
        r.pending_skills[at].ready_at = r.accumulate_time.t + 60.0;
        fire_carrier(&mut r);
        assert!(r
            .target_speed_skills_active
            .iter()
            .all(|a| a.skill_id.as_str() != "200002"));
    }

    /// A gold is a candidate for ActivateRandomGold from its ready tick,
    /// counted in whole ticks as for its own condition, and not a tick
    /// before. See Ya Later!'s shape (3 s, 30 s base cooldown) as a gold at
    /// 2600 m fires on its own condition; a carrier that forces one gold
    /// fires on the tick before the gold's ready tick or on the ready tick
    /// itself, where the gold's own condition does not hold. A comparison of
    /// the clock's float sum with activation + duration + cooldown leaves the
    /// gold out on its ready tick after each of these 60 first firings.
    #[test]
    fn a_gold_is_forced_from_its_exact_ready_tick() {
        use crate::skills::condition::dynamic::ConditionTimers;
        const DISTANCE: f64 = 2600.0;
        let mut carrier =
            target_speed_skill("110071", SkillRarity::White, "infront_near_lane_time>=3");
        carrier.alternatives[0].effects.push(RawSkillEffect {
            modifier: 10000.0,
            target: SkillTarget::SelfTarget,
            effect_type: 37, // ActivateRandomGold
            value_usage: None,
            value_level_usage: None,
            pre_applied_multiplier: None,
            additional_activate_type: None,
        });
        let mut gold = target_speed_skill("201662", SkillRarity::Gold, "behind_near_lane_time>=3");
        gold.alternatives[0].effects[0].modifier = 3500.0;
        gold.alternatives[0].cooldown_time = Some(300000.0);
        let field = |near_behind: f64, near_infront: f64| FieldView {
            condition_timers: Some(ConditionTimers {
                near_behind,
                near_infront,
                ..ConditionTimers::default()
            }),
            ..FieldView::default()
        };
        let ready = ready_ticks(
            3.0 * (DISTANCE / 1000.0),
            30.0 * DISTANCE / 1000.0,
            FRAME_DT,
        ) as u32;
        let mut wrong = Vec::new();
        // First firings 15 ticks apart from tick 150, over most of a race.
        for first in (150_u32..1050).step_by(15) {
            for (after, forced) in [(ready - 1, false), (ready, true)] {
                let mut r = runner_with_skills(vec![carrier.clone(), gold.clone()]);
                prepare_on(&mut r, DISTANCE);
                r.wit_checks_enabled = false;
                // Both armed over the whole race, whatever window each drew.
                assert_eq!(r.pending_skills.len(), 2, "the carrier and the gold");
                for pending in &mut r.pending_skills {
                    pending.trigger = Region::new(0.0, DISTANCE);
                }
                r.position = 1000.0;
                // The clock steps a tick at a time, as in the race.
                for _ in 0..first {
                    r.accumulate_time.advance(FRAME_DT);
                }
                r.process_skill_activations(&field(30.0, 0.0), DISTANCE);
                assert_eq!(r.skills_activated_count, 1, "tick {first}: the gold");
                for _ in 0..after {
                    r.accumulate_time.advance(FRAME_DT);
                }
                r.process_skill_activations(&field(0.0, 30.0), DISTANCE);
                // The carrier, and the gold again if it was a candidate.
                let expected = if forced { 3 } else { 2 };
                if r.skills_activated_count != expected {
                    wrong.push((first, after, r.skills_activated_count));
                }
            }
        }
        assert!(
            wrong.is_empty(),
            "ready after {ready} ticks; (first firing, carrier after, fired) {wrong:?}"
        );
    }

    fn activate_first(r: &mut Runner, field: &FieldView) {
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        r.process_skill_activations(field, 2400.0);
    }

    fn overtaking() -> FieldView {
        FieldView {
            self_previous_order: Some(5),
            self_order: Some(4),
            ..FieldView::default()
        }
    }

    #[test]
    fn additional_activation_order_up_fires_on_overtakes_only_up_to_three() {
        // Mechanics doc § Additional Activate / OrderUp: the effect does nothing
        // at activation; it applies for the remaining duration on each overtake,
        // up to 3 times.
        let mut r = runner_with_skills(vec![skill_with_held_effect("100531", 1)]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        activate_first(&mut r, &FieldView::at_gate());
        assert_eq!(
            r.target_speed_skills_active.len(),
            1,
            "held effect not applied at activation"
        );
        assert!((r.modifiers.target_speed.total() - 0.45).abs() < 1e-9);

        // A pass with no overtake fires nothing.
        r.process_skill_activations(&FieldView::default(), 2400.0);
        assert_eq!(r.target_speed_skills_active.len(), 1);

        for _ in 0..5 {
            r.process_skill_activations(&overtaking(), 2400.0);
        }
        assert_eq!(
            r.target_speed_skills_active.len(),
            4,
            "1 + 3 firings, capped"
        );
        assert!((r.modifiers.target_speed.total() - (0.45 + 3.0 * 0.05)).abs() < 1e-9);
        // Each firing runs for the skill's remaining duration, not a fresh one.
        let base = -r.target_speed_skills_active[0].duration_timer.t;
        for fired in &r.target_speed_skills_active[1..] {
            assert!((-fired.duration_timer.t - base).abs() < 1e-9);
        }
    }

    #[test]
    fn additional_activation_any_skill_fires_on_other_skills_not_itself() {
        // Type 3 (ActivateAnySkill type 2): each OTHER skill activated, up to 2.
        let mut skills = vec![skill_with_held_effect("110211", 3)];
        for id in ["200001", "200002", "200003"] {
            skills.push(target_speed_skill(id, SkillRarity::White, "phase>=2"));
        }
        let mut r = runner_with_skills(skills);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        // Activate the carrier alone first.
        let carrier = r
            .pending_skills
            .iter()
            .position(|p| p.skill_id.as_str() == "110211")
            .expect("carrier pending");
        let carrier_skill = r.pending_skills.remove(carrier);
        r.position = carrier_skill.trigger.start + 0.5;
        r.pending_skills.insert(0, carrier_skill);
        let others = r.pending_skills.split_off(1);
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(
            r.target_speed_skills_active.len(),
            1,
            "its own activation does not fire it"
        );

        // Three other skills activate; the held effect fires on two of them.
        r.pending_skills = others;
        for p in &mut r.pending_skills {
            p.trigger = Region::new(r.position - 1.0, r.position + 10.0);
        }
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.target_speed_skills_active.len(), 1 + 3 + 2);
    }

    #[test]
    fn duration_scaling_4_extends_every_modifier_per_overtake_up_to_three() {
        // Mechanics doc § IncrementOrderUp: +1 s per overtake while active, up to
        // 3, scaled by distance / 1000 like the base duration, applied to all
        // the skill's modifiers.
        let mut skill = target_speed_skill("100531", SkillRarity::Gold, "phase>=2");
        skill.alternatives[0].duration_scaling = Some(4);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        activate_first(&mut r, &FieldView::at_gate());
        let before = -r.target_speed_skills_active[0].duration_timer.t;
        for _ in 0..5 {
            r.process_skill_activations(&overtaking(), 2400.0);
        }
        let after = -r.target_speed_skills_active[0].duration_timer.t;
        assert!(
            (after - before - 3.0 * 2.4).abs() < 1e-9,
            "3 x 1 s x 2400 / 1000"
        );

        // Without code 4 an overtake changes nothing.
        let mut r = runner_with_skills(vec![target_speed_skill(
            "100531",
            SkillRarity::Gold,
            "phase>=2",
        )]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        activate_first(&mut r, &FieldView::at_gate());
        let before = -r.target_speed_skills_active[0].duration_timer.t;
        r.process_skill_activations(&overtaking(), 2400.0);
        assert!((-r.target_speed_skills_active[0].duration_timer.t - before).abs() < 1e-9);
    }

    #[test]
    fn cooldown_rearms_at_a_later_trigger_after_base_times_distance() {
        // Mechanics doc § Skill Cooldown: Cooldown = BaseCooldown x
        // CourseDistance / 1000. 30 s base at 2400 m -> 72 s.
        let mut skill = target_speed_skill("200331", SkillRarity::White, "phase>=2");
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        assert!((r.pending_skills[0].cooldown - 72.0).abs() < 1e-9);
        // Two placed triggers, 20 m apart, as all_corner_random places them.
        let start = r.pending_skills[0].trigger.start;
        r.pending_skills[0].trigger = Region::new(start, start + 10.0);
        r.pending_skills[0].later_triggers = vec![Region::new(start + 20.0, start + 30.0)];
        r.wit_checks_enabled = false;
        r.position = start + 5.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        assert!(
            r.pending_skills[0].wit_passed,
            "stays armed, wit check spent"
        );

        // The next trigger arrives inside the cooldown: no activation, and the
        // skill is done once that window passes.
        r.position = start + 12.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        r.position = start + 25.0;
        r.accumulate_time.advance(10.0);
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "inside the cooldown");

        // Same again with the cooldown elapsed: it fires, without a new wit
        // roll (checks on). The cooldown runs from the end of the effect
        // (3 s x 2400 / 1000 = 7.2 s), so the trigger reached 73 s after the
        // activation is still inside it and the one reached at 80 s is not.
        let mut skill = target_speed_skill("200331", SkillRarity::White, "phase>=2");
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        let start = r.pending_skills[0].trigger.start;
        r.pending_skills[0].trigger = Region::new(start, start + 10.0);
        r.pending_skills[0].later_triggers = vec![Region::new(start + 20.0, start + 30.0)];
        r.wit_checks_enabled = false;
        r.position = start + 5.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        r.wit_checks_enabled = true;
        r.position = start + 12.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        r.accumulate_time.advance(73.0);
        r.position = start + 25.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "inside effect + cooldown");
        r.accumulate_time.advance(7.0);
        r.position = start + 26.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 2, "after the cooldown");
    }

    #[test]
    fn a_single_window_skill_fires_again_after_its_cooldown() {
        // See Ya Later!'s shape: one window, a 3 s effect and a 30 s base
        // cooldown (7.2 s and 72 s at 2400 m). It stays armed inside its
        // window and fires again once the effect and then the cooldown have
        // run.
        let mut skill = target_speed_skill("201662", SkillRarity::White, "phase>=2");
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        activate_first(&mut r, &FieldView::at_gate());
        assert_eq!(r.skills_activated_count, 1);
        assert_eq!(r.pending_skills.len(), 1, "still armed");
        r.accumulate_time.advance(71.0);
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "inside the cooldown");
        r.accumulate_time.advance(8.5);
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 2, "after the cooldown");
    }

    /// The cooldown starts when the effect ends, not at the activation: ready
    /// = activation + duration + cooldown, each x distance / 1000 and counted
    /// in whole ticks. None of the 43 repeats on the 117 recordings (201662,
    /// 201651, 200331, 200332, 200342) falls between activation + cooldown and
    /// that point.
    #[test]
    fn the_cooldown_runs_from_the_end_of_the_effect() {
        // See Ya Later!'s shape at 2400 m: a 7.2 s effect, a 72 s cooldown.
        let mut skill = target_speed_skill("201662", SkillRarity::White, "phase>=2");
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        activate_first(&mut r, &FieldView::at_gate());
        let fired_at = r.accumulate_time.t;
        let ready = ready_ticks(7.2, 72.0, FRAME_DT);
        assert!(
            (r.pending_skills[0].ready_at - (fired_at + ready * FRAME_DT)).abs() < 1e-9,
            "ready {} after the activation",
            r.pending_skills[0].ready_at - fired_at
        );
        // The condition holds throughout, as a near-lane spell still running
        // at the end of the cooldown does: it waits for the effect's 7.2 s.
        r.accumulate_time.advance(72.5);
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(
            r.skills_activated_count, 1,
            "cooldown counted from the activation"
        );
        r.accumulate_time.t = fired_at + (ready - 1.0) * FRAME_DT;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "a tick short");
        r.accumulate_time.t = fired_at + ready * FRAME_DT;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 2, "the ready tick");
    }

    /// Whole ticks from an activation to the ready tick: the effect's, then
    /// the cooldown's from the tick it ends. On the game's 0.0666 s tick they
    /// are the recorded gaps between See Ya Later!'s (201662: 3 s, 30 s base
    /// cooldown) two firings where her condition held before the second:
    /// 10908-r0039 runner 2 at 2600 m (ticks 348 and 1638), 10611-r0077
    /// runner 0 at 1600 m (190 and 984) and 10104-r0045 runner 2 at 2000 m
    /// (240 and 1232). At 1700 m this form gives 843 where the elapsed tick +
    /// 1, the other form the recordings allow, gives 844. On 1/15 s ticks the
    /// engine's float products (3.0 x 2.6 = 7.800000000000001 s) still count
    /// as the whole ticks they are.
    #[test]
    fn ready_ticks_count_the_effect_then_the_cooldown_in_whole_ticks() {
        const GAME_TICK: f64 = 0.0666;
        // As the engine computes them: base x distance / 1000.
        let see_ya_later = |distance: f64, tick: f64| {
            let duration = 3.0 * (distance / 1000.0);
            let cooldown = 300000.0 / 10000.0 * distance / 1000.0;
            ready_ticks(duration, cooldown, tick)
        };
        assert_eq!(see_ya_later(2600.0, GAME_TICK), 1290.0, "118 + 1172");
        assert_eq!(see_ya_later(1600.0, GAME_TICK), 794.0, "73 + 721");
        assert_eq!(see_ya_later(2000.0, GAME_TICK), 992.0, "91 + 901");
        assert_eq!(see_ya_later(1700.0, GAME_TICK), 843.0, "77 + 766");
        assert_eq!(see_ya_later(2600.0, 1.0 / 15.0), 1287.0, "117 + 1170");
        assert_eq!(see_ya_later(1600.0, 1.0 / 15.0), 792.0, "72 + 720");
        assert_eq!(ready_ticks(0.0, 72.0, 1.0 / 15.0), 1080.0, "instant");
    }

    /// A skill re-armed into a condition that already holds fires again on
    /// its ready tick, ceil(duration / tick) + ceil(cooldown / tick) ticks
    /// after its activation on the engine's tick, and not before, whichever
    /// tick it first fired on: a comparison of the clock's float sum with
    /// activation + duration + cooldown put it a tick late on some. See Ya
    /// Later! (201662) as 10908-r0039 runner 2 (2600 m) and 10611-r0077
    /// runner 0 (1600 m) carried it, the uma behind held near throughout.
    #[test]
    fn a_re_armed_skill_fires_on_its_ready_tick_counted_in_whole_ticks() {
        use crate::skills::condition::dynamic::ConditionTimers;
        let mut skill = target_speed_skill(
            "201662",
            SkillRarity::White,
            "behind_near_lane_time>=3&accumulatetime>=10",
        );
        skill.alternatives[0].effects[0].modifier = 3500.0;
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let spell = FieldView {
            condition_timers: Some(ConditionTimers {
                near_behind: 30.0,
                ..ConditionTimers::default()
            }),
            ..FieldView::default()
        };
        for distance in [2600.0, 1600.0] {
            let ready = ready_ticks(
                3.0 * (distance / 1000.0),
                30.0 * distance / 1000.0,
                FRAME_DT,
            );
            let mut late = Vec::new();
            // First firings a second apart from 10 s, over most of a race.
            for first in (0..900).step_by(15) {
                let mut r = runner_with_skills(vec![skill.clone()]);
                prepare_on(&mut r, distance);
                r.wit_checks_enabled = false;
                // The clock steps a tick at a time, as in the race.
                while r.accumulate_time.t < 10.0 {
                    r.accumulate_time.advance(FRAME_DT);
                }
                for _ in 0..first {
                    r.accumulate_time.advance(FRAME_DT);
                }
                r.position = r.pending_skills[0].trigger.start + 0.5;
                r.process_skill_activations(&spell, distance);
                assert_eq!(r.skills_activated_count, 1, "{distance} m: first firing");
                let mut ticks = 0.0;
                while r.skills_activated_count == 1 && ticks <= ready + 2.0 {
                    r.accumulate_time.advance(FRAME_DT);
                    r.process_skill_activations(&spell, distance);
                    ticks += 1.0;
                }
                if ticks != ready {
                    late.push((first, ticks));
                }
            }
            assert!(
                late.is_empty(),
                "{distance} m: ready after {ready} ticks; (ticks past 10 s, fired after) {late:?}"
            );
        }
    }

    #[test]
    fn a_passed_window_moves_on_to_the_next_placed_trigger() {
        let mut skill = target_speed_skill("200331", SkillRarity::White, "phase>=2");
        skill.alternatives[0].cooldown_time = Some(300000.0);
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        r.pending_skills[0].trigger = Region::new(100.0, 110.0);
        r.pending_skills[0].later_triggers = vec![Region::new(900.0, 910.0)];
        r.position = 105.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        r.position = 500.0; // past the first window
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.pending_skills[0].trigger, Region::new(900.0, 910.0));
        r.accumulate_time.advance(80.0);
        r.position = 905.0;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 2);
        r.position = 950.0; // past the last window: done
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert!(r.pending_skills.is_empty());
    }

    /// A rival-dependent token is checked from the first tick of every region
    /// of its window: here `is_overtake` in the Early-Race or the Late-Race.
    /// The port's Erlang policy armed one region, from a random offset in.
    #[test]
    fn a_rival_token_is_checked_from_the_first_tick_of_every_region() {
        use crate::skills::condition::dynamic::ConditionTimers;
        let skill = target_speed_skill(
            "202401",
            SkillRarity::Gold,
            "phase==0&is_overtake==1@phase==2&is_overtake==1",
        );
        let regions = build(&skill)[0].regions.0.clone();
        assert_eq!(regions.len(), 2, "{regions:?}");
        let target = |held: bool| FieldView {
            condition_timers: Some(ConditionTimers {
                has_overtake_target: held,
                ..ConditionTimers::default()
            }),
            ..FieldView::default()
        };
        let runner = || {
            let mut r = runner_with_skills(vec![skill.clone()]);
            prepare(&mut r);
            r.wit_checks_enabled = false;
            r
        };

        let r = runner();
        assert_eq!(r.pending_skills[0].trigger, regions[0]);
        assert_eq!(r.pending_skills[0].later_triggers, regions[1..].to_vec());

        // First region: nothing while no target, then the first tick one is.
        let mut r = runner();
        r.position = regions[0].start + 0.5;
        r.process_skill_activations(&target(false), 2400.0);
        assert_eq!(r.skills_activated_count, 0);
        r.position += 1.0;
        r.process_skill_activations(&target(true), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "the first tick it holds");

        // Second region, the first region passed without a target.
        let mut r = runner();
        r.position = regions[0].start + 0.5;
        r.process_skill_activations(&target(false), 2400.0);
        r.position = regions[1].start - 1.0; // past the first window
        r.process_skill_activations(&target(false), 2400.0);
        r.position = regions[1].start + 0.5;
        r.process_skill_activations(&target(true), 2400.0);
        assert_eq!(
            r.skills_activated_count, 1,
            "the second region's first tick"
        );
    }

    /// `compete_fight_count>0` (Now We're Cruisin'!, 100341) is checked from
    /// the first tick of its window, so it fires on the first tick of the
    /// runner's own Showdown. On the 117 recordings all 15 firings of
    /// 100341/900341 come one recorded tick (0.067 s) after her first duel
    /// event. The port's Uniform policy first drew a random start point into
    /// the window.
    #[test]
    fn a_showdown_skill_fires_on_the_first_tick_of_her_duel() {
        use crate::shared_kernel::language::Strategy;
        use crate::skills::condition::dynamic::ActiveRunner;
        let skill = target_speed_skill("100341", SkillRarity::Unique, "compete_fight_count>0");
        let regions = build(&skill)[0].regions.0.clone();
        let field = |dueling: bool| FieldView {
            active_runners: vec![ActiveRunner {
                is_self: true,
                position: 0.0,
                strategy: Strategy::PaceChaser,
                gate: 0,
                popularity: 0,
                is_rushed: false,
                is_dueling: dueling,
                has_dueled: false,
                activated_advantage_effect_types: 0,
            }],
            ..FieldView::default()
        };
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        assert_eq!(r.pending_skills[0].trigger, regions[0], "the whole window");

        r.position = regions[0].start + 0.5;
        r.process_skill_activations(&field(false), 2400.0);
        assert_eq!(r.skills_activated_count, 0, "no duel yet");
        r.position += 1.0;
        r.process_skill_activations(&field(true), 2400.0);
        assert_eq!(r.skills_activated_count, 1, "the duel's first tick");
    }

    /// A skill of two alternatives, as the data carries them: the first
    /// applies 4500, the second 3500, both with `cooldown_time`.
    fn two_alternative_skill(
        id: &str,
        rarity: SkillRarity,
        conditions: [&str; 2],
        cooldown_time: Option<f64>,
    ) -> Skill {
        let mut skill = target_speed_skill(id, rarity, conditions[0]);
        skill.alternatives[0].cooldown_time = cooldown_time;
        let mut second = skill.alternatives[0].clone();
        second.condition = conditions[1].to_owned();
        second.effects[0].modifier = 3500.0;
        skill.alternatives.push(second);
        skill
    }

    /// Run `ticks` activation passes, 1 m and 1/15 s apart, with the leader
    /// `ahead` metres in front.
    fn run_ticks(r: &mut Runner, ahead: f64, ticks: usize) {
        for _ in 0..ticks {
            let field = FieldView {
                leader_position: Some(r.position + ahead),
                ..FieldView::default()
            };
            r.process_skill_activations(&field, 2400.0);
            r.position += 1.0;
            r.accumulate_time.advance(1.0 / 15.0);
        }
    }

    /// Wit rolls from a script: each draw takes the next value, the last one
    /// repeating.
    struct ScriptedRolls(Vec<f64>);

    impl Prng for ScriptedRolls {
        fn int32(&mut self) -> u32 {
            0
        }
        fn random(&mut self) -> f64 {
            if self.0.len() > 1 {
                self.0.remove(0)
            } else {
                self.0[0]
            }
        }
        fn uniform(&mut self, _upper: u32) -> u32 {
            0
        }
    }

    /// 110101's shape: the first alternative needs the leader within 5 m, the
    /// second does not. The game fires the first that holds, once (the
    /// recordings log which: 15 of 110101's firings the first, 10 the second).
    #[test]
    fn exclusive_alternatives_fire_the_first_that_holds_once() {
        let fire = |ahead: f64| {
            let skill = two_alternative_skill(
                "110101",
                SkillRarity::Unique,
                ["phase>=2&distance_diff_top<=5", "phase>=2"],
                Some(5_000_000.0),
            );
            let mut r = runner_with_skills(vec![skill]);
            prepare(&mut r);
            assert_eq!(r.pending_skills.len(), 2, "both alternatives kept");
            r.position = r.pending_skills[0].trigger.start + 0.5;
            run_ticks(&mut r, ahead, 3);
            (r.skills_activated_count, r.modifiers.target_speed.total())
        };
        // Both hold: the first fires, and only it.
        let (count, speed) = fire(2.0);
        assert_eq!(count, 1);
        assert!((speed - 0.45).abs() < 1e-9, "{speed}");
        // Only the second holds: it fires, once.
        let (count, speed) = fire(9.0);
        assert_eq!(count, 1);
        assert!((speed - 0.35).abs() < 1e-9, "{speed}");
    }

    /// 100671's shape (leader within 5 m, or not): one wit roll and one
    /// cooldown (30 s base: 72 s at 2400 m, from the end of the 7.2 s effect)
    /// for the skill, whichever alternative fires.
    #[test]
    fn exclusive_alternatives_share_one_wit_check_and_one_cooldown() {
        let runner = |rolls: Vec<f64>| {
            let skill = two_alternative_skill(
                "100671",
                SkillRarity::White,
                [
                    "phase>=2&distance_diff_top<=5",
                    "phase>=2&distance_diff_top>5",
                ],
                Some(300_000.0),
            );
            let mut r = runner_with_skills(vec![skill]);
            prepare(&mut r);
            r.wit_rng = Box::new(ScriptedRolls(rolls));
            r.position = r.pending_skills[0].trigger.start + 0.5;
            r
        };
        // The first roll passes, every later one would fail.
        let mut r = runner(vec![0.0, 0.99]);
        run_ticks(&mut r, 2.0, 1);
        assert_eq!(r.skills_activated_count, 1, "the first alternative");
        r.accumulate_time.advance(10.0);
        run_ticks(&mut r, 9.0, 1);
        assert_eq!(
            r.skills_activated_count, 1,
            "the second, inside the cooldown"
        );
        r.accumulate_time.advance(63.0);
        run_ticks(&mut r, 9.0, 1);
        assert_eq!(
            r.skills_activated_count, 1,
            "the second, 73 s on: inside effect + cooldown"
        );
        r.accumulate_time.advance(7.0);
        run_ticks(&mut r, 9.0, 1);
        assert_eq!(
            r.skills_activated_count, 2,
            "the second, after the cooldown, on the roll already passed"
        );
        assert!((r.modifiers.target_speed.total() - 0.80).abs() < 1e-9);

        // The first roll fails, every later one would pass: the skill is out
        // whichever alternative comes to hold.
        let mut r = runner(vec![0.99, 0.0]);
        run_ticks(&mut r, 2.0, 1);
        run_ticks(&mut r, 9.0, 3);
        assert_eq!(r.skills_activated_count, 0);
    }

    /// A later alternative naming `is_activate_other_skill_detail` is a second
    /// stage of the skill: it still triggers on its own, after the first.
    /// One naming `is_used_skill_id` first (110641's shape) is the same skill
    /// under another condition, exclusive of the plain one after it.
    #[test]
    fn multi_trigger_tokens_keep_their_own_trigger() {
        let fire = |conditions: [&str; 2], used: Option<&str>| {
            let skill = two_alternative_skill("110641", SkillRarity::Unique, conditions, None);
            let mut r = runner_with_skills(vec![skill]);
            prepare(&mut r);
            if let Some(id) = used {
                r.used_skills.insert(id.to_owned());
            }
            r.position = r.pending_skills[0].trigger.start + 0.5;
            run_ticks(&mut r, 2.0, 3);
            (r.skills_activated_count, r.modifiers.target_speed.total())
        };
        let (count, speed) = fire(
            ["phase>=2", "phase>=2&is_activate_other_skill_detail==1"],
            None,
        );
        assert_eq!(count, 2, "both stages");
        assert!((speed - 0.80).abs() < 1e-9, "{speed}");

        let runaway_first = ["phase>=2&is_used_skill_id==202051", "phase>=2"];
        let (count, speed) = fire(runaway_first, Some("202051"));
        assert_eq!(count, 1);
        assert!((speed - 0.45).abs() < 1e-9, "{speed}");
        let (count, speed) = fire(runaway_first, None);
        assert_eq!(count, 1);
        assert!((speed - 0.35).abs() < 1e-9, "{speed}");
    }

    #[test]
    fn a_scripted_skill_fires_once_through_its_first_alternative() {
        let skill = two_alternative_skill(
            "110101",
            SkillRarity::Unique,
            ["phase>=2&distance_diff_top<=5", "phase>=2"],
            Some(5_000_000.0),
        );
        let mut r =
            runner_with_skills_forced(vec![skill], HashMap::from([("110101".to_owned(), 1700.0)]));
        prepare(&mut r);
        assert_eq!(r.pending_skills.len(), 1, "one scripted activation");
        r.position = 1700.5;
        run_ticks(&mut r, 9.0, 3);
        assert_eq!(r.skills_activated_count, 1);
        assert!((r.modifiers.target_speed.total() - 0.45).abs() < 1e-9);
    }

    #[test]
    fn a_forced_gold_is_one_candidate_whatever_its_alternatives() {
        // A carrier forcing two golds, and one gold of two alternatives: the
        // gold fires once, through its first alternative, and not again in its
        // own window.
        let mut carrier = target_speed_skill("110071", SkillRarity::White, "phase==1");
        carrier.alternatives[0].effects.push(RawSkillEffect {
            modifier: 20000.0,
            target: SkillTarget::SelfTarget,
            effect_type: 37, // ActivateRandomGold
            value_usage: None,
            value_level_usage: None,
            pre_applied_multiplier: None,
            additional_activate_type: None,
        });
        let gold = two_alternative_skill(
            "200002",
            SkillRarity::Gold,
            ["phase>=2&distance_diff_top<=5", "phase>=2"],
            None,
        );
        let mut r = runner_with_skills(vec![carrier, gold]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let window = r
            .pending_skills
            .iter()
            .find(|p| p.skill_id.as_str() == "200002")
            .expect("gold pending")
            .trigger;
        let applied = |r: &Runner| -> Vec<f64> {
            r.target_speed_skills_active
                .iter()
                .filter(|a| a.skill_id.as_str() == "200002")
                .map(|a| a.modifier)
                .collect()
        };
        fire_carrier(&mut r);
        assert_eq!(applied(&r), vec![0.45]);
        // On into the gold's own window with the leader 2 m ahead, where both
        // alternatives hold: the forced activation was the gold's one.
        r.position = window.start + 0.5;
        run_ticks(&mut r, 2.0, 3);
        assert_eq!(applied(&r), vec![0.45]);
        assert_eq!(r.skills_activated_count, 2, "the carrier and the gold");
    }

    #[test]
    fn a_forced_gold_with_a_second_stage_is_one_candidate() {
        // A carrier forcing two golds, and one gold of two stages, the second
        // naming is_activate_other_skill_detail (100703111's shape, an
        // Evolution skill): the second stage triggers on its own, so the gold
        // has two pending entries, yet it is forced once, through its first
        // alternative, and neither stage fires again in its own window.
        let mut carrier = target_speed_skill("110071", SkillRarity::White, "phase==1");
        carrier.alternatives[0].effects.push(RawSkillEffect {
            modifier: 20000.0,
            target: SkillTarget::SelfTarget,
            effect_type: 37, // ActivateRandomGold
            value_usage: None,
            value_level_usage: None,
            pre_applied_multiplier: None,
            additional_activate_type: None,
        });
        for rarity in [SkillRarity::Gold, SkillRarity::Evolution] {
            let gold = two_alternative_skill(
                "200002",
                rarity,
                ["phase>=2", "phase>=2&is_activate_other_skill_detail==1"],
                None,
            );
            let mut r = runner_with_skills(vec![carrier.clone(), gold]);
            prepare(&mut r);
            r.wit_checks_enabled = false;
            let entries = r
                .pending_skills
                .iter()
                .filter(|p| p.skill_id.as_str() == "200002")
                .count();
            assert_eq!(entries, 2, "{rarity:?}: both stages pending");
            let window = r
                .pending_skills
                .iter()
                .find(|p| p.skill_id.as_str() == "200002")
                .expect("gold pending")
                .trigger;
            let applied = |r: &Runner| -> Vec<f64> {
                r.target_speed_skills_active
                    .iter()
                    .filter(|a| a.skill_id.as_str() == "200002")
                    .map(|a| a.modifier)
                    .collect()
            };
            fire_carrier(&mut r);
            assert_eq!(applied(&r), vec![0.45], "{rarity:?}");
            r.position = window.start + 0.5;
            run_ticks(&mut r, 2.0, 3);
            assert_eq!(applied(&r), vec![0.45], "{rarity:?}");
            assert_eq!(
                r.skills_activated_count, 2,
                "{rarity:?}: the carrier and the gold"
            );
        }
    }

    #[test]
    fn duration_scaling_2_stretches_by_distance_behind_the_leader() {
        // Mechanics doc: ScaledDuration = BaseDuration * min(0.8 + DistanceFromTop
        // / 62.5m, 1.6). 50 m behind -> 1.6x; 12.5 m -> 1.0x; leading -> 0.8x.
        let direct = activated_duration(None, 50.0);
        assert!((activated_duration(Some(2), 50.0) / direct - 1.6).abs() < 1e-9);
        assert!((activated_duration(Some(2), 12.5) / direct - 1.0).abs() < 1e-9);
        assert!((activated_duration(Some(2), 0.0) / direct - 0.8).abs() < 1e-9);
        assert!((activated_duration(Some(2), 500.0) / direct - 1.6).abs() < 1e-9);
        // Direct (1) and unmodeled codes do not scale.
        assert!((activated_duration(Some(1), 50.0) - direct).abs() < 1e-9);
        assert!((activated_duration(Some(5), 50.0) - direct).abs() < 1e-9);
    }

    #[test]
    fn forced_position_bypasses_dynamic_condition_and_wit_check() {
        // `order==1` resolves to a dynamic gate; with no field resolved
        // (`FieldView::at_gate`, order None) it can never pass. A forced
        // position must activate the skill anyway — and deterministically,
        // with wit checks left enabled.
        let skill = target_speed_skill("100001", SkillRarity::Gold, "phase>=0&order==1");

        // Control: without forcing, the dynamic gate holds the skill back.
        let mut control = runner_with_skills(vec![skill.clone()]);
        prepare(&mut control);
        control.position = 1000.5;
        control.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(control.skills_activated_count, 0, "control must not fire");

        let mut r =
            runner_with_skills_forced(vec![skill], HashMap::from([("100001".to_owned(), 1000.0)]));
        prepare(&mut r);
        assert_eq!(r.pending_skills.len(), 1);
        assert!(r.pending_skills[0].forced);
        assert_eq!(r.pending_skills[0].trigger, Region::new(1000.0, 1010.0));

        r.position = 1000.5;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("100001"));
    }

    #[test]
    fn forced_position_overrides_statically_unsatisfiable_condition() {
        // `distance_type==1` (sprint) never matches the Long test course: the
        // trigger survives only as a sentinel `Region::INVALID` window the
        // runner can never reach. Forcing a position must replace it.
        let skill = target_speed_skill("100002", SkillRarity::Gold, "distance_type==1");

        let mut control = runner_with_skills(vec![skill.clone()]);
        prepare(&mut control);
        // The sentinel window sits beyond the course end: unreachable.
        assert!(control.pending_skills[0].trigger.start > 2400.0);

        let mut r =
            runner_with_skills_forced(vec![skill], HashMap::from([("100002".to_owned(), 1200.0)]));
        prepare(&mut r);
        assert_eq!(r.pending_skills.len(), 1);
        assert!(r.pending_skills[0].forced);
        assert_eq!(r.pending_skills[0].trigger, Region::new(1200.0, 1210.0));

        r.position = 1200.5;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("100002"));
        assert!((r.modifiers.target_speed.total() - 0.45).abs() < 1e-9);
    }

    #[test]
    fn forced_position_synthesizes_trigger_when_none_is_built() {
        // A condition the parser cannot handle aborts trigger building
        // entirely (`build_skill_data` returns no triggers), so there is
        // nothing for the forced position to override. The runner must
        // synthesize a forced pending entry from the alternative's effects.
        let skill = target_speed_skill("100003", SkillRarity::Gold, "unknown_token_xyz==1");

        let mut control = runner_with_skills(vec![skill.clone()]);
        prepare(&mut control);
        assert!(control.pending_skills.is_empty(), "control has no trigger");

        let mut r =
            runner_with_skills_forced(vec![skill], HashMap::from([("100003".to_owned(), 800.0)]));
        prepare(&mut r);
        assert_eq!(r.pending_skills.len(), 1);
        assert!(r.pending_skills[0].forced);
        assert_eq!(r.pending_skills[0].trigger, Region::new(800.0, 810.0));

        r.position = 800.5;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("100003"));
    }

    #[test]
    fn activation_records_green_tags_in_ledger() {
        let mut green = target_speed_skill("200011", SkillRarity::Gold, "phase>=2");
        green.tags = vec![401, 608];
        let mut r = runner_with_skills(vec![green]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        // Tags flow through the trigger onto the pending skill.
        assert_eq!(r.pending_skills[0].tags, vec![401, 608]);
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        // The green-tagged activation is recorded for caster-context scaling.
        assert_eq!(r.activated_ledger.activated_green_count(), 1);
    }

    #[test]
    fn activation_ignores_non_green_tags_in_ledger() {
        // 99 Problems-shaped tags (404/405) are not counted greens.
        let mut non_green = target_speed_skill("202181", SkillRarity::Gold, "phase>=2");
        non_green.tags = vec![404, 405];
        let mut r = runner_with_skills(vec![non_green]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        r.process_skill_activations(&FieldView::at_gate(), 2400.0);
        assert_eq!(r.skills_activated_count, 1);
        assert_eq!(r.activated_ledger.activated_green_count(), 0);
    }

    /// Wild Wind / Speed Eater bundle a self-target buff with an opponent-facing
    /// Current Speed debuff in the same skill. The caster must receive the
    /// self-buff but never the debuff (regression: it used to self-apply the
    /// Current Speed reduction, slowing its own runner).
    fn wild_wind_like_skill(id: &str) -> Skill {
        Skill {
            skill_id: SkillId::new(id),
            rarity: SkillRarity::Gold,
            tags: vec![],
            alternatives: vec![SkillAlternative {
                base_duration: 18000.0,
                cooldown_time: None,
                duration_scaling: None,
                condition: "phase>=2".to_owned(),
                precondition: None,
                effects: vec![
                    RawSkillEffect {
                        modifier: 3500.0,
                        target: SkillTarget::SelfTarget,
                        effect_type: 27, // TargetSpeed (self buff)
                        value_usage: None,
                        value_level_usage: None,
                        pre_applied_multiplier: None,
                        additional_activate_type: None,
                    },
                    RawSkillEffect {
                        modifier: -1500.0,
                        target: SkillTarget::All,
                        effect_type: 21, // CurrentSpeed (opponent debuff)
                        value_usage: None,
                        value_level_usage: None,
                        pre_applied_multiplier: None,
                        additional_activate_type: None,
                    },
                ],
            }],
        }
    }

    #[test]
    fn owned_debuff_effect_is_not_self_applied() {
        let mut r = runner_with_skills(vec![wild_wind_like_skill("202131")]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;

        let field = FieldView::at_gate();
        r.process_skill_activations(&field, 2400.0);

        // Self-target buff applied.
        assert_eq!(r.target_speed_skills_active.len(), 1);
        assert!(r.modifiers.target_speed.total() > 0.0);
        // Opponent-facing Current Speed debuff must NOT land on the caster.
        assert!(r.current_speed_skills_active.is_empty());
        assert!((r.modifiers.current_speed.total()).abs() < 1e-9);
        // The skill still counts as activated.
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("202131"));
    }

    #[test]
    fn recovery_increments_heal_count() {
        let skill = Skill {
            skill_id: SkillId::new("300001"),
            rarity: SkillRarity::White,
            tags: vec![],
            alternatives: vec![SkillAlternative {
                base_duration: 0.0,
                cooldown_time: None,
                duration_scaling: None,
                condition: "phase>=2".to_owned(),
                precondition: None,
                effects: vec![RawSkillEffect {
                    modifier: 5000.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 9,
                    value_usage: None,
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        };
        let mut r = runner_with_skills(vec![skill]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        let field = FieldView::at_gate();
        r.process_skill_activations(&field, 2400.0);
        assert_eq!(r.heals_activated_count, 1);
    }

    #[test]
    fn derive_target_strategy_reads_both_hesitant_and_frenzied_tokens() {
        // Hesitant (EnemyStrategy) family.
        assert_eq!(
            derive_target_strategy("running_style_count_nige_otherself>=1"),
            Some(Strategy::FrontRunner)
        );
        // Frenzied (KakariStrategy) family — the four running styles.
        assert_eq!(
            derive_target_strategy(
                "running_style_temptation_opponent_count_nige>=1&is_temptation==0"
            ),
            Some(Strategy::FrontRunner)
        );
        assert_eq!(
            derive_target_strategy(
                "running_style_temptation_opponent_count_senko>=1&is_temptation==0"
            ),
            Some(Strategy::PaceChaser)
        );
        assert_eq!(
            derive_target_strategy(
                "running_style_temptation_opponent_count_sashi>=1&is_temptation==0"
            ),
            Some(Strategy::LateSurger)
        );
        assert_eq!(
            derive_target_strategy(
                "running_style_temptation_opponent_count_oikomi>=1&is_temptation==0"
            ),
            Some(Strategy::EndCloser)
        );
        assert_eq!(derive_target_strategy("phase>=2"), None);
    }

    #[test]
    fn change_strategy_skips_wit_check() {
        let mut r = runner_with_skills(vec![runaway_skill("phase>=2")]);
        r.strategy = Strategy::FrontRunner;
        prepare(&mut r);

        assert_eq!(r.pending_skills.len(), 1);
        assert!(r.should_skip_wit_check(&r.pending_skills[0]));
    }

    #[test]
    fn runaway_activates_at_gate_and_promotes_only_position_keep_strategy() {
        let mut r = runner_with_skills(vec![runaway_skill("running_style==1")]);
        r.strategy = Strategy::FrontRunner;
        r.position_keep_strategy = Strategy::FrontRunner;

        prepare(&mut r);

        assert_eq!(
            r.strategy,
            Strategy::FrontRunner,
            "the race-entry strategy remains unchanged"
        );
        assert_eq!(r.position_keep_strategy, Strategy::Runaway);
        assert_eq!(r.skills_activated_count, 1);
        assert!(r.used_skills.contains("202051"));
    }

    #[test]
    fn savvy_skill_builds_trigger_with_only_wisdom_effect() {
        // Regression: a Savvy skill (Wisdom Up + vision) must still produce a
        // trigger — carrying only the modeled Wisdom Up effect — instead of being
        // discarded wholesale because of the unmodeled vision effect.
        let triggers = build(&savvy_skill("201531"));
        assert_eq!(triggers.len(), 1, "the Savvy skill must still trigger");
        assert_eq!(
            triggers[0].effects.len(),
            1,
            "only the modeled effect remains"
        );
        assert_eq!(triggers[0].effects[0].effect_type, SkillType::WisdomUp);
    }

    #[test]
    fn savvy_skill_activates_at_gate_and_applies_wit_bonus() {
        // End-to-end: Pace Chaser Savvy is a green (running-style) skill, so it
        // activates at the gate during `on_prepare` and adds its wit bonus —
        // rather than silently never firing because of the bundled vision effect.
        let baseline = runner_with_skills(vec![]);
        let wit_before = baseline.adjusted_stats.wit;

        let mut r = runner_with_skills(vec![savvy_skill("201531")]);
        prepare(&mut r);

        // Green skill consumed off the pending queue at the gate.
        assert!(r.pending_skills.is_empty());
        assert_eq!(
            r.skills_activated_count, 1,
            "Savvy must fire, not be dropped"
        );
        assert!(r.used_skills.contains("201531"));
        assert_eq!(
            r.adjusted_stats.wit,
            wit_before + 60.0,
            "the modeled Wisdom Up (+60) must apply"
        );
    }

    #[test]
    fn receive_targeted_effect_applies_current_speed() {
        let mut r = runner_with_skills(vec![]);
        prepare(&mut r);
        let effects = vec![ResolvedSkillEffect {
            target: SkillTarget::All,
            effect_type: SkillType::CurrentSpeed,
            base_duration: 3.0,
            modifier: -0.5,
        }];
        r.receive_targeted_effect(SkillId::new("999"), effects, RunnerId(5), 2400.0);
        assert_eq!(r.targeted_current_speed_active.len(), 1);
        assert!(r.modifiers.current_speed.total() < 0.0);
        assert_eq!(r.used_targeted_skills.len(), 1);
        // The receiver applies the caster-resolved value verbatim: no re-roll,
        // no re-resolution (enforced by the ResolvedSkillEffect type).
        assert_eq!(r.targeted_current_speed_active[0].skill.modifier, -0.5);
    }

    #[test]
    fn active_skill_expires_and_reverses_modifier() {
        let mut r = runner_with_skills(vec![target_speed_skill(
            "100001",
            SkillRarity::Gold,
            "phase>=2",
        )]);
        prepare(&mut r);
        r.wit_checks_enabled = false;
        let trigger = r.pending_skills[0].trigger;
        r.position = trigger.start + 0.5;
        let field = FieldView::at_gate();
        r.process_skill_activations(&field, 2400.0);
        let applied = r.modifiers.target_speed.total();
        assert!(applied > 0.0);
        r.target_speed_skills_active[0].duration_timer.t = 0.0;
        r.process_skill_activations(&field, 2400.0);
        assert!(r.target_speed_skills_active.is_empty());
        assert!(r.modifiers.target_speed.total().abs() < 1e-9);
    }
}
