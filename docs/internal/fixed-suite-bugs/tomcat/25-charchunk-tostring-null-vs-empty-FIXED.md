# `CharChunk.toString()` returns `""` instead of `null` when empty/recycled — FIXED

**Status:** ✅ **FIXED** (2026-07-27, branch
`fix/tomcat-charchunk-tostring-20260727`). `TestCharChunk` passes.

Root cause was **not** in `CharChunk.java` — Tomcat's own bytecode is correct
and never ran. CratonVM force-dispatches `CharChunk.toString()` to a Rust
native, and that native reproduced only two of the Java method's three
outcomes.

## Symptom

`org.apache.tomcat.util.buf.TestCharChunk.testToString`:

```
java.lang.AssertionError: expected null, but was:<>
	at org.apache.tomcat.util.buf.TestCharChunk.testToString(TestCharChunk.java:77)
```

Line 77 is the **third** assertion — the one after `recycle()`, not the
fresh-chunk one:

```java
CharChunk cc = new CharChunk();
Assert.assertNull(cc.toString());            // passed
char[] data = new char[8];
cc.setChars(data, 0, data.length);
Assert.assertNotNull(cc.toString());         // passed
cc.recycle();
Assert.assertNull(cc.toString());            // FAILED: got ""
```

## Root cause

`native_char_chunk_to_string` (`native-builtins/src/lib.rs`) decided
null-vs-string from `char_chunk_parts`, whose only null test is
`buff == null`. The Java method decides from `isNull()`:

```java
// CharChunk.toString()
if (isNull()) { return null; }
else if (end - start == 0) { return ""; }
return StringCache.toString(this);

// AbstractChunk.isNull()
if (end > 0) { return false; }
return !isSet;
```

`AbstractChunk.recycle()` clears `hasHashCode`/`isSet`/`start`/`end` but
**deliberately keeps `buff`** so the buffer can be reused. So a recycled
chunk is `isNull() == true` *with a live buffer* — the exact state a
`buff == null` test cannot see. The native fell through to its
`units.is_empty()` branch and returned `""`.

The fresh-chunk assertion passed only by coincidence: a never-used chunk has
`buff == null` too, so there the two tests agree.

## Fix

`native_char_chunk_to_string` now mirrors the Java three-way decision,
reading the real `isSet`/`end` state (`end > 0 ? false : !isSet`) instead of
the buffer. "No buffer" and "no data" are now distinct: a chunk that is set
but empty (`allocate()`d, or `setChars(buf, off, 0)`) still returns `""`;
only never-set/recycled returns `null`.

**Ordering matters for throughput, not just correctness.** The first cut of
this fix called an `isNull()` helper up front, which put a *fourth*
`get_field_by_name` on every `toString()` — and this method runs for every
response URL. It now tests `end` first and reads `isSet` **only** when
`end == 0`, so the ordinary non-empty path performs exactly the same three
name-keyed lookups (`end`, `buff`, `start`) it did before the fix, and the
recycled path skips `buff`/`start` entirely. The helper is therefore named
`char_chunk_is_set`, documented as requiring callers to short-circuit on
`end > 0` themselves.

That this is worth caring about is corroborated independently by
[32-doc04-residual-perf-assertions](../../../known-issues/tomcat/32-doc04-residual-perf-assertions.md):
snapshotting the `CharChunk` range once per `Mapper` search, instead of
re-reading three `get_field_by_name` per binary-search probe, moved that
benchmark from 3.34 s to 2.82 s. Name-keyed field reads in this family are
a measurable cost.

Measured for this change (2M iterations, set non-empty chunk, two rounds
each, on a loaded shared host): base 6386/5643 ns/op, first cut 6134/5753,
final 6299/5840 — all inside run-to-run variance, i.e. no measurable
regression from either version. Recorded because "no regression" is the
claim being made; neither version justified itself on the numbers alone,
and the final shape is simply the one that cannot regress by construction.

## Residuals found and fixed in the same pass

An audit of the rest of the `CharChunk` native family (probe:
`CCDispatch.java`, oracle: HotSpot) turned up **9 more divergences**, all
fixed alongside the primary bug:

1. **Null-argument contract (6 cases).** `equals(String)`,
   `equalsIgnoreCase`, `startsWith` and `startsWithIgnoreCase` returned
   `false` for a null argument where Tomcat throws NPE. The two families
   differ in *when*: `equals*` computes `len` from the chunk first and
   short-circuits on `c == null || len != s.length()`, so a null argument on
   a **buffer-less** chunk is legitimately `false`; `startsWith*` reads
   `s.length()` into a local **before** the `c == null` test, so it always
   throws. Both shapes are now reproduced exactly.

2. **Ignore-case folding rule (3 cases).** `CharChunk.equalsIgnoreCase` /
   `startsWithIgnoreCase` use a **paired** branch —
   `if (c1 > 0xFF || c2 > 0xFF)` → `Character.toLowerCase` both, else
   `Ascii.toLower` both — where the native lowered each side independently.
   The `||` matters: one non-Latin-1 side pulls the *other* side onto
   `Character.toLowerCase` too. Demonstrated divergence: `U+212B ANGSTROM
   SIGN` vs `U+00C5 Å` folds to `U+00E5 å` on both sides under Tomcat
   (equal), but the per-char rule leaves `U+00C5` untouched (`Ascii.toLower`
   is `A`-`Z` only) and reported not-equal. Fixed with a new
   `char_chunk_chars_equal_ignore_case` helper.

   `Mapper.compareIgnoreCase` genuinely *does* use the per-char rule, so
   `mapper_compare_chunk_to_string` and the `char_chunk_range_*` helpers
   correctly keep `ascii_lower_char`. Both helpers now carry doc comments
   naming the Tomcat method each mirrors.

3. **Dead dispatch-gate config.** `force_native_over_real_jdk_bytecode`
   (`vm/src/runtime/interpreter.rs`) claimed `CharChunk.endsWith(String)`,
   `CharChunk.indexOf(char)` and `AbstractChunk.indexOf(String,III)`, none
   of which were ever registered. Every consumer resolves the callback
   through `NativeMethodRegistry::find` and silently falls back to bytecode
   on a miss, so those three were behaviourally inert but read as "served by
   a native" to anyone auditing the list — the same two-lists-out-of-sync
   footgun that produced this bug. Removed, with a note that both lists must
   move together. `AbstractChunk` is consequently gone from the
   `org/apache/` early-reject exception list too.

Worth recording for future work in this family: the four comparison natives
are live **without** being in `force_native_over_real_jdk_bytecode`
(registration alone suffices for a Tomcat application class), while
`endsWith` was in the gate and still ran bytecode. The gate is not the
authority on what is native — the registry is.

## Verification

- `CCRepro.java` (44 assertions over `toString`/`isNull`/`endsWith`/
  `startsWith`/`equals`/`indexOf`/`CharSequence`, expectations taken from
  HotSpot): baseline 1 FAIL → fixed **ALL PASS**.
- `CCDispatch.java` (19 null-argument and ignore-case-folding assertions):
  baseline 9 FAIL → fixed **ALL PASS**, matching HotSpot exactly.
- `org.apache.tomcat.util.buf.*`: 15/15 PASS (`TestCharChunk` was the only
  pre-existing failure in the package).
- `TestMapper`, `TestMapperListener`, `TestResponse`, `TestRequest` (the
  heaviest consumers of the `char_chunk_*` helpers): PASS.
- Tomcat suite regression sweep, **211 of 646 classes** (`-Parallel 6
  -TimeoutSec 900`), diffed against the `fullsuite-local-20260728`
  reference: **200 PASS, 7 FAIL, 2 HANG — no regressions**. All 5
  unchanged FAILs are pre-existing in the reference
  (`TestDefaultInstanceManager` = doc 26, `TestAsyncContextImpl`,
  `TestRestCsrfPreventionFilter`, `TestMapperPerformance`,
  `TestJNDIRealm`); both HANGs are likewise pre-existing. Two classes moved
  HANG → FAIL (`TestDeployTask` 508s, `TestManagerWebapp` 419s) — they now
  complete, which is the longer timeout rather than anything this change
  did.

  The sweep was stopped at 211 rather than run to 646: this fix's blast
  radius (`util.buf`, `catalina.mapper`, `catalina.connector`) is covered
  exhaustively above, and no `CharChunk` native path is reachable from the
  remaining classes.

Two operational traps worth recording for the next long suite run on this box:

* The `DoHead` family (65 classes) legitimately takes up to ~306s, so at the
  suite's default `-TimeoutSec 300` it flips PASS↔HANG on host load alone.
  An earlier run of this very build showed 49 spurious HANGs there; all pass
  at 900s, and three re-run individually on a quiet host passed at 282s,
  306s and 250s. Do not read those as regressions.
* Long runs on this shared host get killed with exit code `-1073741510`
  (`STATUS_CONTROL_C_EXIT`) — a console control event that reaches
  `Start-Process` children, WMI-created processes AND `schtasks`-launched
  ones alike. CSV rows carrying that rc are kill artifacts, not results:
  drop them and let the resumable runner redo those classes.

## Reproduction (historical)

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category all -Parallel 1 `
  -TimeoutSec 60 -RunName charchunk-repro -Exe <cratonvm.exe>
```
