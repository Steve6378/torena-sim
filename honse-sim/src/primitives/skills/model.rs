//! Skill **value objects** and runtime structs: [`Skill`], [`SkillAlternative`],
//! [`SkillEffectSpec`], [`ResolvedSkillEffect`], and the pending/active trigger
//! types.
//!
//! Ports `skills/skill.types.ts`. The input DTOs ([`Skill`], [`SkillAlternative`],
//! [`RawSkillEffect`]) arrive from the TypeScript data layer and so derive serde
//! with `camelCase` field names. The runtime structs ([`SkillTrigger`],
//! [`PendingSkill`], [`ActiveSkill`], …) live entirely inside the simulation and
//! carry domain types ([`ActivationSamplePolicy`], [`DynamicCondition`]) that do
//! not cross the boundary, so they do not derive serde.

use serde::{Deserialize, Serialize};

use crate::shared_kernel::ids::{RunnerId, SkillId};
use crate::shared_kernel::language::Strategy;
use crate::shared_kernel::math::Timer;
use crate::shared_kernel::region::{Region, RegionList};
use crate::skills::activation::ActivationSamplePolicy;
use crate::skills::condition::dynamic::DynamicCondition;
use crate::skills::effect::{SkillRarity, SkillTarget, SkillType};
use crate::skills::value_scaling::ValueScalingPolicy;

/// Raw effect as it appears in the skill data, before duration is attached and
/// modifiers are scaled.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawSkillEffect {
    /// Effect strength in raw (×10000) units.
    pub modifier: f64,
    /// Target selector.
    pub target: SkillTarget,
    /// Raw effect type id (mapped to [`SkillType`] when built).
    #[serde(rename = "type")]
    pub effect_type: i32,
    /// Optional usage discriminator (e.g. recovery sub-mode).
    #[serde(default)]
    pub value_usage: Option<i32>,
    /// Optional level-usage discriminator.
    #[serde(default)]
    pub value_level_usage: Option<i32>,
    /// The value-scaling tier multiplier the data extract has **already applied**
    /// to `modifier`, when it did.
    ///
    /// Only meaningful for the tiered usages ([`ValueScalingPolicy::PreAppliedTier`]).
    /// Its presence is the authority on whether pre-application happened: the
    /// extract gates that per *skill*, not per usage, so the same usage can arrive
    /// pre-scaled on one skill and raw on another. Absent on a tiered usage means
    /// the tier is unknown and the effect is dropped — the engine never assumes a
    /// tier it was not told about, because assuming would understate the effect by
    /// up to 1.2x with no way to notice.
    #[serde(default)]
    pub pre_applied_multiplier: Option<f64>,
    /// Additional activation (`additional_activate_type`; mechanics doc §
    /// Additional Activate): the effect does nothing at activation and is
    /// applied, for the skill's remaining duration, each time its trigger
    /// fires while the skill is active. 1 OrderUp (each overtake, up to 3),
    /// 2 ActivateAnySkill type 1 (each other skill activated, up to 3), 3
    /// type 2 (up to 2). `None` = applied at activation.
    #[serde(default)]
    pub additional_activate_type: Option<i32>,
}

/// A single alternative (condition branch) of a skill's effect data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAlternative {
    /// Base duration in raw (×10000) units.
    pub base_duration: f64,
    /// Optional cooldown between activations (raw units).
    #[serde(default)]
    pub cooldown_time: Option<f64>,
    /// Duration scaling code (`ability_time_usage`; mechanics doc § Duration
    /// Scaling). `None` or 1 = Direct. 2 = distance behind the leader, 3 and 7
    /// = remaining HP tables, applied at activation. Other codes run unscaled.
    #[serde(default)]
    pub duration_scaling: Option<i32>,
    /// The activation condition DSL string.
    pub condition: String,
    /// Optional precondition DSL string.
    #[serde(default)]
    pub precondition: Option<String>,
    /// Raw effects this alternative applies.
    pub effects: Vec<RawSkillEffect>,
}

/// A skill as loaded from the data layer (input DTO).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    /// Skill identifier.
    pub skill_id: SkillId,
    /// Rarity tier.
    pub rarity: SkillRarity,
    /// Authoritative master-data tags carried from `skill_data.tag_id`.
    #[serde(default)]
    pub tags: Vec<i32>,
    /// Condition branches; the first satisfiable one is used.
    pub alternatives: Vec<SkillAlternative>,
}

/// A built, **normalized** effect specification (value object). `base_duration`
/// and `modifier` are in real units (the raw ×10000 values divided by 10000).
///
/// A spec carries a *base* modifier and the [`ValueScalingPolicy`] that governs
/// how the runtime modifier is derived from it. It is deliberately distinct from
/// [`ResolvedSkillEffect`] so a resolved value can never be re-resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkillEffectSpec {
    /// Target selector.
    pub target: SkillTarget,
    /// Effect type.
    pub effect_type: SkillType,
    /// Effect duration in seconds.
    pub base_duration: f64,
    /// Base effect strength in real units (before value scaling).
    pub modifier: f64,
    /// How the runtime modifier is derived from `modifier`.
    pub value_scaling: ValueScalingPolicy,
    /// Additional activation trigger (see
    /// [`RawSkillEffect::additional_activate_type`]); `None` = at activation.
    pub additional_activate_type: Option<i32>,
    /// Optional level-usage discriminator (carried only; level scaling is out of
    /// scope for value resolution).
    pub value_level_usage: Option<i32>,
}

/// A runtime-**resolved** effect: the value-scaling policy has already been
/// applied, yielding a concrete `modifier`. Produced from a [`SkillEffectSpec`]
/// exactly once by the caster before self/target routing, so it can never be
/// resolved again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedSkillEffect {
    /// Target selector.
    pub target: SkillTarget,
    /// Effect type.
    pub effect_type: SkillType,
    /// Effect duration in seconds.
    pub base_duration: f64,
    /// Resolved effect strength in real units.
    pub modifier: f64,
}

/// Why the engine cannot faithfully model a raw effect, and so drops it.
///
/// Every variant means "this effect does not participate in the simulation at
/// all". None is coerced to a stand-in value: an unmodeled effect contributes
/// nothing rather than contributing a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmodeledEffect {
    /// The raw type id has no [`SkillType`] mapping (vision `8`, temptation
    /// `13`, carnival/event `502`/`503`, ...). Carries the raw id.
    EffectType(i32),
    /// The `value_usage` has no [`ValueScalingPolicy`] mapping, so the runtime
    /// value cannot be derived. Carries the raw usage.
    ValueUsage(i32),
    /// A [`ValueScalingPolicy::PreAppliedTier`] usage arrived without a
    /// `pre_applied_multiplier`, so which tier (if any) is baked into `modifier`
    /// is unknown. Carries the raw usage.
    ///
    /// Distinct from [`Self::ValueUsage`]: the engine models this usage, and the
    /// fix is for the data to state the multiplier it applied rather than for the
    /// engine to add a policy.
    MissingPreAppliedMultiplier(i32),
}

/// Classify one raw effect: the built spec, or why it cannot be modeled.
///
/// The single decision point behind both [`build_skill_effects`] (which keeps
/// the `Ok`s) and [`unmodeled_effects`] (which reports the `Err`s), so a
/// consumer-facing support report can never disagree with what the simulation
/// actually runs.
///
/// Effect type is checked before value usage, so an effect failing both is
/// reported as [`UnmodeledEffect::EffectType`].
fn classify_effect(
    effect: &RawSkillEffect,
    base_duration: f64,
) -> Result<SkillEffectSpec, UnmodeledEffect> {
    let effect_type = SkillType::try_from(effect.effect_type)
        .map_err(|_| UnmodeledEffect::EffectType(effect.effect_type))?;
    let value_scaling = ValueScalingPolicy::from_value_usage(effect.value_usage)
        .map_err(|e| UnmodeledEffect::ValueUsage(e.0))?;
    // A tiered value is only trustworthy when the data says which tier it already
    // carries. Passing an unannotated modifier through would silently understate
    // the effect by up to 1.2x, which is worse than dropping it: a plausible
    // wrong number instead of an obviously missing one.
    if value_scaling == ValueScalingPolicy::PreAppliedTier
        && effect.pre_applied_multiplier.is_none()
    {
        return Err(UnmodeledEffect::MissingPreAppliedMultiplier(
            effect.value_usage.unwrap_or_default(),
        ));
    }
    Ok(SkillEffectSpec {
        target: effect.target,
        effect_type,
        base_duration,
        modifier: effect.modifier / 10000.0,
        value_scaling,
        additional_activate_type: effect.additional_activate_type,
        value_level_usage: effect.value_level_usage,
    })
}

/// Build the scaled [`SkillEffectSpec`]s for one alternative.
///
/// Port of `buildSkillEffects`: `base_duration` and every `modifier` are divided
/// by `10000`, and the raw type id is resolved to a [`SkillType`].
///
/// Effects the engine cannot model are **skipped**, not fatal — see
/// [`UnmodeledEffect`] for the two reasons. The game bundles unmodeled effects
/// alongside ones we do model — e.g. every Savvy skill carries Wisdom Up (`5`)
/// *and* a vision effect (`8`). Rejecting the whole alternative silently killed
/// the entire skill (no trigger, no stat bonus, no proc). Dropping only the
/// unmodeled effect keeps the known ones live. An alternative whose effects are
/// *all* unmodeled yields an empty list, which `build_skill_data` treats as "no
/// trigger" (correct — there is nothing to simulate).
///
/// Dropping loses a contribution; coercing an unsupported `value_usage` to
/// `Direct` would instead invent one, applying a tier we cannot cite as though it
/// were the measured value. So dropping is deliberate, and a usage only becomes
/// [`ValueScalingPolicy::PreAppliedTier`] when the mechanics reference
/// documents the tier the extract bakes in. [`unmodeled_effects`] reports what
/// was lost.
pub fn build_skill_effects(alt: &SkillAlternative) -> Vec<SkillEffectSpec> {
    let base_duration = alt.base_duration / 10000.0;
    alt.effects
        .iter()
        .filter_map(|effect| classify_effect(effect, base_duration).ok())
        .collect()
}

/// The effects of `alt` that [`build_skill_effects`] drops, as
/// `(effect_index, reason)` over the alternative's raw effect list.
///
/// Empty means the alternative is modeled in full. Exposed so a consumer can
/// tell a user that a skill is only partially simulated instead of letting the
/// gap pass unnoticed.
pub fn unmodeled_effects(alt: &SkillAlternative) -> Vec<(usize, UnmodeledEffect)> {
    let base_duration = alt.base_duration / 10000.0;
    alt.effects
        .iter()
        .enumerate()
        .filter_map(|(index, effect)| {
            classify_effect(effect, base_duration)
                .err()
                .map(|reason| (index, reason))
        })
        .collect()
}

/// Where a targeted (injected) skill originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetedSkillOrigin {
    /// Injected externally (e.g. a debuff test harness).
    Injection,
    /// Cast by another runner during the race.
    Runner,
}

/// A skill prepared for activation: its sampled trigger windows plus the runtime
/// predicate that gates it.
///
/// No `PartialEq`: it holds a dynamic-condition closure (see
/// [`DynamicCondition`]). `extra_condition` is `None` when no runtime gate is
/// needed (`kTrue`).
#[derive(Debug, Clone)]
pub struct SkillTrigger {
    /// Skill identifier.
    pub skill_id: SkillId,
    /// Rarity tier (1★/2★ uniques, upgrades, and 3★ uniques differ here).
    pub rarity: SkillRarity,
    /// Authoritative master-data tags (`skill_data.tag_id`), carried so the
    /// activated-skill ledger can classify green activations.
    pub tags: Vec<i32>,
    /// How activation windows are sampled.
    pub sample_policy: ActivationSamplePolicy,
    /// Candidate activation windows.
    pub regions: RegionList,
    /// Effects applied on activation.
    pub effects: Vec<SkillEffectSpec>,
    /// Extra runtime gate evaluated each tick (`None` == always-true).
    pub extra_condition: Option<DynamicCondition>,
    /// For `EnemyStrategy`-targeted (and Kakari-strategy) external debuffs, the
    /// running style the debuff hits — derived from the activation condition
    /// (`running_style_count_<style>_otherself`), since the effect data does not
    /// carry it. `None` for skills whose targeting needs no strategy.
    pub target_strategy: Option<Strategy>,
    /// The alternative's duration scaling code (see
    /// [`SkillAlternative::duration_scaling`]).
    pub duration_scaling: Option<i32>,
    /// The alternative's base cooldown (raw x10000 seconds), if any.
    pub cooldown_time: Option<f64>,
    /// The runtime half of the alternative's precondition, if it has one.
    pub precondition: Option<DynamicPrecondition>,
    /// The alternative's index in the skill data when the skill has another
    /// live alternative it excludes: the game checks them in order and fires
    /// the first that holds (the recordings' skill events log that index).
    /// `None` for a skill's only live alternative, and for one naming
    /// `is_activate_other_skill_detail` / `is_used_skill_id`, which triggers
    /// on its own.
    pub exclusive_alternative: Option<usize>,
}

/// The runtime half of a skill's precondition.
///
/// A precondition's static part (course position, phase, corner) already
/// narrows the trigger to start where it first holds. Its dynamic part (order,
/// overtake targets, nearby runners, the gap to the leader) is checked here,
/// every tick the runner is inside `regions`, and once it has held the skill's
/// own condition is armed for the rest of the race. Before this, the dynamic
/// part was dropped: on the 117 recordings, the carriers of Certain Victory,
/// Lights of Vaudeville and My True Strength fired them 7%, 45% and 15% of the
/// time, and the engine about 92%, which is its wit roll and nothing else.
#[derive(Debug, Clone)]
pub struct DynamicPrecondition {
    /// Where the precondition's static part holds.
    pub regions: RegionList,
    /// Its dynamic part.
    pub check: DynamicCondition,
    /// Whether it has held yet this race.
    pub met: bool,
}

impl DynamicPrecondition {
    /// Whether the precondition allows the skill's condition to be checked.
    pub fn is_met(precondition: Option<&DynamicPrecondition>) -> bool {
        precondition.is_none_or(|p| p.met)
    }

    /// Whether `position` is inside the precondition's static regions.
    pub fn covers(&self, position: f64) -> bool {
        self.regions
            .0
            .iter()
            .any(|r| position >= r.start && position < r.end)
    }
}

/// Duration multiplier for a skill's `ability_time_usage` code, resolved at
/// activation (mechanics doc § Duration Scaling, Ability Time Usage):
///
/// - 2 MultiplyDistanceDiffTop: `min(0.8 + distance_from_top / 62.5, 1.6)`,
///   metres behind the leader.
/// - 3 MultiplyRemainHp type 1: remaining HP (absolute) < 2000 1.0x, < 2400
///   1.5x, < 2600 2.0x, < 2800 2.2x, < 3000 2.5x, < 3200 3.0x, < 3500 3.5x,
///   else 4.0x.
/// - 7 MultiplyRemainHp type 2: < 1500 1.0x, < 1800 1.5x, < 2000 2.0x,
///   < 2100 2.5x, else 3.0x.
///
/// Every other code is 1.0: `None`/1 is Direct; 4 (IncrementOrderUp) extends
/// the duration after activation rather than scaling it; 5 and 6 (blocked
/// time) are not modeled.
pub fn duration_scaling_multiplier(code: Option<i32>, hp: f64, distance_from_top: f64) -> f64 {
    fn band(value: f64, cuts: &[f64], tiers: &[f64]) -> f64 {
        cuts.iter()
            .position(|cut| value < *cut)
            .map_or(tiers[cuts.len()], |i| tiers[i])
    }
    match code {
        Some(2) => (0.8 + distance_from_top.max(0.0) / 62.5).min(1.6),
        Some(3) => band(
            hp,
            &[2000.0, 2400.0, 2600.0, 2800.0, 3000.0, 3200.0, 3500.0],
            &[1.0, 1.5, 2.0, 2.2, 2.5, 3.0, 3.5, 4.0],
        ),
        Some(7) => band(
            hp,
            &[1500.0, 1800.0, 2000.0, 2100.0],
            &[1.0, 1.5, 2.0, 2.5, 3.0],
        ),
        _ => 1.0,
    }
}

/// An additional-activation effect (mechanics doc § Additional Activate),
/// held back when its skill activates and applied, for the skill's remaining
/// duration, each time its trigger fires while the skill is active.
#[derive(Debug, Clone)]
pub struct HeldAdditionalEffect {
    /// The skill that carries the effect (identity for the active-skill lists).
    pub skill: PendingSkill,
    /// The held effect.
    pub spec: SkillEffectSpec,
    /// `additional_activate_type`: 1 OrderUp, 2 / 3 ActivateAnySkill.
    pub trigger: i32,
    /// Firings left (3 for types 1 and 2, 2 for type 3).
    pub remaining: u32,
    /// The skill's remaining duration, counting up to 0 like an active skill's.
    pub timer: Timer,
}

impl HeldAdditionalEffect {
    /// Firing limit per trigger type, from the doc.
    pub fn limit(trigger: i32) -> u32 {
        if trigger == 3 {
            2
        } else {
            3
        }
    }
}

/// Duration code 4 (IncrementOrderUp) for one running skill: each overtake
/// while it is active lengthens every modifier it applied.
#[derive(Debug, Clone)]
pub struct OrderUpExtension {
    /// The running skill.
    pub skill_id: SkillId,
    /// Extensions left (the doc: up to 3 times).
    pub remaining: u32,
    /// Seconds added per overtake: 1 s x course distance / 1000.
    pub seconds: f64,
    /// The skill's remaining duration, counting up to 0.
    pub timer: Timer,
}

/// A skill whose trigger point has been fixed, awaiting the runner reaching it.
///
/// No `PartialEq`: it holds a dynamic-condition closure.
#[derive(Debug, Clone)]
pub struct PendingSkill {
    /// Skill identifier.
    pub skill_id: SkillId,
    /// Rarity tier.
    pub rarity: SkillRarity,
    /// Authoritative master-data tags (`skill_data.tag_id`), recorded into the
    /// activated-skill ledger when this skill activates.
    pub tags: Vec<i32>,
    /// The concrete trigger window.
    pub trigger: Region,
    /// Effects applied on activation.
    pub effects: Vec<SkillEffectSpec>,
    /// Extra runtime gate evaluated each tick (`None` == always-true).
    pub extra_condition: Option<DynamicCondition>,
    /// Derived target running style for `EnemyStrategy` external debuffs (see
    /// [`SkillTrigger::target_strategy`]).
    pub target_strategy: Option<Strategy>,
    /// The alternative's duration scaling code (see
    /// [`SkillAlternative::duration_scaling`]).
    pub duration_scaling: Option<i32>,
    /// Real cooldown in seconds (mechanics doc § Skill Cooldown: base x course
    /// distance / 1000); 0 = the skill activates at most once.
    pub cooldown: f64,
    /// Later trigger windows, position-ordered (all_corner_random places up
    /// to 4); the skill moves to the next when it passes the current one.
    pub later_triggers: Vec<Region>,
    /// Race time of the first tick a cooled-down skill may activate again on:
    /// its last activation + its effect's duration + `cooldown` in whole ticks,
    /// starting when the effect ends (`NEG_INFINITY` until its first
    /// activation: the race clock starts at -1 s, before the gate).
    pub ready_at: f64,
    /// Whether this skill's once-per-race wit check has already passed.
    pub wit_passed: bool,
    /// User-forced activation (scripted `forcedPositions`): the skill fires
    /// unconditionally when the runner reaches its trigger window — dynamic
    /// condition gates and the wit check are bypassed, matching injected-debuff
    /// semantics.
    pub forced: bool,
    /// The runtime half of the precondition, latched once it holds.
    pub precondition: Option<DynamicPrecondition>,
    /// See [`SkillTrigger::exclusive_alternative`]: the skill's exclusive
    /// alternatives share one wit check and one cooldown, and the lowest
    /// index that holds fires.
    pub exclusive_alternative: Option<usize>,
}

/// An opponent-facing (external) debuff a runner emitted this frame, awaiting the
/// race aggregate to route it onto the resolved target runners. Emitted by the
/// caster during its update; consumed by the aggregate's coordinator pass (the
/// caster never applies it to itself).
#[derive(Debug, Clone, PartialEq)]
pub struct EmittedDebuff {
    /// The skill that produced the debuff.
    pub skill_id: SkillId,
    /// The single external-debuff effect to apply to each target, already
    /// **resolved** by the caster (its value-scaling policy has been applied
    /// against caster state). The receiving runner only scales duration by
    /// course distance; it must never re-resolve the value.
    pub effect: ResolvedSkillEffect,
    /// The effect's target selector.
    pub target: SkillTarget,
    /// Derived target running style for `EnemyStrategy`/`KakariStrategy`.
    pub target_strategy: Option<Strategy>,
}

/// A targeted (debuff/ally) skill awaiting its trigger point.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingTargetedSkill {
    /// Skill identifier.
    pub skill_id: SkillId,
    /// Where this targeted skill came from.
    pub origin: TargetedSkillOrigin,
    /// The source runner, when cast by another runner.
    pub source_runner_id: Option<RunnerId>,
    /// The concrete trigger window.
    pub trigger: Region,
    /// Effects applied on activation.
    pub effects: Vec<SkillEffectSpec>,
}

/// A currently-active effect on a runner (duration-based).
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveSkill {
    /// Skill identifier.
    pub skill_id: SkillId,
    /// Remaining-duration timer.
    pub duration_timer: Timer,
    /// Effect strength in real units.
    pub modifier: f64,
    /// Target selector.
    pub effect_target: SkillTarget,
    /// Effect type.
    pub effect_type: SkillType,
    /// Whether the current-speed effect decays naturally on expiry (adds a
    /// one-frame acceleration when it ends).
    pub natural_deceleration: bool,
}

/// A currently-active targeted (debuff/ally) effect on a runner.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTargetedSkill {
    /// The underlying active effect.
    pub skill: ActiveSkill,
    /// Where this targeted skill came from.
    pub origin: TargetedSkillOrigin,
    /// The source runner, when cast by another runner.
    pub source_runner_id: Option<RunnerId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_scaling_remaining_hp_tables_match_the_doc() {
        let f = |code, hp| duration_scaling_multiplier(Some(code), hp, 0.0);
        // Type 1 (3): boundaries are inclusive on the upper tier.
        let hp3 = [
            1999.0, 2000.0, 2399.0, 2400.0, 2600.0, 2800.0, 3000.0, 3200.0, 3499.0, 3500.0,
        ];
        let want3 = [1.0, 1.5, 1.5, 2.0, 2.2, 2.5, 3.0, 3.5, 3.5, 4.0];
        for (hp, want) in hp3.iter().zip(want3) {
            assert_eq!(f(3, *hp), want, "code 3 at {hp}");
        }
        // Type 2 (7).
        let hp7 = [1499.0, 1500.0, 1800.0, 2000.0, 2099.0, 2100.0];
        let want7 = [1.0, 1.5, 2.0, 2.5, 2.5, 3.0];
        for (hp, want) in hp7.iter().zip(want7) {
            assert_eq!(f(7, *hp), want, "code 7 at {hp}");
        }
        // Direct, absent and not-applied codes are 1.0 whatever the HP.
        for code in [None, Some(1), Some(4), Some(5), Some(6)] {
            assert_eq!(duration_scaling_multiplier(code, 4000.0, 100.0), 1.0);
        }
    }

    #[test]
    fn build_skill_effects_scales_by_10000() {
        let alt = SkillAlternative {
            base_duration: 30000.0,
            cooldown_time: None,
            duration_scaling: None,
            condition: "phase>=2".to_owned(),
            precondition: None,
            effects: vec![
                RawSkillEffect {
                    modifier: 4500.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 27,
                    value_usage: None,
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                },
                RawSkillEffect {
                    modifier: -10000.0,
                    target: SkillTarget::All,
                    effect_type: 9,
                    value_usage: Some(8),
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                },
            ],
        };
        let effects = build_skill_effects(&alt);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].base_duration, 3.0);
        assert_eq!(effects[0].modifier, 0.45);
        assert_eq!(effects[0].effect_type, SkillType::TargetSpeed);
        assert_eq!(effects[1].modifier, -1.0);
        assert_eq!(effects[1].effect_type, SkillType::Recovery);
        assert_eq!(effects[1].value_scaling, ValueScalingPolicy::MultiplyRandom);
    }

    #[test]
    fn build_skill_effects_skips_unknown_type_and_keeps_known() {
        // Regression: a Savvy-shaped alternative bundles Wisdom Up (5, modeled)
        // with a vision effect (8, unmodeled). The unknown type must be dropped,
        // not reject the whole alternative — otherwise the skill silently never
        // activates and the wit bonus is lost.
        let alt = SkillAlternative {
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
        };
        let effects = build_skill_effects(&alt);
        assert_eq!(effects.len(), 1, "the unmodeled type-8 effect is dropped");
        assert_eq!(effects[0].effect_type, SkillType::WisdomUp);
        assert_eq!(effects[0].modifier, 60.0);
    }

    #[test]
    fn unmodeled_effects_reports_exactly_what_build_drops() {
        // One effect per drop reason plus a modeled Direct one: an unsupported
        // usage (19), a tiered usage that never said which tier it carries (12),
        // and an unmodeled effect type (8). The report and the builder must
        // partition the same list — anything else means a consumer is told
        // something the simulation does not do.
        let alt = SkillAlternative {
            base_duration: 50000.0,
            cooldown_time: None,
            duration_scaling: None,
            condition: "phase>=2".to_owned(),
            precondition: None,
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
                    value_usage: Some(12),
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                },
                RawSkillEffect {
                    modifier: 500.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 8,
                    value_usage: Some(1),
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                },
                RawSkillEffect {
                    modifier: 500.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 27,
                    value_usage: Some(19),
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                },
            ],
        };

        assert_eq!(
            unmodeled_effects(&alt),
            vec![
                (1, UnmodeledEffect::MissingPreAppliedMultiplier(12)),
                (2, UnmodeledEffect::EffectType(8)),
                (3, UnmodeledEffect::ValueUsage(19)),
            ]
        );
        assert_eq!(
            build_skill_effects(&alt).len() + unmodeled_effects(&alt).len(),
            alt.effects.len(),
            "every raw effect is either built or reported, never both or neither"
        );
    }

    #[test]
    fn unmodeled_effects_is_empty_for_a_fully_modeled_alternative() {
        let alt = SkillAlternative {
            base_duration: 30000.0,
            cooldown_time: None,
            duration_scaling: None,
            condition: "phase>=2".to_owned(),
            precondition: None,
            effects: vec![RawSkillEffect {
                modifier: 4500.0,
                target: SkillTarget::SelfTarget,
                effect_type: 27,
                value_usage: None,
                value_level_usage: None,
                pre_applied_multiplier: None,
                additional_activate_type: None,
            }],
        };
        assert!(unmodeled_effects(&alt).is_empty());
    }

    #[test]
    fn build_skill_effects_all_unknown_yields_empty() {
        let alt = SkillAlternative {
            base_duration: 0.0,
            cooldown_time: None,
            duration_scaling: None,
            condition: String::new(),
            precondition: None,
            effects: vec![RawSkillEffect {
                modifier: 1.0,
                target: SkillTarget::SelfTarget,
                effect_type: 999,
                value_usage: None,
                value_level_usage: None,
                pre_applied_multiplier: None,
                additional_activate_type: None,
            }],
        };
        assert!(build_skill_effects(&alt).is_empty());
    }

    #[test]
    fn skill_dto_round_trips_with_camel_case_fields() {
        // The raw JS->domain numeric-enum mapping lives in the wasm `dto.rs`
        // boundary layer; core round-trips through its own symmetric serde
        // representation. This asserts the `camelCase` field naming and the raw
        // `type` rename survive a round trip.
        let skill = Skill {
            skill_id: SkillId::new("100012"),
            rarity: SkillRarity::Gold,
            tags: vec![401, 608],
            alternatives: vec![SkillAlternative {
                base_duration: 12000.0,
                cooldown_time: Some(2000.0),
                duration_scaling: None,
                condition: "phase>=1".to_owned(),
                precondition: None,
                effects: vec![RawSkillEffect {
                    modifier: 3000.0,
                    target: SkillTarget::SelfTarget,
                    effect_type: 27,
                    value_usage: None,
                    value_level_usage: None,
                    pre_applied_multiplier: None,
                    additional_activate_type: None,
                }],
            }],
        };
        let json = serde_json::to_string(&skill).expect("serialize");
        assert!(json.contains("\"skillId\":"), "json was: {json}");
        assert!(json.contains("\"baseDuration\":12000"));
        assert!(json.contains("\"cooldownTime\":2000"));
        assert!(json.contains("\"type\":27"));

        let reparsed: Skill = serde_json::from_str(&json).expect("parse");
        assert_eq!(reparsed, skill);
    }
}
