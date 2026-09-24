# Capture findings

What recorded races say about the engine. Every entry names the fixtures or
capture it came from and the command that reproduces it. Numbers are the
measurement, not a model of it. Where a finding is unexplained it says so.

`docs/mechanics/README.md` stays the mechanics source of truth. A finding here
that contradicts it gets a note there. The nearest so far is the near-lane
timers' lane edge (§ Near-lane timers below): the README's rule line reads
`<`, the recordings count a pair one lane apart but cannot say whether the
game's offset for it sits a hair inside the lane, and the README's note gives
the engine's `<=` reading.

## Evidence base

- 53 horseACT captures under `honse-sim-wasm/tests/fixtures/captures/`: 41
  Champions Meeting and 12 Room Match races, all Hanshin turf 1600 m (course
  10903), 9 runners each. Exported with torena-hub's `pnpm run race:fixture`.
- 117 recorded tournament races on 15 courses, kept outside this crate (§
  Tournament recordings: patches 0026-0037).
- Three Room Match captures from the same course that started this work, not in
  the fixture set: `Mihono Bourbon-74.8213s-20260811.json`,
  `Silence Suzuka-74.8617s-20260811.json`, `Taiki Shuttle-74.7314s-20260821.json`.
- The game records every frame for the first second and near the finish, and
  about one frame per second in between. Per-second HP and speed deltas below
  come from those one-second frames.
- Harness: `cargo test -p honse-sim-wasm --test capture_accuracy -- --ignored --nocapture`.
  `ACCURACY_FIXTURE=<substring>` narrows the set, `ACCURACY_TRACE=<gate>`
  prints one runner frame by frame. "Pinned" below means gate, start delay,
  skill activations, last spurt transition, rushed spells and downhill mode
  taken from the recording; the engine rolls only section variance and dueling.

## HP consumption formula: confirmed

Fit of the game's recorded HP drain per second against its recorded speed,
31,502 one-second samples, rushed frames excluded, base speed
`20 - (1600 - 2000) / 1000 = 20.4`. Multiplier is recorded drain divided by
the documented `20 * (speed - base + 12)^2 / 144`.

| Where | n | Median multiplier | After dividing by guts modifier |
|---|---|---|---|
| Early, flat | 5203 | 1.003 | |
| Mid, flat | 14328 | 1.003 | |
| Mid, on the 950 to 1350 m downhill | 2295 | 0.548 | |
| Late (2/3 to 5/6), on the downhill | 5060 | 0.571 | |
| Final sixth, flat | 1043 | 1.335 | 1.004 |
| Final sixth, uphill | 1642 | 1.333 | 1.002 |

The downhill samples are bimodal, not a uniform 0.55. Mid-race downhill peaks
at 0.4 and 1.0. Late-race downhill peaks at 0.5 and 1.3, which are 0.4 and 1.0
times the guts modifier. So: the formula is exact on flat ground, the guts
multiplier applies from two-thirds of the course, and downhill mode is the
documented 0.4 factor. The engine has all three.

## Downhill mode share: open

Share of one-second downhill samples draining under 0.7 of the formula, 16
fixtures: game 0.570 (2105 samples), engine rolling freely 0.626 (18,092
samples over 8 seeds each). The engine enters the mode a little more often
than the game. Not pursued.

## Start-dash HP drain: unexplained

Capture `Mihono Bourbon-74.8213s-20260811.json`, gate 6, first second. The game
drains 1 to 2 HP every frame from the first frame, 16 HP by 1.0 s, while speed
climbs from 3.0 to 17.9 m/s. The documented formula on current speed gives 7 HP
over the same second, and that is what the engine drains. Speed and distance
match the game to 0.01 m/s and 0.1 m through the dash.

Across the 53 fixtures the engine carries about 6 more HP than the game
through the early phase, and the offset stays. No rule for it is known.

## Runaway is running style 1 plus skill 202051: fixed

Capture `Mihono Bourbon-74.8213s-20260811.json`, Silence Suzuka at gate 4,
running style 1, stamina 560 at Bad mood (549 base). Game HP at the gate is
1977, which is `0.8 * 0.86 * 549 + 1600`, the runaway coefficient. The front
runner coefficient gives 2017.

Run as a front runner the engine had 713 HP at 41 s and hit zero before the
line. Run as a runaway it had 1159 HP at 41 s, the game's exact value, and
finished within 0.05 s and in the same place. torena-hub's importer now applies
the rule (torena-hub#49). 14 runners across 12 fixtures changed strategy.

## Replay frame zero: fixed

The game's capture has a frame at time 0 with the field standing in the gates.
The engine's replay started at the first tick, so its frame k was one tick
after the game's frame k. Matched by index, the start dash read as a +0.55 m/s
early-phase speed bias on every race; matched by time the bias is +0.03. The
replay collector now records the gate frame (torena-sim#96).

## Last spurt transition: pinned to within a frame

With `forcedLastSpurtDistance` set from the recording, spurt-start MAE is
0.96 m over 53 fixtures. The remainder is the game logging the first frame past
the transition rather than the transition itself.

## Rushed: pinned with 0.999 frame agreement

With `forcedRushedRegions` from the recorded `temptationMode` and the engine's
roll disabled, frame agreement on rushed state is 0.999.

## Late-race speed excess is overtaking time: open

After every pin, the engine is still fast in the late race for pack runners
and not for leaders. Fixture `10903-mihono-bourbon-74-2859s-20260831`, pinned:

| Runner | Style | Late speed bias | Late HP bias |
|---|---|---|---|
| Oguri Cap, gate 2 | end closer | +0.49 m/s | −149 |
| Narita Taishin | end closer | +0.32 m/s | −84 |
| Special Week | pace chaser | +0.20 m/s | −58 |
| Seiun Sky | front runner | +0.06 m/s | −55 |
| Mihono Bourbon | front runner | +0.07 m/s | +5 |

Traced, Oguri Cap at gate 2 starts a full spurt at 1068 m with the same skills
fired at the same places as the engine. The raw capture marks him blocked by
gate 6, a pace chaser 1.7 m ahead in the next lane at 20.2 m/s, on the frames
at 53.3, 54.4 and 56.5 s. His speed holds near 20.1 for two seconds and near
21.1 for two more while his lane goes from 4699 to 5877 units between 53.3 and
59.7 s to get past. Once clear he
accelerates at 0.48 m/s², the documented base for his power. The engine's copy
is clear almost at once and accelerates at 0.98 m/s²: the same base plus the
unique skill's +0.3 and a gold skill's +0.2, both of which the game also fired
but which land on a capped runner there.

## Lane trajectory: measured, model open

Lane MAE in meters from the rail, pinned, 53 fixtures, engine replay lane
scaled by course width like the game (10000 units per width, gate k at k/18):

| Strategy | Runners | Lane MAE |
|---|---|---|
| Front runner | 50 | 0.71 m |
| Pace chaser | 203 | 1.31 m |
| Late surger | 122 | 1.24 m |
| End closer | 88 | 1.51 m |
| Runaway | 14 | 0.85 m |

Traced on the same race as above, gate 2: the game moves the closer out to
2.8 m in the first five seconds and back to 0.7 m by mid-race; the engine
leaves him in his gate lane at 1.25 m and drifts in from 20 s. From 40 s both
move out; the game reaches 6.8 m, the engine stops at 4.13 m from 53 s on.

Applying the documented front-block speed cap (0.988 to 1.0 times the
blocker's speed under 2 m) without the documented target-lane rules made
things worse: pinned finish MAE 0.212 s to 0.352 s, order Spearman 0.835 to
0.612, finish bias flipped to +0.21 s. With the cap, the engine held runners
front-blocked in 9 to 20% of late-race frames by strategy; the game's recorded
blocker column is near zero in the same frames. The engine does not spread
the pack the way the game's rules do (two horse lanes from the runner inside
in mid-race, candidate lanes in overtake mode), so its pack stacks and stays
capped. Once the documented target-lane rules landed (#100) the cap went on and
stayed on: the engine resolves the front blocker with the documented 2 m taper,
closest gap wins, and caps the blocked runner's speed at 0.988 to 1.0 times the
blocker's (`primitives/runner/physics.rs`, `FrontBlock::speed_cap` and the cap in
`on_update`). The scoreboard row below records the result.

## Lane units: fixed, one constant held back

The doc's lane section measures lanes in course widths (11.25 m on this
course); the engine measured them in meters and had read the doc's constants
unscaled. Fixture `10903-mihono-bourbon-74-2859s-20260831`, gate 2, power 1140:

- Lane change speed. The game moves him out at 0.33 m/s from 41.6 to 52.2 s.
  The documented `0.02 * (0.3 + 0.001 * power)` is 0.0293 per second; in
  widths that is 0.33 m/s. The engine had moved 0.0293 m per tick.
- Final-corner lane. The corner starts at 800 m and every runner moves out
  from there, the leader included. Runners at 1.10 to 1.33 m from the rail
  settle at 6.10, 6.61 and 6.28 m; runners on the rail settle between 0.26 and
  1.04 m. The documented `clamp(lane / 0.1, 0, 1) * 0.5 + random(0.1)` in
  widths gives 5.5 to 6.6 m for the first group and 0 to 1.1 m for the second.
  The engine had used meters with no clamp, so the random part was 0.1 m and
  a runner at 1.25 m targeted 6.3 m.
- Inward drift. Normal-mode rule 4 moves the target 0.05 widths in, 0.56 m;
  the engine had used 0.05 m.

With the three in widths (torena-sim#102), pinned means over 53 fixtures:
finish MAE 0.205 to 0.203 s, trajectory 4.61 to 4.55 m, lane MAE 1.00 to
0.98 m, Spearman 0.815 to 0.809.

The pace-down lane, 0.18 in the doc, still stays in meters, but no longer
because the unit is open. It is not: the recordings settle it at **course
widths**, and the constant is held back only because correcting it exposes
defects elsewhere in the pack model that this engine cannot yet cover.

### Pace-down lane: unit settled, constant held back

The unit. Under the band where normal mode is forced there are three
paced-down runner-races on the rail with nobody inside them and no front
blocker, so rule 2 is the only rule that can move them. Two of the three march
outward for four to five seconds at the documented lane-change speed for their
Power and reach 1639 and 1498 lane units (1/10000 course width); 0.18 m is 160
units, 0.18 widths is 1800. A runner still stepping at the full lane-change
speed has not reached the 0.5-horse-lane refresh radius (278 units), which
bounds the target at 1758 units or more in the first case. Of 1379 matched
runner-frames in the same state but not paced down, one moves out 1200 units
and none 1500. `0.18` is course widths; the metres reading cannot produce
either march.

The regression, reproduced. Scaling rule 2 by the course width and changing
nothing else, against the fork baseline (pinned, 53 fixtures, 8 seeds):

| | finish MAE | trajectory MAE | lane MAE |
|---|---|---|---|
| fork baseline | 0.203 s | 4.545 m | 0.985 m |
| rule 2 in widths | 0.217 s | 4.740 m | 0.972 m |

The lane trajectory improves, including on both fixtures whose marches settled
the unit (`nishino-flower-74-8612s` lane MAE 1.05 to 0.94 m,
`yukino-bijin-74-7362s` 0.98 to 0.73 m). The finish times do not:
`10903-special-week-74-3953s-20260830` goes from 0.208 to 0.626 s, its whole
field 0.2 to 0.3 m/s slow through the mid-race. Per-tick traces of that fixture
name the mechanism. At the first position-keep check six of the nine runners
are already inside the pacemaker's minimum distance, so with rule 2 in widths
six targets converge on the single lane 2.025 m and the field jams there;
paced-down time over sections 1 to 10 rises from 11.8 to 21.3% of runner
distance and pace-up time halves, which is the whole 0.2 m/s. The pacemaker
herself ends up front-blocked by a paced-down runner at a 0.3 m lane gap and
held to 0.988x her pace with no free candidate lane to escape into.

Two documented gaps the correct constant exposes, both measured:

- **Side blocking is read as a corridor, not a window.** The doc blocks on a
  side only for a runner within 1.05 m along the course *and* under two horse
  lanes across (§ Side Blocking); `side_space_free` blocks on any runner
  anywhere between the runner and her target lane. With a target 0.18 m away
  the two agree; with the target 2.025 m away the engine freezes runners the
  doc leaves free. Adding the two-horse-lane window takes pinned finish MAE to
  0.209 s, lane MAE to 0.967 m, trajectory to 4.645 m, and that fixture from
  0.626 to 0.246 s. It is not the whole difference. Reading the same section's
  "the uma with lowest lane gap determines how much space is available for
  movement" as a movement bound instead (move up to the nearest side runner's
  lane) is refuted outright: 0.292 s, the field over-spreads.
- **Normal mode is never forced.** The doc uses normal mode "when there are no
  overtake targets, **or** when uma is within 200 m before the move lane point
  during early-race or mid-race" (§ Normal Mode); `resolve_target_lane` only
  ever checks the first clause, so in that band the engine picks overtake
  candidate lanes where the game runs the normal rules — which is where both
  measured marches happen. Forcing normal mode there is the best lane
  trajectory measured (pinned lane MAE 0.939 m) and still costs finish MAE
  (0.219 s alone, 0.209 s with the window above).

What is still missing, and why the constant stays in meters. § Position, World
Transform: `DistanceAdd_world = DistanceAdd_course / max(1, ratio_prev /
ratio_base)` — running wide while cornering, or moving lanes, costs course
distance. The engine charges nothing for it. With rule 2 at 0.18 m nobody ran
wide during position keeping and the gap was invisible; with the documented
0.18 widths a large part of the field spends the mid-race 2 m off the rail and
the engine gives them that width free, while the runners in the recordings pay
for it. The ratio is a property of the course's 1001 keyframes (or at least
each corner's radius), and neither the capture params nor the course DTO carry
any of it, so it cannot be transcribed here. Until it can, flipping rule 2
trades 0.006 s of finish MAE (0.203 to 0.209 s at best, 96 per-fixture
baseline regressions) for 0.018 m of lane MAE, and the flip is not landed.

Refuted while looking: making the first position-keep entry check happen at
2 s rather than on the first tick, which the 2-second cadence in § Position
Keeping could be read to require, costs 0.034 s of pinned finish MAE with the
constant in either unit, so the engine's first-tick check is load-bearing and
is not the defect.

Firing normal-mode rule 3 from the final corner as well as the final straight,
which the leader's move at 800 m suggests, changed nothing: finish MAE 0.206 s,
lane MAE 0.99 m. Not applied.

## Closer's corner lane: open

Same fixture, gate 2, after the unit fix. The game holds him at 1.10 m from
8.5 s to 40 s, which is 1.76 horse lanes off the runners on the rail, the
edge of normal-mode rule 5. The engine drifts him to 0.42 m by 40 s. The
final-corner lane is set from the lane at the corner entry, so the game sends
him to 6.6 m and the engine to 2.3 m, and he spurts inside the pack instead
of around it. He finishes 0.92 s early in the engine.

## Career races: excluded

Career runners carry debut-level stats (speed 92, stamina 115 in one capture)
and the game grants +400 adjusted stats in single mode. Without that rule the
engine finished 7 to 11 s slow on 20 career captures. The exporter skips
`Single` and `Legend` captures.

## Tournament recordings: patches 0026-0037

Patches 0026-0037 each transcribe a rule the 117 tournament recordings
settle. The README sections they touch carry a short note; the evidence is
here, one entry per rule. Tape figures are read off the recordings and do not
depend on a build. Engine figures name the build they were measured on, by
patch number and, where a commit id would not survive the fork being rebuilt
from its patch files, by tree id. "The series head" is patch 0037's engine as
measured (tree df1e96b); 0036's engine is tree 1dcf01a and ulc.25 tree
acec6f6. The clean-up later folded into 0026-0037 (shared comparators, one
grace constant, tests and comments) leaves the census at 50 rounds and both
harnesses identical at the series head. Patches 0038 (the finish time) and
0039 (the gate-skill stamp) come after it and have their own entries below.

### Evidence base and commands

- 117 recorded tournament races, 12 runners each (1,404 runners), on 15
  courses of 1200 to 3200 m, turf and dirt. They are kept outside this crate,
  in uma-sim-lab's `engine-fork/captures/fixtures/` (converted by its
  `engine-fork/captures/convert.py`), in the same fixture format as the 53
  here and with their own `baseline.json`. Runner i ran from gate i + 1.
- Frames: every tick in the first second and in the last 25 m, every 16
  ticks otherwise (§ The game's tick and clock). Events: skill activations
  (type 3, params [runner, skill, ..., alternative index]), CompeteTop (4),
  CompeteFight (5), ReleaseConservePower (6), each at a tick of the game's
  clock.
- Harness on these races:
  `ACCURACY_FIXTURE_DIR=<uma-sim-lab>/engine-fork/captures/fixtures ACCURACY_SAMPLES=48 cargo test --release -p honse-sim-wasm --test capture_accuracy -- --ignored --nocapture`,
  paired per fixture between two builds.
- Census (uma-sim-lab, against a package built here):
  `HONSE_SIM_DIR=<checkout>/honse-sim-wasm/pkg node scripts/probes/engine-census.mjs 200 --workers 8 --out census.json`.
  Per skill with 30 or more carriers: the share of carriers that fired it at
  least once on tape against the engine's mean share of rounds (recorded
  gates, pre-race inputs, the recorded start delay dropped), and z = (tape -
  engine) / sqrt(engine (1 - engine) / carriers). z is undefined where the
  engine fires every round or none.
- Repeats (uma-sim-lab): `node scripts/probes/skill-repeats.mjs 8`.
- Tape analyses: the investigators' scripts in uma-sim-lab's
  `engine-fork/measurements/census-residuals/`, round 1 under `scripts/`
  (one directory per investigation, A to J, and its skeptic's `verify-`
  twin) and round 2 under `round2/scripts/integrate-step-00NN/`. The
  ready-tick and tick studies behind the clock, repeat-timing and lag
  figures, and the compete_fight_count study (their scripts `t1_grid.py`,
  `r1_clock.py`, `r2_repeats.py`, `t3_kmin.py`, `r4_lag.py`, `accum.py`,
  `tape_showdown.py`), are not in that record yet; each entry below says
  what they compute.

### The game's tick and clock

Patch 0037; README § Frame Rate.

- Every frame time (15,840) and every event time (22,617: 20,874 skill,
  248 CompeteTop, 533 CompeteFight, 962 ReleaseConservePower) in the 117
  recordings is, bit for bit, a value of t(0) = 0, t(n+1) = float32(t(n) +
  float32(0.0666)). A float32 or float64 sum of 1/15 s, or 0.0666 n in
  float64, matches only the 117 frames at t = 0 (and the 4,271 skill events
  there). Reproduce: generate the sequence in float32 and look up every
  `observed.frames[].time` and `observed.events[].frameTime`.
- Consecutive frames are 1 tick apart (5,983 gaps) or 16 ticks apart
  (9,740 gaps).
- The motion steps the same tick: over 20,074 pairs of per-tick frames at
  constant speed, distance over speed has median 0.0666123 s; 14,789
  (73.7%) are within 2e-5 of 0.0666 and none within 2e-5 of 1/15.
- The clock reads 4.995 s on tick 75, 5.0616 s on tick 76, 9.990 s on tick
  150, 10.0566 s on tick 151 and 94.97075 s on tick 1426, where it runs
  0.85 ms behind 0.0666 n.
- Engine (the series head): pinned replays of 10104-r0006 (2000 m) and
  10811-r0081 (3200 m, the longest recording), 2 seeds each, put every
  recorded frame the simulated round reaches on the same float32 bits at the
  same tick (131 of 131, 127 of 127, 242 of 242, 240 of 240), and every
  distinct skill event time on the game's clock (137 and 133). On 0036's
  engine only the frame at t = 0 matched. Gate skills' events stayed on
  tick 1, where the recordings stamp them at 0 (36.5 a race), until patch
  0039 (§ Gate skills are stamped at the gate): a replay question, not the
  tick.
- Seconds rules on the new tick (the tick study's audit): a 3 s timer holds
  on its 46th tick (45 give 2.997 s), position keep's 2 s and 3 s waits take
  31 and 46 ticks, a rushed spell's snap-out marks fall on its ticks 46, 91
  and 136 and the 12 s cap on 181. Per-frame rules keep their counts.
  `lane_change_acceleration_per_frame` is a per-frame input its producers
  compute as 0.03 / 15; at 0.0666 s it runs 0.1% fast, and converting it is
  theirs to do.
- Open: which tick of each second the downhill check falls on (the engine
  keeps the port's last tick before each whole second of the clock).

### The race clock starts at the gate

Patches 0035 and 0037; README § accumulatetime, § order_rate.

- Tape: accumulatetime>=5 skills first fire on tick 76 (5.0616 s) in 376 of
  their 999 first firings, accumulatetime>=10 skills on tick 151 (10.0566 s)
  in 25 of 1,094, none earlier (`accum.py`: each carrier's first firing of
  every skill whose condition names accumulatetime>=N, on the game's clock).
  So the condition reads the race clock from 0 at the gate and holds on the
  first tick whose clock reaches N s.
- Engine: the runner clock started at -1 s until 0035, so accumulatetime>=N
  opened at N + 1 s and the `*_continue` 5 s grace ended at 6 s of race.
  Round 1 (investigator H) found one carrier whose band broke at 5.27 s and
  who did not fire. On the series head (4 rounds a race): >=5 opens on tick
  76 in 1,360 of 3,919 first firings, >=10 on tick 151 in 110 of 4,168, none
  earlier. 0036's engine opened >=5 on tick 75 (5.000000000000002 s).

### A condition fires on the tick it first holds

Patch 0035; README § Skill Conditions.

- Tape: the accumulatetime firings above come on the first tick the
  condition holds. Uma Stan (201591, near_count>=3&accumulatetime>=5) fires
  for 424 of 486 carriers, 252 of them (59.4%) on tick 76, median 89 m into
  the race (round 1, investigator B). Now We're Cruisin'! (100341, and its
  inherited 900341; compete_fight_count>0) fires in all 15 of its firings (12
  of 37 carriers of 100341, 3 of 9 of 900341) one recorded tick after the
  carrier's own first CompeteFight event; every carrier who dueled fired and
  none who did not (`tape_showdown.py`: each carrier's first type-5 event
  against her first firing).
- Engine: until 0035 the 23 tokens only the live race resolves were armed
  at a random point drawn into their window (Erlang k 3, rate 2; k 1, rate 2
  for is_overtake; k 5, rate 1 for is_move_lane; a uniform point for
  compete_fight_count) and checked from there. On ulc.25, 4.9% of 201591's
  firings landed on the first eligible tick and its median was 281 m. The
  contested engine now checks each region of their window from its first
  tick (the FirstTick policy).
- Engine, the series head, census at 200 rounds: 201591 81.13% against tape
  87.2% (z +3.45; ulc.25 75.3%); 100341 28.82% against 32.4% (z +0.48).
- Vacuum engine: it runs no field, so these tokens fall back to static
  filters that hold everywhere; FirstTick would fire every sample at the
  window's start (20 of 20 samples of phase>=2&is_overtake==1 at 1600 m on a
  2400 m test course). It keeps the port's policies. A probe of the three
  tokens is_overtake, near_count and is_move_lane after phase>=2 on that
  course places the same 120 triggers on ulc.25, on 0036's engine and on
  the series head: placement happens before the race runs.
- Open: 202401/202402 fire in the first 4 m past 2/3 of the course in
  22.7/23.4% of engine firings against the tape's 4.3/3.5% (measured on
  0035's first build); is_move_lane holds on the engine's second tick, where
  the tape's carriers of 201331/201332 fire at a median 78 m (on 0035's
  first build, tree 3f8b592). Among 100341/900341's carriers the engine's
  duels start 65.4/164.5/265.9 m (p10/50/90) past the final straight's
  start, the tape's 15 at 82.9/101.9/142.0 m (measured on 0035's first
  build with its compete_fight_count change, tree af9bd49). Not
  determined.

### A skill's cooldown starts when its effect ends

Patches 0032 and 0037; README § Skill Cooldown.

- Tape: 43 repeats (every case of one runner firing one skill twice; 43
  runners in 23 races, none three times): 201662 22, 200331 12, 201651 5,
  200332 3, 200342 1. Durations and cooldowns from each runner's own fixture
  entry (one alternative each, none scaled). All 43 come after activation +
  duration + cooldown, none between activation + cooldown and that point;
  measured from activation + cooldown the earliest is 4.88 s later.
- In game ticks (n1 the first firing's tick): none comes before n1 +
  ceil(duration / 0.0666) + ceil(cooldown / 0.0666), and 6 come on it.
  Three See Ya Later! repeats whose condition already held there fired
  exactly on it: 10908-r0039 runner 2 (2600 m, 1290 = 118 + 1172 ticks),
  10611-r0077 runner 0 (1600 m, 794 = 73 + 721), 10104-r0045 runner 2
  (2000 m, 992 = 91 + 901). 10908-r0039 is decided by recorded frames alone
  (the spell was at least 85 ticks old); 10611-r0077 by frames and the
  game's own firing; 10104-r0045 by frames and two measured quantities, the
  2.5 m along bound and the 46-tick near-lane lag. 10504-r0065 runner 6
  fired on the tick whatever her condition did, which caps the rule there
  and rules out "the tick after the first tick whose clock reaches ready"
  (1379 against 1378).
- One other form fits all 43: n1 + ceil((duration + cooldown) / 0.0666) +
  1. Among the skills that can re-arm in a race (30 s base cooldown; the
  rest have 500 s), the two differ, a tick apart, on the recorded courses
  for the 3 s skills at 1700 m, for the 1.8 s skills 200461/200462 at 1400,
  1700 and 1800 m (carriers there: 200461 3/1/2, 200462 37/27/59) and for
  the 2.4 s corner skills 200331/200332 at 1400, 1600, 1700 and 1800 m. The
  one 1700 m repeat fired 8 ticks late on its own timer, every corner repeat
  is at 2500 or 3200 m, and 200461/200462 never repeat on the recordings.
  Not determined.
- Scripts: the ready-tick study (`t2_repeats.py`, `r2_repeats.py`: the
  repeat table on the game's clock; `t3_nearlane.py`, `t3_corner.py`: the
  condition at the ready tick), and round 1's investigator C and its
  skeptic (`scripts/C-lane-time`, `scripts/verify-C-lane-time`), who
  predicted the near-lane repeats from the timers: re-armed at activation +
  duration + cooldown, 22 of 22 See Ya Later! and 5 of 5 Slipstream
  repeats, none extra; at activation + cooldown, 44 and 9.
- Engine, the series head, `skill-repeats.mjs 8` (runners who fired a skill
  twice or more / runners who fired it; tape, engine): 201662 22/726,
  145/5390 (z +0.57); 201651 5/119, 41/980 (+0.01); 200331 12/998, 93/7965
  (+0.10); 200332 3/115, 36/872 (-0.82); 200342 1/19, 1/178 (+2.74); 201652
  0/26, 6/156 (-1.02); 201661 0/54, 8/386 (-1.07). z is undefined for the
  nine other 30 s skills, which repeat on neither side (200341, 200361,
  200362, 200371, 200372, 200461, 200462, 200491, 200492). All skills but
  the two 500 s additional activations (100531, 110351): tape 0.207%,
  engine 0.199%. Timing, 4 rounds a race: 183 engine repeats, none before
  the tape's rule tick, 40 on it (tape: 43, none before, 6 on it). On 0036's
  engine (1/15 s tick), 31 of 170 fired again sooner than the rule allows,
  by 0.011 to 0.119 s.
- On a 1/15 s tick the 30 s base cooldown is a whole number of ticks on
  every course that is a multiple of 20 m, so the count fell on the tick
  where duration + cooldown had elapsed; the recordings' extra tick comes
  back only with the game's tick (0037). The effect's own float timer ran
  one tick past ceil(duration / tick) for 31 of the 95 (base duration,
  distance) pairs the fixtures carry on 1/15 s, and for 0 of 95 on 0.0666 s.

### A skill's alternatives: the first that holds fires

Patch 0027; README § Skill Alternatives.

- Tape: the skill event's params[3] is the alternative index. It matches
  the base-power split of 202331/202332 in 131 of 131 activations, and 117
  activations over 23 skills went through a second alternative. 110101
  fired through its second (no distance_diff_top term) 10 times of 25;
  order 2 to 5 at 200 m remaining predicts 55 of its 56 carriers (the 56th
  is a 0.01 m tie). Round 1, investigator G (`scripts/G-windows-and-counts`,
  `alt_index.py`); round 2, `integrate-step-0027/alt-split.mjs`.
- Engine: until 0027 it kept a skill's first alternative with a live
  window and dropped the rest. Census at 50 rounds on 0027 with the
  ascending pass of 0026 (tree af85426: 0027 before it counted forced-gold
  candidates once per skill, which moves no census cell at 200 rounds on
  the series head of the time, the first fold of 0026-0035, tree
  1498499): 110101 44.3% against
  tape 44.6% (0026: 28.3%); where the two alternatives' effects differ, the
  engine's share of firings through the second matches the tape's on all
  ten skills (|z| <= 1.3; 110101 450 of 1240 against 10 of 25), and no round
  fired both. The series head at 200 rounds: 110101 45.16% (z -0.08).

### Skills are checked in ascending id order

Patch 0026; README § activate_count_x.

- Tape: 920011 (is_activate_any_skill) fires on the frame one of the
  runner's lower-id skills fires, 5 of 5; 120011 one frame after a higher-id
  one, 10 of 10 (round 1, investigator A, `scripts/A-never-fires-and-token-sweep`).
- Engine: the pass walked pending skills from the highest id down until
  0026. On the capture harness at 48 seeds the ascending pass alone moved
  free winner hit -1.10 +- 0.49 points (t -2.26) on 117 fixtures, measured
  as a fixup (tree e2f0116) on the series' first build (tree 3f8b592); no
  other metric passed |t| 2 but pinned finish bias, on 5 fixtures.

### Condition tokens read as GameTora defines them

Patch 0026; README § Other condition tokens. Round 1, investigator A and
its skeptic (`scripts/A-never-fires-and-token-sweep`,
`scripts/verify-A-never-fires-and-token-sweep`), evaluating GameTora's
definitions on the frames.

| Token | Engine until 0026 | Tape |
|---|---|---|
| change_order_up_* | a 0/1 "placing improved" flag compared with 2 or 3: never held | passes since 2/3 of the course: 100191 58/59 fired against 0/14; 900171 29/31 against 0/25 |
| temptation_count | runners rushed across the field on this tick | own spells: gated on ==0, 0 of 44 fired after a rush of their own, 142 of 384 never rushed fired |
| distance_diff_rate | gap to the leader over the course length | over the field's spread: 56 of 62 whose window met it fired, 0 of 18 |
| running_style_count_same(_rate) | exact strategy; a fraction against a percentage | runaway = front, a percentage: 200282 39/39 at 40% or more, 0/18 below |
| post_number | the 0-based gate read as a block (12 runners: 0-8, 8, 7, 6) | the JRA gate block: 200252 7/7 against 0/16, 200262 59/59 against 0/56 |
| running_style_equal_popularity_one | the runner in the first gate | the popularity-1 runner: 200292 13/13 sharing her style fired, 0/10 others |
| lane_type | ignored (always true) | course widths 0.2/0.4/0.6: all 40 activations of 200752 at 0.2 or less |
| is_move_lane | any lateral move is 1, 2 never held | not separable: every carried skill takes either |
| is_activate_any_skill | any skill ever fired | another fired this tick or the last: 15 of 15 activations of 120011/920011 |
| is_surrounded | 1 lane front and behind, no outside clause | 3 carriers, not testable |

Engine, the series head, census at 200 rounds: 100191 83.37% against tape
79.5% (z -0.90), 900171 57.94% against 51.8% (z -0.93), 200282 68.42%
against 68.4% (z 0.00), 200752 81.72% against 83.0% (z +0.22), 900591
26.45% against 22.8% (z -1.42). On ulc.25 100191, 900171 and 200282 never
fired.

### The order_rate out bands keep the threshold place

Patch 0028; README § order_rate.

- Tape (12 runners; out70 is 8th or worse, in20 2nd or better): a band
  holds when every frame from 5 s to the check satisfies it (a 6 s grace
  gives the same split). 110611 (out70, 36 carriers, 31 fired): >= admits
  32 and 31 of them fired, none outside; > admits 23 and 8 fired outside
  it. 910611 (out70, 25 carriers, 17 fired): >= admits 17, all fired; >
  admits 10 and 7 fired outside it. 100641 (in20, 22 carriers, 11 fired): <=
  admits 13, 11 fired, none outside; < admits 1 and 10 fired outside it.
  900261's one firing went through its in20 alternative (params[3] = 0)
  after ranks 1-2 only, which <= predicts. out40 (100341, 900341) and out50
  (100441, 900441) do not separate the readings. Round 1, investigator H
  (`scripts/H-precondition-semantics/tape_bands.py`); round 2,
  `integrate-step-0028/tape_edges.py`.
- Engine, the series head, census at 200 rounds: 110611 76.57% against
  86.1% (z +1.35; ulc.25 66.6%). 0028's first build (tree 2939272) put most
  of the rest in the engine's carriers rushing before the 50% mark (10.5% of
  rounds against 0 of 36 on tape, P 0.015).

### Side blocking: the doc's window

Patch 0029; README § Side Blocking.

- Tape (round 1, investigator D, reproduced by round 2's
  `integrate-step-0029` scripts): 201271/201272 fire at 5.06 s, the first
  tick accumulatetime>=5 allows, when already side-blocked for 2 s. With
  side geometry at the 3.20 s and 4.26 s frames the doc's window (1.05 m,
  2 lanes) flags 132 of 366 carriers, 87 of whom fired before 5.2 s,
  against 5 of the 234 unflagged; the old 3 m / 1 lane window flags 76 (21
  fired early) and 71 of the 290 it leaves out fired early. 1.75 lanes
  flags 73 (55 fired early), 2.25 lanes 160 (88), 0.9 m 123 (86), 1.35 m
  162 (87): the edges sit near 2 lanes and between 0.9 and 1.35 m.
  201671 (blocked_side_continuetime>=2 alone, phase 1, 37 carriers): a 2 s
  run in the doc's window holds for 27, 25 fired; 10 without, 2 fired.
- Engine, the series head, census at 200 rounds: 201271 86.34% against
  85.7% (z -0.30), 201272 85.59% against 75.6% (z -2.64), 201671 79.32%
  against 73.0% (z -0.95), 201672 83.22% against 82.1% (z -0.25); ulc.25
  89.9, 89.6, 90.4 and 88.9%. 201272's gap is its 16 runaways (round 1:
  tape 56.2% fired, engine 79.8%; the engine's runaways lead at 0.6 of the
  course in 34% of rounds, the tape's in 73%).
- Open: 900511 (side-block precondition) 37.38% against 47.1% (z +2.74):
  the doc window is its better reading on tape (177 of 216 carriers meet it
  and 95 fired, 39 do not and 7 fired), but the engine meets it less often
  and fires less given it. Not determined.

### Overtake targets: the lane list with the vision cone

Patch 0030; README § Overtake Targets.

- Tape (round 1, investigator E and its skeptic, `scripts/E-overtake-target`):
  GameTora's reading (0-20 m ahead, caught within 15 s) predicts 79.6 /
  86.1 / 86.5 / 92.0% firing for 210111 / 202401 / 202402 / 202472; the lane
  list with the vision cone 49.8 / 82.4 / 80.0 / 88.3%; fired 54.1 / 79.7 /
  80.4 / 85.6%. Carriers that held a target and did not fire: 75 against
  36.5 expected from the wit roll under GameTora's reading, 46 against 33.9
  under the cone. overtake_target_time (100871, 200732, 910561, 100481,
  110131; 44 carriers, 24 fired; round 2, `integrate-step-0030/tape_timers.py`):
  GameTora's reading is met more than 0.3 s before 9 of the 24 firings and
  for one unique carrier of 100871 that never fired; the cone, 1 such firing
  (through 100481's is_overtake alternative) and no unfired unique.
  overtake_target_no_order_up_time (910031, 30 carriers, 2 fired) is not
  settled at the recordings' frame spacing.
- Engine, the series head, census at 200 rounds: 210111 57.69% against
  54.1% (z -0.45; ulc.25 80.3%), 202401 82.44% against 79.7%, 202402 77.92%
  against 80.4%, 202472 88.28% against 85.6%, 910031 18.55% against 6.7%
  (z -1.67).

### Near-lane timers: the placement-adjacent uma, one lane inclusive

Patch 0031; README § behind_near_lane_time, whose rule line reads
`abs(LaneGap) < 1 HorseLane`; its note gives the engine's `<=` reading.

- The lane edge. The recordings floor lanes to 1/10000 of the course width,
  so a pair one horse lane apart reads 555 or 556. 62 carriers of the
  near-lane skills meet the condition only if an adjacent uma at that
  offset counts; 53 fired, where the wit roll expects 57.0 and a strict <
  none. The recordings cannot separate <= from < with the game's offset a
  hair inside one lane, but either way a pair at it counts. In the engine
  such pairs sit 0.625 m apart up to float rounding, so the edge takes a
  1e-9 m slack: over 2 rounds of the 117 races, of 1,998,534 pair-ticks of
  position-adjacent umas within 2.5 m, 80,745 sit exactly one lane apart,
  2,003 up to 8.9e-16 m inside it and 1,446 up to 2.2e-16 m past it.
- The 2.5 m edge is not decided: no adjacent pair holds within 1 mm of it
  over two recorded frames, and a crossing is placed no better than a
  frame's change (552 of the 1,280 fired runs entered within one of it,
  median 0.62 m). The engine keeps GameTora's "no more than".
- Adjacency (round 1, investigator C and its skeptic, `scripts/C-lane-time`;
  round 2, `integrate-step-0031/tape-check.mjs`): 201662 (1167 carriers,
  62.2% fired): any uma meets the condition for 911 and predicts 71.8% with
  the wit roll; the adjacent uma for 798, predicting 62.8%. 715 of the 798
  fired (89.6%; mean wit pass 91.9%). 113 carriers meet it only through a
  non-adjacent uma: 2 fired, where the wit roll expects 104.2. 201651: 17
  and 1 against 15.6; 201661: 13 and 1 against 12.0; 200492: 49 and 1
  against 45.2. set1 (900051, 100051): 6 carriers meet the precondition only
  through a non-adjacent uma; 1 fired, 1.1 s before that reading allows,
  where the wit roll expects 5.6.
- Lag on the game's tick (the tick study's `t3_kmin.py`, first firings):
  the tape fires 46 ticks after an adjacent uma enters the window along the
  course (89 of 107) and 48 ticks after the runner's own placement changes
  (46 of 50). The series head (4 rounds a race): 46 (321 of 322) and 47
  (183 of 185); 0036's engine, on 1/15 s: 45 (299 of 299) and 46 (201 of
  202). The engine's timer
  reads 0 on the tick the new placement is first seen. Whether the game's
  second tick is a placement read a tick late or a reset held a tick longer
  is not determined.
- Engine, the series head, census at 200 rounds: 201662 58.30% against
  62.2% (z +2.71), 201651 59.12% against 59.2% (z +0.02), 201661 54.43%
  against 61.4% (z +1.31), 900051 44.32% against 45.2%, 200492 49.12%
  against 56.8% (z +3.45), 200491 51.44% against 50.9%, 202302 61.54%
  against 82.1% (z +2.63); ulc.25 69.5% for 201662, 52.0% for 200492.
- Open: the engine meets the adjacent condition less often than the tape
  (on 0031's first build, 8 rounds a race: 201662 64.1% of carriers
  against 68.4%, 202302 68.3% against 87.2%, 200492 55.5% against 62.1%).
  Not determined.

### The duel window is timed per target

Patches 0033 and 0037; README § Dueling.

- Tape (round 1, investigator I and its skeptic, each with its own
  evaluator: `scripts/I-race-events`, `scripts/verify-I-race-events`): the
  documented rule on the frames interpolated to 1/15 s, over 1,404 runners
  and 533 recorded duels, scores true positives 528, false positives 17,
  false negatives 5, true negatives 854 with a window per target, against
  530 / 34 / 3 / 837 with one window across partners. Recorded minus
  predicted start, p5/50/95: -0.11/0.11/0.19 s against -0.04/0.13/0.83 s;
  12 against 50 duels more than 0.3 s late. A single sticky target (528 ->
  483 true positives), a reset whenever the partner set changes (484), a
  longer window and a smaller box all fit worse.
- The window counts whole ticks: more than 2 s is more than 30 ticks, on
  1/15 s (30 ticks sum to 1.9999999999999998 s in float64, where the old
  seconds sum passed on tick 31 only through rounding) and on 0.0666 s
  (30 ticks 1.998 s, 31 ticks 2.0646 s) alike.
- Engine, the series head, census at 200 rounds, duel share: all runners
  39.78% against tape 38.0% (z -1.39); ulc.25 40.6%.
- Open: late runners duel more in the engine than on tape (round 1 put it
  in the final-straight closing speeds, not in the duel rule).

### Rushed spells begin in sections 2 to 9

Patch 0034; README § Rushed State.

- Tape (round 2, `integrate-step-0034/tape-rushed-entry.py`; round 1,
  investigator I and its skeptic, found the same): 143 spells, one per
  rushed runner. The bracket between the last clear frame and the first
  rushed frame (21.2 m wide, median) holds one section start from 2 to 9
  counted from 1: 15, 17, 19, 14, 19, 21, 18, 20 spells (chi-square 2.29 on
  7 degrees of freedom against uniform), none at 1 or 10. 136 brackets
  contain the start; in the other 7 the last clear frame lies less than a
  tick of her own movement past it (143 / 16 = 8.9 such brackets
  expected). The engine's old window, sections 3 to 10, cannot hold the 15
  spells in section 2, and leaves section 10 empty with probability (7/8)^143
  = 5.1e-9. The first rushed frame is 3.0 / 11.8 / 21.6 / 24.7 m
  (p5/50/95/max) into its section, on sections 50 to 133 m long.
- The 53 Hanshin captures here, a separate sample
  (`integrate-step-0034/tape-rushed-entry-hanshin.py`): 61 spells, 13, 6,
  7, 3, 7, 10, 7, 8 over sections 2 to 9, none at 1 or 10.
- Engine: every spell now begins exactly one section earlier; the same
  runners rush (the same draws in the same order). Rushed share, the series
  head, all runners: 9.76% against tape 10.2%.

### Passing a window checks the next on the same tick

Patch 0036; README § all_corner_random.

- The rule is the README's ("the condition is fulfilled if uma is within
  one of the triggers"); the recordings do not show triggers. The engine
  used to move on to the next window and skip that tick's check, so a window
  starting less than one tick's travel after the last lost its first tick.
- Measured: the census at 200 rounds on the 117 races is the same with and
  without it in every skill rate and style row, on the fold's 0035 against
  0036 (trees 1ad71ac and 1dcf01a; sum of z² 181.10 on both). A counting
  build of the series before that fold (its head, tree 1498499, with this
  patch, native, 200 rounds a race) saw
  1,409,254 window changes, none passing two windows in one tick; 69,647
  landed inside the next window, and on 69,646 of them the skill was still
  cooling down. The one left, a repeat of 200331, now fires on that tick.

### The series on the census and the harness

The series head (patch 0037, tree df1e96b) against ulc.25 (tree acec6f6).

- Census, 200 rounds, the 192 skills with 30 or more carriers: |z| < 3 for
  180 (ulc.25: 173), |z| >= 3 for 2 (200492 +3.45, 201591 +3.45; ulc.25:
  6), |z| >= 2 for 11 (19). Sum of z² over the 179 skills with z defined on
  both: 306.65 -> 186.03; over the head's 182, 187.70. Mean |tape - engine|
  4.28 -> 2.74 points. z is undefined for 10 rows where the engine fires
  every round: 100981 (86 carriers), 200141 (33), 200154 (41), 201522 (306),
  201532 (205), 201542 (51), 202051 (45), 202161 (93), 202331 (53), all tape
  100%, and 210141 (65), tape 98.5%. ulc.25 had these 10 and 100191,
  900171 and 200282, which it never fired.
- Harness on the 117 races, 48 seeds, paired per fixture: free winner hit
  21.47 -> 21.56% (t +0.12), free skill activation error 0.2160 -> 0.2084
  (t -8.64), free HP bias +9.80 -> +10.66 (t +3.42); pinned winner hit
  41.61 -> 42.56% (t +1.56), pinned HP MAE 21.53 -> 21.62 (t +2.10). Against
  0036, the tick alone: pinned HP MAE 21.50 -> 21.62 (t +2.93, 62 fixtures
  up, 39 down); free winner hit t +0.75, pinned winner hit t +1.41.
- Harness on the 53 Hanshin captures here, 8 seeds, pooled, ulc.25 -> the
  series head: pinned finish MAE 0.1998 -> 0.1992 s, winner hit 44.58 ->
  45.28%, Spearman 0.8195 -> 0.8175, trajectory MAE 4.503 -> 4.404 m (t
  -4.32, 40 fixtures down, 13 up), lane MAE 0.983 -> 1.006 m (t +1.41),
  rushed agreement 0.99853 -> 0.99841 (t -4.29, 28 down, 1 up); free finish
  MAE 0.2979 -> 0.3055 s (t +1.48), winner hit 27.59 -> 27.59%, Spearman
  0.578 -> 0.590, skill activation error 0.2529 -> 0.2480 (t -2.48). Split
  at 0036: patches 0026-0036 leave every pinned metric within |t|
  1.21; the tick (0037) moves pinned trajectory -0.099 m (t -4.30), lane
  +0.022 m (t +1.41) and rushed agreement (t -4.29). `baseline.json` is
  re-cut once, in patch 0041, with 0038 and 0039 in (§ Finish time and
  same-tick order).

## Finish time and same-tick order: fixed

Patch 0038; README § Race Time. Tape scripts: `finish_tape.py`,
`finish_tape2.py`, `sametick_defs.py`, `sametick_places.py` and
`display_gaps.py`, not in uma-sim-lab's measurement record yet.

- Tape: `finishTimeRaw` is never a value of the race clock (0 of 1,404
  runners). For 1,400 runners the frames either side of the line are one
  tick apart; for the other 4 (10501-r0108 runner 6, 10601-r0009 runner 3,
  10606-r0100 runner 10, 10606-r0115 runner 5) they are 16 ticks apart and
  the rule cannot be read. For all 1,400, t(n) - (p(n) - D) / (p(n) -
  p(n-1)) x float32(0.0666), computed in float32 from the two frames (t(n)
  the later frame's time, p the distances, D the course distance), is the
  recorded value bit for bit, in every operation order of the fraction
  tried. Anchored on t(n-1) the same line gives it for 912, in float32 and
  in float64; with the frame gap t(n) - t(n-1) in place of the tick, 1,145;
  with the frame's speed in place of the distance change, 326 to 491, off
  by up to 3.3 ms. The t(n) form in float64 is off by up to 7.6e-6 s and
  rounds to the recorded float32 for all 1,400.
- `finishOrder` sorts by `finishTimeRaw` in 117 of 117 races, and no two
  runners in a race share a time. Grouping runners by the tick of the clock
  their crossing falls in: 222 groups of two or more, 90 in runner-index
  order and 132 not. The winner shares her tick in 21 races; in 14 of them
  a runner with a lower index is in her group, so runner-list order would
  have given the race to another runner. One of the top two shares a tick
  in 42 races, and runner-list order would change the top two in 25.
- The engine before (the series head; `runRaceSim` on the fixture params, 4
  rounds a race, 5,616 runner-rounds): every finish time was the clock of
  the crossing tick, on average 0.0331 s after an estimate of the crossing
  from the replay frames, t(k-1) + (D - d(k-1)) / v(k). 910 groups crossed
  on one tick, all placed in runner order and 413 in the estimate's order;
  the winner shared her tick in 81 rounds and was not the first across in
  49.
- After: every finish time lies inside its crossing tick (5,616 of 5,616, 2
  at its end), at a mean 0.503 of the tick and 0.0000 s from the estimate
  on average. 909 of the 910 groups are in the estimate's order; the other
  holds two equal float32 times (11006-r0057 round 1, gates 0 and 7), kept
  in runner order. The winner is the first across in all 81. In the harness
  runs below (48 seeds): 10,875 free and 11,387 pinned groups, 10,874 and
  11,384 in the estimate's order, one tie in each.
- Harness on the 117 races, 48 seeds, paired per fixture against the series
  head. Every metric but the four the finish writes is identical on all 117
  fixtures in both modes. Free: finish bias -0.0074 -> -0.0408 s (t -424,
  all 117 down), finish MAE 0.3422 -> 0.3430 s (t +0.75), winner hit 21.56
  -> 21.60% (t +0.10), Spearman 0.3472 -> 0.3462 (t -0.86). Pinned: finish
  bias +0.0063 -> -0.0269 s (t -367), finish MAE 0.2371 -> 0.2383 s (t
  +0.87), winner hit 42.56 -> 42.86% (t +0.60), Spearman 0.7233 -> 0.7215
  (t -1.92). Per runner-round, pinned: MAE 0.2905 -> 0.2918 s, bias +0.0063
  -> -0.0270 s, largest error 6.175 -> 6.134 s; free: 0.4537 -> 0.4547 s,
  -0.0074 -> -0.0408 s, 7.054 -> 7.024 s. The old rule added half a tick on
  average, which had hidden an engine that finishes about 0.03 s early in
  pinned mode; that pace is a separate question.
- Harness on the 53 Hanshin captures here, 8 seeds: pinned finish MAE
  0.1992 -> 0.1985 s (t -0.36), bias +0.0014 -> -0.0322 s, winner hit
  45.28 -> 43.40% (t -1.03), Spearman 0.8175 -> 0.8136 (t -2.14); free
  finish MAE 0.3055 -> 0.3021 s (t -2.27), bias +0.0322 -> -0.0017 s,
  winner hit 27.59 -> 28.30% (t +0.57). Against a baseline cut at the
  series head, five lines fail on finish MAE (up 0.021 to 0.024 s); patch
  0041 re-cuts `baseline.json` with this patch in.
- The Spearman drop. Inside the engine's same-tick pairs (48 seeds), the
  tape's order agrees with the crossing order in 51.7% pinned (49.9% free)
  and with runner-index order in 53.5% (51.7%). Counted once per distinct
  (fixture, pair), pinned, 52.1% against 52.2%; bootstrapped over fixtures,
  the crossing order's difference is -1.75 points, 95% interval -4.0 to
  +0.5. On the tape a lower gate finishes ahead in 50.1% of all pairs. The
  engine's finish error, 0.29 s a runner-round pinned, is over four ticks,
  so its order inside a tick says nothing about the tape's.
- Census, 50 rounds, all 117 races: identical to the series head's in all
  192 skill rows and all style rows. Finished runners hold the top places in
  the order map whatever their order among themselves, so no condition
  reads the change.
- Open: the rule for equal times (the recordings hold none); the 4 runners
  whose crossing fell in a 16-tick gap; the displayed time. On the
  recordings `finishTime` / `finishTimeRaw` runs from 1.199 to 1.301, never
  the 1.18 the replay writes, but each runner's displayed gap to the winner
  is 1.18 times her raw gap (117 of 117 races, to 1.5e-5 s): a race's
  displayed times are 1.18 x raw plus an offset of the race's own, 1.51 to
  10.12 s. The replay writes no offset. Whether README §
  Race Time's display bounds give it is not determined.

## Gate skills are stamped at the gate: fixed

Patch 0039. Tape script `gate_events.py`, engine probes `gate_probe.mjs`,
`finish_probe.mjs` and `early2.mjs` (runRaceSim on the fixture params), not
in uma-sim-lab's measurement record yet.

- Tape: 4,271 skill events at t = 0 (12 to 55 a race, mean 36.5; 74
  skills; target mask 0 on all). None falls on tick 1 or 2; the first later
  one is on tick 3 (0.1998 s).
- The engine fires gate skills while it prepares the runner
  (`activate_gate_skills`), before the first tick, so they already act from
  the first tick's move, as the game's do. Only the replay was late: it
  wrote a skill's event on the first tick after it saw it used, tick 1 for
  the gate skills. It now writes them at the time they fired, t = 0, still
  on the first tick, once any debuff targets are known. Nothing a race does
  changes.
- 4 rounds a race (468 rounds): 0 skill events at t = 0 and 17,020 on tick 1
  before; 16,893 at t = 0 and 127 on tick 1 after. On 2 rounds a race, the
  tape's t = 0 (runner, skill) pairs are at t = 0 in the engine for 8,211 of
  8,542, and at t = 0 or on tick 1 for 8,274; the rest did not fire there.
- The 127 are skills the engine fires on tick 1 itself, and with them come
  some it fires on ticks 2 and 3, where the tape has none. Measured, not
  changed (4 rounds a race): 201601 (`activate_count_start>=3`, 586
  carriers): tape 498 of its 535 firings at t = 0, none on ticks 1 to 3;
  engine 154 of 2,138 on ticks 1 to 3. 200291 and 200292
  (`running_style_equal_popularity_one==1`, 3 and 23 carriers): tape 16 of
  16 at t = 0; engine 64 of 64 on ticks 1 to 3, none at t = 0. 201331 and
  201332 (a pace chaser's `is_move_lane`, 3 and 73 carriers): tape none
  before 1.66 s, 74 of 74; engine 270 of 274 on ticks 1 to 3. These change
  when the skills act, not how they are stamped.

## Pinned scores by change

Means over the 53 fixtures, pinned mode, 8 seeds.

| Change | Finish MAE | Finish bias | Spearman | Spurt MAE | Trajectory MAE | HP MAE |
|---|---|---|---|---|---|---|
| Harness lands (#92, Hanshin set) | 0.224 s | −0.085 s | 0.795 | 4.10 m | 5.28 m | |
| Runaways imported as Runaway (#93/#94) | 0.220 s | −0.090 s | 0.802 | 2.78 m | 5.23 m | |
| Spurt pinned (#94) | 0.221 s | −0.097 s | 0.809 | 0.96 m | 5.19 m | |
| Frames matched by time (#95) | 0.221 s | −0.097 s | 0.809 | 0.96 m | 4.77 m | 36.8 |
| Rushed pinned (#96) | 0.226 s | −0.094 s | 0.824 | | 4.72 m | 33.3 |
| Downhill pinned (#97) | 0.212 s | −0.068 s | 0.835 | | 4.67 m | 22.3 |
| Target-lane rules and blocking cap (#100) | 0.205 s | +0.007 s | 0.815 | | 4.61 m | 21.5 |
| Lane constants in course widths (#102) | 0.203 s | −0.003 s | 0.809 | | 4.55 m | 21.1 |
| Fork patches 0001-0025 (ulc.25) | 0.200 s | +0.002 s | 0.820 | | 4.50 m | 20.8 |
| Tournament series 0026-0037 | 0.199 s | +0.001 s | 0.817 | | 4.40 m | 20.7 |
| Finish time inside the tick (0038) | 0.199 s | −0.032 s | 0.814 | | 4.40 m | 20.7 |

Speed bias by phase after #97, early / mid / late: +0.031 / +0.043 / +0.055 m/s.
