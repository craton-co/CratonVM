# G80-1 — the first AWT measurement, and why it changes the retag order

**Status:** MEASURED (25 rows, **10 diverged**); **7 FIXED, 3 left** — and the
three that remain are one design decision, not three gaps.
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

**This changes the P0 row's recommended order.** That row nominates `native-awt`
as the FIRST retagging subsystem, on the grounds that it is small, single-file
and has the clearest evidence. All true. But retagging decides which
implementation answers a call, and the measurement above says a substantial
fraction of this surface currently answers with `null` or `AbstractMethodError`.
Retagging a subsystem that is 40 % non-conformant optimises the classification
of code that does not work yet. `G79-1` §1's 22 genuine bridges are still worth
PINNING (that record's N1, behaviour-neutral); the wholesale retag should wait
behind the conformance gaps here.

## 5. NOMINATIONS

**N1 — populate `BufferedImage.raster` / `colorModel`, or declare them out of
scope.** §4. These are not obscure APIs; `getRaster()` is how most image code
reaches pixels. Either the side table becomes a real `Raster` the JDK classes
can see, or the P0 closure rule 5 route is taken and the image surface is
declared unsupported under `--jdk-only` — but the present state, returning
`null` from a method that cannot return `null`, is neither.

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
