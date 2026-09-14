# G79-1 — the census that was said not to exist

**Status:** MEASURED. No code changed. The measurement corrects three published
numbers and one method.
**Provenance:** CratonVM `C:/craton/target-nolto`, `--jdk-only`, JDK 25.0.3+9.
Probe `regression-suite/probes/AwtCategoryCensus.java`; registry dump via
`--dump-native-registry`; the inherited/absent split re-asked of HotSpot by
reflection.

This is step one of the P0 **"Wholesale `Bridge` over-tagging"** row in
[`jdk-only-runtime-services.md`](runtime-services-blocker-inventory.md), which
requires a census before any retag ([`jdk-only-native-review.md`](../../jdk-only-native-review.md) §7).

---

## 0. The claim this record overturns

Two P0 rows say, in effect, that the census the review needs cannot be run yet
— the JMX row states outright that "the census fields that review needs do not
exist yet".

**They exist.** `--dump-native-registry` already emits, per registration:

```
"real_declaring_method": { "loaded": …, "declared": …, "acc_native": …, "has_code": … }
```

The reason they looked absent is narrower and more interesting than a missing
feature: **those fields are only populated for classes the run actually
LOADED**, and no vector in the corpus touches AWT. A dump taken from any
existing vector reports all 187 AWT registrations as `class-not-loaded`, which
reads exactly like "no data" and is in fact "no question was asked".

A 36-line probe that force-loads the classes — `Class.forName(name, false, cl)`,
deliberately WITHOUT initialising, because a census wants the method table and
not the static state — turns the same dump into a complete census. That probe
is checked in.

## 1. The census (`native-awt`, JDK 25, one image)

**187 registrations**, all currently tagged `Bridge` by one line:
`registry.with_category(NativeKind::Bridge, natives::register_all)` in
`native-awt/src/lib.rs:115`.

| verdict | count |
| --- | ---: |
| `ACC_NATIVE` on the real class — **genuine bridge** | **22** |
| real class HAS bytecode — a shim shadowing real code | 103 |
| abstract, no code — a shim on an abstract API | 32 |
| registered on a SUBCLASS; method inherited, not declared here | 22 |
| **truly absent** — names nothing that exists | 7 |
| descriptor mismatch (name present, signature differs) | 1 |

The 22 genuine bridges are the `initIDs()V` family (12), the JPEG codec entries
(7), `Toolkit.initIDs`, `PlatformGraphicsInfo.hasDisplays0`, and
`Disposer.initIDs` — that is, the JNI-backed leaves, which is what a bridge
should be.

## 2. Three published numbers are stale, and one method is wrong

**Count.** The P0 row says 122 registrations; there are **187**.

**Bridges.** It says 10 target `ACC_NATIVE`; **22** do. The row's list (the two
`initIDs`, `hasDisplays0`, seven JPEG) missed the whole
`java.awt.image.*`/`sun.awt.image.*` `initIDs` family.

**"Names absent methods" is not one category, it is three.** This is the method
correction and it matters for every over-tagging row, not just this one. The
audit counts a registration as naming an absent method when the real class does
not DECLARE it. Re-asking HotSpot by reflection splits those 30 into:

* **22 inherited** — `Graphics2D.clearRect`, `drawLine`, `setColor` and so on.
  The method exists; it is declared on `Graphics`. Registering on the subclass
  is a placement question, not an absence.
* **7 truly absent** — and six of them are the INVERSE error, `Graphics2D`-only
  methods (`rotate`, `scale`, `setStroke`, `setTransform`, `setRenderingHint`,
  `getTransform`) registered on the superclass `java.awt.Graphics`, which does
  not declare them. Plus `ComponentSampleModel.initIDs`, which does not exist.
* **1 descriptor mismatch.**

A raw "absent" count therefore overstates genuine absence by roughly 4×. The
published figures for `native-collections` (91 absent) and `native-io` (91
absent) were produced the same way and should be re-derived before anyone
plans work against them.

## 3. What a retag would actually do — it is not bookkeeping

`native-api/src/registry.rs:5268`:

```rust
CompatibilityMode::JdkOnly => !matches!(self, NativeKind::SyntheticStub),
```

A `SyntheticStub` is **not allowed in `JdkOnly`**, and invoking one is
`Reject(SyntheticNativeInvocation)`. So the 165 non-bridge AWT registrations
survive strict mode **only because they are mistagged** — which is precisely
the harm the P0 row describes, now with a number on it.

Retagging them is therefore a SEMANTIC change: AWT calls that presently work
under `--jdk-only` would begin to fail, correctly. That may well be the desired
end state (closure rule 5 — declare AWT headless and out of scope), but it is a
scope decision, not a cleanup, and it must be taken deliberately.

## 4. The safety net is not what the P0 row implies

The row recommends `native-awt` as the FIRST retag subsystem partly because it
has "an existing conformance test". It has exactly one:
`vm/tests/t7_desktop_conformance.rs`, 21 test functions.

**The differential suite has ZERO AWT coverage.** No vector in
`regression-suite/src/` references `java.awt`, `javax.swing` or
`javax.imageio`. All three arms — 100 vectors, 100 more, 62 — are blind to this
subsystem. Whoever performs the retag should know that the arms staying green
proves nothing about it.

## 4a. The same dump, applied to the other three crates

N4 below asks for the other crates to be re-derived. A first pass costs
nothing — the same dump already carries every registration. What it does NOT
carry is a verdict for a class the run never loaded, and this run loaded AWT
plus whatever the boot path touches. **So each crate's numbers below are only
as good as its measured fraction, which is stated.**

| crate | registrations | measured | `ACC_NATIVE` | shims (bytecode + abstract) |
| --- | ---: | ---: | ---: | ---: |
| `native-collections` | 1301 | **94 %** | **0** | 918 |
| `native-awt` | 187 | 100 % | 22 | 135 |
| `native-io` | 965 | 18 % | ≥16 | ≥143 |
| `native-builtins` | 8166 | 39 % | ≥227 | ≥2255 |

**`native-collections` is the one fully-supported finding here, and it is
stark: 1301 registrations, 1300 tagged `Bridge`, and NOT ONE targets an
`ACC_NATIVE` method.** The P0 row said "1,195 of 1,219 … and not one targets an
`ACC_NATIVE` method"; the count has since grown to 1301 and the "not one"
independently reproduces. Every one of those 1300 is a shim wearing a bridge's
tag, and by §3 that is what lets the whole collections surface survive
`--jdk-only`.

**`native-io` and `native-builtins` are floors, not corrections.** At 18 % and
39 % measured they are consistent with the published figures (86 `ACC_NATIVE`
for `native-io`) and must not be read as contradicting them. To finish those
two, extend the probe's class list — it is a list of names and a
`Class.forName` loop — or take the dump from a run that exercises them.

**The "not declared here" column is deliberately absent from the table.** §2
showed it is three different things, and separating them needs the reflection
pass, not the dump. For `native-collections` the raw figure is 302 against a
published 91 — which is not a discrepancy so much as evidence that the two
counts measure different questions.

## 5. NOMINATIONS

**N1 — WITHDRAWN: already done, and I should have measured before nominating.**
The audit's §8.3 stage 2 asks for a `register_as` that does not exist. The
mechanism does exist, under another name — `NativeMethodRegistry::
register_with_kind`, which sets the kind explicitly and records
`kind_stated` so the census can tell *"someone adjudicated this"* from *"this
inherited the default"*. And the dump carries `kind_stated` per registration, so
the question is answerable rather than arguable:

> **All 22 genuine bridges in `native-awt` are ALREADY pinned.** `kind_stated`
> is true for every one; none inherits the ambient tag.

So the preparatory stage for this crate is complete, and the only thing standing
between here and a retag is the semantic decision in §3 — which is the part that
needs the conformance gaps closed first (`G80-1` §4), not more bookkeeping.

**N1a — the INVERSE query, which the forward one could not find.** Asking
which registrations are pinned but are NOT `ACC_NATIVE` returns exactly one:

```
java/awt/image/ComponentSampleModel.initIDs()V   kind_stated=true, method ABSENT
```

Someone adjudicated that as a bridge, and the adjudication is wrong: §2's
reflection pass puts `ComponentSampleModel.initIDs` in the *truly absent*
group — the real class declares no such method and inherits none. A STATED
bridge against a method that does not exist is worse than an inherited one,
because the census now reports it as reviewed.

It is deliberately NOT deleted here. The census is one image (JDK 25), and this
record's own §2 warns against exactly that inference; `initIDs` is the kind of
method that has come and gone across versions, so the check belongs on 17 and 21
before the registration is removed. Recorded rather than acted on.

**N2 — fix the six inverse-placement registrations regardless of the retag.**
`Graphics2D`-only methods registered on `java.awt.Graphics` are wrong under any
tagging policy: they intercept a class that does not declare them. Same for the
22 subclass placements, which should move to the declaring class.

**N3 — write ONE AWT vector before retagging anything.** §4 is the gap. Even a
headless smoke vector (`Toolkit.getDefaultToolkit`, a `BufferedImage` draw, an
`ImageIO` round trip) converts the retag from unverifiable to measurable.

**N4 — re-derive the `native-collections` and `native-io` figures.** §2 shows
the absent-method count is inflated by inheritance and by placement errors. The
probe generalises: it is a list of class names and a `Class.forName` loop.
