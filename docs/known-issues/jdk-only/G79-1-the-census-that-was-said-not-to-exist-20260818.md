# G79-1 — the census that was said not to exist

**Status:** MEASURED. No code changed. The measurement corrects three published
numbers and one method.
**Provenance:** CratonVM `C:/craton/target-nolto`, `--jdk-only`, JDK 25.0.3+9.
Probe `regression-suite/probes/AwtCategoryCensus.java`; registry dump via
`--dump-native-registry`; the inherited/absent split re-asked of HotSpot by
reflection.

This is step one of the P0 **"Wholesale `Bridge` over-tagging"** row in
[`jdk-only-runtime-services.md`](../../jdk-only-runtime-services.md), which
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

## 5. NOMINATIONS

**N1 — pin the 22 genuine bridges with `register_as` BEFORE narrowing the
ambient tag.** This is behaviour-neutral today (they are already `Bridge`) and
is what makes the eventual retag safe: once the real bridges are pinned
explicitly, changing the ambient default cannot silently flip them. The audit's
§8.3 stage 2, now with the exact list of 22 to pin (§1).

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
