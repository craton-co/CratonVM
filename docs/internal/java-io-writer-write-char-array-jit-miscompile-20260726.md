# `java.io.Writer.write(...)` JIT miscompile — silently drops written characters — CLOSED 2026-07-27

**Status: ✅ CLOSED.** Filed 2026-07-26 while re-testing the
`org/glassfish/jaxb/` JIT ban (`jaxb_mapping_residual_skip_prefix`) during the
JIT-ban-removal sweep that followed the 2026-07-26 JIT rework. It no longer
reproduces on dev, and the probe it blocked now runs clean with the JAXB ban
removed entirely.

Two things in the original writeup turned out to be wrong; both are corrected
below, because the same reasoning is easy to repeat.

## Original symptom (2026-07-26)

Marshalling a small `@XmlRootElement` class through a real
`jakarta.xml.bind` / `org.glassfish.jaxb` 4.0.7 `Marshaller` into a fresh
`StringWriter` per iteration produced, from iteration 26 onward, XML with an
**empty closing tag**:

```
<?xml version="1.0" encoding="UTF-8" standalone="yes"?><widget id="w26">…</>
```

Expected `</widget>`; the six characters of the root element name vanished, as
if the write had been turned into a zero-length write. `--nojit` was clean;
`CRATONVM_JIT_BISECT_SKIP=java/io/Writer.write` was clean;
`CRATONVM_JIT_BISECT_ONLY=java/io/Writer` still failed.

## Status on 2026-07-27: does not reproduce

Re-run on dev at `c042e794f` with the documented reproducer
(`docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java`, real
JAXB 4.0.7 jars, real JDK 25 via `--java-home`):

| Config | Result |
|---|---|
| Default (JAXB ban active), 100 iterations | clean |
| Default (JAXB ban active), 4000 iterations | clean, 0 failures |
| JAXB ban lifted + `CRATONVM_JIT_DENY=java/io/Writer` (the 2026-07-26 config), 4000 iterations | clean, 0 failures |
| JAXB ban **removed from the skip list**, 4000 iterations × 6 | clean, 0 failures |

Closed by general JIT correctness work merged into dev between 2026-07-26 and
2026-07-27 — not by a targeted fix, and not by this session's change. The
`Writer` family was never a JIT ban (it has no skip-list entry), so there is
nothing to lift; this doc is retired to `docs/internal/` per the
`docs/known-issues` convention (that directory holds only *unfixed* bugs).

## Correction 1 — the overload was `write(String)`, not `write(char[])`

The original writeup pinned the defect on `Writer.write(char[] cbuf)` on the
strength of `BISECT_SKIP=java/io/Writer.write` clearing it, and reasoned about
`write(cbuf, 0, cbuf.length)`'s `arraylength`. But `CRATONVM_JIT_BISECT_SKIP`
matches by **method name**, so it covered every `write` overload at once.

`CRATONVM_DBG_DUMP_JIT=LIST` over the same probe on 2026-07-27 shows the JAXB
path compiles exactly one `java/io/Writer` method:

```
[JIT_COMPILED] java/io/Writer.write(Ljava/lang/String;)V
```

`Writer.write(char[])` is never compiled in this workload. The two bodies have
the same shape (`write(x, 0, <length of x>)`), so the symptom description still
fits — but any future search keyed on `arraylength` would have been looking in
the wrong place. **Lesson: `BISECT_SKIP` narrows to a method *name*; confirm the
actual compiled overload with `CRATONVM_DBG_DUMP_JIT=LIST` before writing the
root cause down.**

## Correction 2 — it does not subsume, and is not subsumed by, the LICM bug

While closing this doc, the same probe at 4000 iterations exposed a *different*,
still-live general VM bug — the LICM / speculative pre-header bypass, whose
`org.xml.sax.helpers.AttributesImpl.ensureCapacity` face killed the run with
`OutOfMemoryError … anewarray … length 1677721600` about one run in three. That
one is real, root-caused, and fixed:
`docs/internal/jit-licm-preheader-bypass-20260727.md`. It is **not** the bug
described here: it has no loop in any `Writer` method to hoist out of, and the
`Writer` symptom predates it and reproduced under `BISECT_ONLY=java/io/Writer`
where `AttributesImpl` cannot compile at all.

## Related

- `docs/internal/jit-licm-preheader-bypass-20260727.md` — the real bug this
  probe was hiding at higher iteration counts.
- `docs/internal/jaxb-jit-ban-removed-20260727.md` — the ban this was found
  under; now removed.
- `docs/known-issues/repros/jitban-remaining-20260726/JaxbQNameProbe.java` —
  the reproducer, kept as the regression witness for all three.

---

## Fixing commit (added 2026-07-27 by a second session)

"Does not reproduce on dev" above now has an exact attribution:
**`82b78bca5` — "fix(jit): String field intrinsics read compact primitive
fields 4 bytes high"** (2026-07-26 13:14 UTC).

Bisected with this doc's own reproducer (`JaxbQNameProbe`, real
`jakarta.xml.bind` / `org.glassfish.jaxb` 4.0.7, JDK 25): dev `95e4d9929`
(07-26 12:15Z) fails deterministically at iteration 27 with the documented
shape — `< ="w27"><>type5_27</>…</>` — and dev `82b78bca5` and every later
tree is clean.

This confirms the correction stated above: `Writer.write(char[])` was never
miscompiled. The JIT's `java/lang/String` field intrinsic read the primitive
`coder`/`hash` field four bytes above its real offset on a compact-layout
`String`, so the *name* strings were already empty by the time they reached
the write. That is also why `CRATONVM_JIT_BISECT_ONLY=java/io/Writer` still
reproduced — the intrinsic fires inside `Writer`'s own compiled body — and why
`BISECT_SKIP=java/io/Writer.write` cleared it without the defect being in
`write`.

The same commit is the root cause of the `org/glassfish/jaxb/` JIT ban; see
`jaxb-jit-ban-removed-20260727.md` in this directory.
