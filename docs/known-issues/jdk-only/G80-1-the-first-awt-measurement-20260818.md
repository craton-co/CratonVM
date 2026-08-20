# G80-1 — the first AWT measurement, and why it changes the retag order

**Status:** MEASURED (25 rows, **10 diverged**); **all 10 FIXED**. The probe is
clean at 0 of 25 under `--jdk-only`, with one known limit stated in N1.
**Provenance:** both VMs, headless (`-Djava.awt.headless=true`). Oracle HotSpot
25.0.3+9-LTS; CratonVM `--jdk-only`. Probe
`regression-suite/probes/Sweep17AwtHeadless.java`.

Follows `G79-1`, which measured that no vector in the suite touches AWT. This
is the first differential measurement of the subsystem.

---

## 0. Why this probe exists

`G79-1` N3 asked for one AWT vector, because the P0 over-tagging row recommends
retagging `native-awt` FIRST and all three arms are blind to it. Before a vector
can be written, somebody has to find out what a vector could honestly assert.

The probe deliberately avoids everything a software rasterizer may legitimately
render differently from Java2D — no antialiasing, no font metrics, no curves.
Solid fills read back through `getRGB`, image geometry, type constants and an
`ImageIO` round trip are CONTRACTS, not rendering choices, so a divergence in
one of those is a defect rather than a difference of opinion.

## 1. Result: 10 of 25

Fifteen rows agree, including the whole `Color` family, image geometry and type
constants, clipping, `getClipBounds`, and — pleasingly — a full PNG `ImageIO`
round trip with the correct magic bytes and pixel value.

## 2. The alpha defect — four rows, one cause, and a misleading symptom

```
new BufferedImage(4,4,TYPE_INT_RGB).getRGB(0,0)        ff000000   (agreed)
   ... same image, after createGraphics()+fillRect     0          (HotSpot ff000000)
setRGB(2,3,0x00FF7F10) then getRGB(2,3)                ff7f10     (HotSpot ffff7f10)
```

An OPAQUE image type has no alpha channel to report, so `getRGB` must SET it:
the JDK returns the ColorModel's RGB, and an opaque ColorModel answers `0xFF`
for alpha whatever the backing store holds. Ours returned the raw 24-bit value.

**The symptom pointed at the wrong component.** A pristine image read correctly,
and only pixels the RASTERIZER had touched came back wrong — which reads exactly
like a drawing bug. It is a READ bug: `BufferedImageData::try_new` fills opaque
images with `0xFF000000`, so an untouched pixel happens to carry alpha, and
anything written afterwards does not. Fixed at the read, where the rule belongs.

## 3. The invented exception message

```
new BufferedImage(4,4,TYPE_INT_RGB).getRGB(9,9)
   HotSpot   ArrayIndexOutOfBoundsException: Coordinate out of bounds!
   CratonVM  ArrayIndexOutOfBoundsException: Array index out of range: 45
```

The type and its precedence were right. The message was not — and the code
carried a comment asserting the opposite:

> `// Match the JDK: the index reported is the offending linear pixel index`

`BufferedImage.getRGB` bottoms out in the raster's own bounds check, which
throws with no index at all. **This is the fifth instance this session of a
comment stating a contract confidently next to code that does not implement
it**, and the second where the comment's claim about HotSpot was simply
invented rather than measured. The oracle was one command away.

Fixed for both `getRGB` and `setRGB`.

## 4. What was found, what is fixed, and why the retag order changes

**`BufferedImage` has no raster and no colour model at all.**

```
b.getRaster()        -> null   (NPE at the call site)
b.getSampleModel()   -> NPE, "this.raster is null"
b.getColorModel()    -> null   (NPE at the call site)
```

Our `BufferedImage` is a handle into a side table (`image::image_registry()`);
the real JDK object's own fields are never populated. Every `Raster`,
`SampleModel` and `ColorModel` API is therefore unavailable — which is a large
part of the public image surface, not an edge.

**FIXED — two `Graphics` methods were not registered at all**, and because the
real classes are abstract the call did not fall through to anything:

```
Graphics2D.setBackground(Color)  -> AbstractMethodError: has no Code attribute
Graphics.getColor()              -> AbstractMethodError: has no Code attribute
```

`AbstractMethodError` for a method the platform declares is a hard failure with
no workaround available to a user program. Both are now registered against the
existing graphics handle (plus `getBackground`, which had the same shape and
would have been the next one found). Note where they were hiding: `setColor` sat
directly above the missing `getColor`, and `clearRect` — which consumes the
background — sat next to the missing `setBackground`. **Each gap was adjacent to
its own other half**, which is exactly the configuration that makes a missing
method invisible to a reader scanning for absent functionality.

**This changed the P0 row's recommended order — see §4b, where the per-group
split it implies is now landed.** That row nominates `native-awt`
as the FIRST retagging subsystem, on the grounds that it is small, single-file
and has the clearest evidence. All true. But retagging decides which
implementation answers a call, and the measurement above says a substantial
fraction of this surface currently answers with `null` or `AbstractMethodError`.
Retagging a subsystem that is 40 % non-conformant optimises the classification
of code that does not work yet. `G79-1` §1's 22 genuine bridges are still worth
PINNING (that record's N1, behaviour-neutral); the wholesale retag should wait
behind the conformance gaps here.

## 4a. The retag, actually run — and what it measured

`G79-1` §3 said retagging is a semantic change and should be taken
deliberately. It can also just be TRIED, which is cheaper than arguing about
it: the ambient tag is one line, and because all 22 genuine bridges are already
pinned with `register_with_kind` (`G79-1` N1), flipping it cannot touch them.

```rust
// native-awt/src/lib.rs:115  — EXPERIMENT, reverted
registry.with_category(NativeKind::SyntheticStub, natives::register_all);
```

Under `--jdk-only` a `SyntheticStub` is refused, so the real bytecode answers.
**3 diverging rows became 8**, and the split is the useful part:

| group | effect of the retag |
| --- | --- |
| `java.awt.image.*` | **FIXED, exactly.** `getRaster()`, `getSampleModel()` and `getColorModel()` all answer correctly and match HotSpot byte for byte. The real `BufferedImage.<init>` runs and populates its own fields. |
| `Graphics`/`Graphics2D`/`SunGraphics2D` | **BROKEN.** 7 rows became `UnsatisfiedLinkError: sun/java2d/windows/WindowsFlags.initNativeFlags()Z` and `NoClassDefFoundError: java/awt/GraphicsEnvironment$LocalGE`. |
| `ImageIO` | **BROKEN**, downstream of the same cause. |

**So the shims are not gratuitous.** The Java2D path bottoms out in the
PLATFORM native library — `awt.dll`'s JNI, which CratonVM does not implement —
and that is why the shim exists. Retagging that group converts a working
approximation into a hard failure. Retagging the image group replaces a
side-table fake with the genuine implementation, for free.

This is exactly the per-group split the ambient-category audit §8.3 prescribes,
now with evidence rather than advice behind it. It also answers N1 below: the
question was never "populate the fields or declare them out of scope" — the
real constructor populates them correctly the moment it is allowed to run.

**A consequence of over-tagging the P0 row does not mention.** `CRATONVM_REAL`
is the differential switch built for precisely this comparison — run real
bytecode instead of a fake, and diff. It only bypasses `SyntheticStub`
registrations. With 1300 `native-collections` and 165 `native-awt` shims
mistagged `Bridge`, `CRATONVM_REAL=all` changes NOTHING for either surface
(measured). The over-tagging does not only inflate a count and hide stubs from
the census; it disables the instrument you would use to plan and validate the
retag.

The experiment was reverted. The tree is unchanged.

## 4b. The per-group retag, LANDED

§4a ran the wholesale retag and reverted it. The audit's actual prescription is
a per-group split, and with a vector in place that is now testable rather than
arguable. Bisected by measurement, not by reading:

* ambient tag → `SyntheticStub`;
* **pinned `Bridge`**: `toolkit`, `headless`, `graphics`, `image`;
* **retagged**: `component`, `frame`, `event`, `font`, `swing`, `clipboard`.

**54 of 199 `native-awt` registrations reclassified**, and under `--jdk-only`
those 54 are now correctly DROPPED (145 remain) so real bytecode answers.
Probe 0 of 25; vector 54 checks PASS; arms 101/101, 96/101, 61/62.

**The vector earned its keep on the first attempt.** An intermediate split that
left `toolkit` retagged passed the PROBE 0/25 and failed the VECTOR:
`Toolkit.getDefaultToolkit()` reached real bytecode and went
`PlatformGraphicsInfo.createToolkit` → `WToolkit.<init>`, the Windows toolkit
that needs the platform AWT library. The probe does not call it; the vector's
headful-refusal rows do. Without `G80-1` N3 that regression would have landed
green.

**What this does and does not establish.** The four pinned groups are pinned on
EVIDENCE — `graphics`/`image` from §4a, `toolkit` from the failure above,
`headless` because its font-family shim is the one thing standing between
`getAvailableFontFamilyNames()` and the `NullPointerException` that `G81-1` §2
rejects under rule 5.

The six retagged groups are safe *for what is measured*. The vector exercises
`frame`, `clipboard` and `component` only through their headful-refusal paths —
where real bytecode throws `HeadlessException`, which is the correct answer and
strictly better than a shim. It does NOT exercise Swing widget behaviour or
event dispatch. Those six groups are now answered by real JDK bytecode under
strict mode, which is what the row wants; the claim here is that nothing
measured regressed, not that they are fully covered.

## 4c. Why `native-collections` did NOT follow in the same session

Having landed §4b, the obvious next move is the same treatment for
`native-collections` — a different crate from `native-builtins`, so contract §8
permits it, and unlike AWT it is heavily covered by existing vectors, so the
arms would adjudicate immediately.

The call site forbids it, in terms:

> **DO NOT flip this line to `SyntheticStub` as a bulk edit.** That is the exact
> shape of the 2026-07-14 regression, at ~8x the blast radius, and the 214
> abstract-interface registrations additionally decide dispatch for every USER
> subclass, not just for `java.util` classes. **Retag per subsystem, one PR
> each, with schema-v2 `invocations` and `overwrote` evidence.**

Two things in that are not obvious from outside and would have cost a
regression to learn. The **abstract-interface** registrations do not merely
shadow `java.util`; they intercept any user class implementing the interface,
so their blast radius is unbounded by the JDK. And a **bulk flip has been tried
before**, on 2026-07-14, at an eighth of this size.

So the boundary here is not §8 and not my judgement — it is this crate's own
stated discipline: **one subsystem, one PR, with invocations and overwrote
evidence.** This session's PR for that row is `native-awt` (§4b). The next
subsystem is the next PR, by the rule the file states.

The method §4b demonstrates transfers directly, and the evidence the comment
demands is already available: `--dump-native-registry` carries `invocations`
and `overwrote` per registration, and `G79-1`'s force-loading probe populates
the `real_declaring_method` census for whichever classes a subsystem targets.

## 5. NOMINATIONS

**N1 — DONE via option (A).** `BufferedImage.<init>` now builds the genuine
`DataBufferInt` / `WritableRaster` / `DirectColorModel` trio — every one of them
real JDK bytecode, each verified identical to HotSpot BEFORE any Rust was
written — and stamps them onto the object's own fields. `getRaster()` is
registered solely to synchronise the rasterizer's pixels into the data buffer
before it leaves for Java. Probe 3 diverging rows -> 0; the vector asserts 40
checks including a band read THROUGH the raster after drawing, which tests the
synchronisation rather than the object's mere existence.

**The limit, stated rather than left to be discovered:** the raster is a
SNAPSHOT refreshed when handed out, not a live view. Pixels written through it
do not flow back. That is option (B) below, and it still costs what it cost.

*Superseded framing, kept because the correction is the point:* the original
framing here was wrong. Neither option was needed: the real constructor
populates `raster`/`sampleModel`/`colorModel` correctly as soon as the shim
stops shadowing it, measured exact against HotSpot.

The work is a per-group split rather than one line, and it has a REAL hazard
§4a did not have to face, because the retag broke `createGraphics` before it
could: our `getRGB`/`setRGB`/`createGraphics` read a side table keyed off the
image, populated by our own `<init>`. Let the real `<init>` run and no
side-table entry exists.

**I called that "bounded: one backing store instead of two". Then I measured
it, and it is not.** `native-awt/src/renderer.rs`, `graphics2d.rs` and
`image.rs` are 4770 lines containing **zero** references to `NativeContext` —
they are deliberately VM-independent Rust, which is why they are testable
without a VM at all. A rasterizer cannot write into a Java `int[]` without VM
access. So this is an ownership inversion, not a plumbing change. Three
honest options, in ascending cost:

**(A) Sync at interception points.** Register `getRaster()` / `getData()` as
natives that copy the Rust buffer into the real `DataBufferInt` before
returning the field. Bounded — the real `<init>` already builds correct raster
and colour-model objects (§4a proved that). The gap it leaves must be
documented, not hidden: writes made THROUGH the returned raster do not flow
back, so `raster.getDataBuffer()` is a snapshot, not a view.

**(B) Invert ownership.** The Java `int[]` becomes the truth and the renderer
operates on it through `ctx`. Correct with no snapshot semantics, and it costs
`NativeContext` threaded through all 4770 lines plus the loss of their
VM-independence and their standalone tests.

**(C) Declare the raster APIs unsupported under `--jdk-only`** (closure rule
5) and make them throw rather than return `null` — which is at least a
truthful failure, and is strictly better than today whatever else is chosen.

**(C) should land regardless**, because returning `null` from a method that
cannot return `null` is the one option nobody would defend. (A) is the natural
next increment. (B) is a project.

**N2 — DONE.** `Graphics.getColor`, `Graphics2D.setBackground` and
`getBackground` are registered. 10 diverging rows -> 3.

**N3 — DONE.** `regression-suite/src/RJdkAwtHeadless.java`, 32 checks, scheduled
in the JDK-only corpus and listed in `jdk-only-coverage.txt` against the P0
over-tagging row. It asserts contracts only, and deliberately does NOT assert
the three §4 rows — writing the present behaviour in would freeze a defect into
the suite. **The arms are no longer blind to AWT**: new baseline `--jdk-only`
101/101, `SUITE=all` 96/101 (the same five known failures), `SUITE=core` 61/62
unchanged.

**N4 — the probe avoided rasterization on purpose, so nothing here says our
rasterizer draws correctly.** Lines, curves, antialiasing, strokes, transforms
and text are all unmeasured. A second probe that compares whole rendered
buffers would need a tolerance policy, which is a design question this one
deliberately sidestepped by asking only about contracts.
