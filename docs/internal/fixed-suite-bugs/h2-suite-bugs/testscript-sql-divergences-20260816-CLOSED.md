# ✅ CLOSED — `org.h2.test.scripts.TestScript`: the SQL-level divergence census, all 15 fixed

## Status
**CLOSED 2026-08-17.** `org.h2.test.scripts.TestScript` reports **0 errors**
under CratonVM, the same as HotSpot JDK 25 on the identical classpath.

Re-verified on `dev` @ `559daa8b8` — not on the branch that fixed the last two
clusters — with three consecutive runs, each from a cleared data directory and
serialized so no run holds the database file another needs:

| arm | rc | errors | elapsed |
|---|---|---|---|
| CratonVM run 1 | 0 | **0** | 1192s |
| CratonVM run 2 | 0 | **0** | 1145s |
| CratonVM run 3 | 0 | **0** | 1147s |
| HotSpot JDK 25 | 0 | **0** | 10s — stdout empty |

16 errors were reported on `dev` @ `0d8c5f077`; 15 still reproduced on `dev` @
`496bc3c2c` (the 16th, `functions/numeric/cosh.sql:7`, was fixed in between —
see below). All 15 are now closed under five root causes.

## The census, and where each row went

| # | script:line | Symptom | Root cause | Closed by |
|---|---|---|---|---|
| 1–4 | `testScript.sql:6425` | `SCRIPT` emits the `ALTER TABLE … ADD CONSTRAINT` rows in a different order (`A_TEST`/`B_TEST` and `DATE_UNIQUE`/`DATE_UNIQUE_2` swapped pairwise) | the CHM iteration reorder masked with the sum of OUR segment capacities, not the JDK's flat table size | `testscript-concurrenthashmap-iteration-order-20260816-FIXED` |
| 5–6 | `datatypes/json.sql:46`, `:49` | `CAST(1e100::FLOAT AS JSON)` renders 101 literal digits, not `1.0E100` | `BigDecimal.valueOf(double)` | `bug-h2-testscript-bigdecimal-valueof-double-and-string-codepoints-FIXED-20260816` |
| 7–11 | `datatypes/varchar-ignorecase.sql:147`, `:153`, `:156` + 2 script-level stack traces | `SET COLLATION TURKISH STRENGTH IDENTICAL` → `INVALID_VALUE_2`, and the two inserts that depend on it | `Locale.getDisplayLanguage` returned the subtag, so H2's name→locale lookup never matched; and the collator had no tailoring even once it did | `testscript-collation-turkish-and-locale-display-names-20260816-FIXED` |
| 12 | `ddl/alterTableAdd.sql:166` | a foreign key between uncomparable ARRAY types is accepted | CratonVM's `checkExistingData` native skipped the type check with its empty-table shortcut | `bug-h2-testscript-fk-array-comparability-skipped-by-rowcount-shortcut-FIXED-20260817` |
| 13–14 | `functions/aggregate/percentile.sql:451`, `:457` | `MEDIAN` over `DOUBLE` gives `1.5` where `1.50` is expected | `BigDecimal.valueOf(double)` | `bug-h2-testscript-bigdecimal-valueof-double-and-string-codepoints-FIXED-20260816` |
| 15 | `functions/string/btrim.sql:22` | `BTRIM` with a 3-code-point astral trim set removes nothing | `String.codePoints()` | `bug-h2-testscript-bigdecimal-valueof-double-and-string-codepoints-FIXED-20260816` |

Rows 8 and 10 are `TestScript`'s separate stack-trace records for a statement
that threw where the script expected success. The runner counts them as errors
even though they duplicate a `line:` row, which is why five errors came from
three script lines.

### The 16th

`functions/numeric/cosh.sql:7` (`1.5430806348152437` vs `1.543080634815244`)
stopped reproducing between the two commits, fixed by `755affa6c` *"java.lang.Math
must be fdlibm wherever HotSpot has no intrinsic"*. Re-confirmed on `dev` @
`559daa8b8`: `Math.cosh(1.0)` and `StrictMath.cosh(1.0)` both return
`0x3ff8b07551d9f551` on both VMs.

Worth keeping for its direction: the value CratonVM used to return was the
**more accurate** one. HotSpot has no `cosh` intrinsic on x86-64, so its answer
is FDLIBM's, one ulp above the correctly-rounded `(e + 1/e)/2`. Matching HotSpot
here means matching FDLIBM, not matching the real number.

## The oracle, and how to re-run it

Real HotSpot JDK 25 on the same classpath and working-directory shape: **0
errors, exit 0, empty stdout** — measured again at closure, `stdout_lines=0`.
Every divergence in the census was therefore CratonVM-specific, not an H2 bug
and not a test bug.

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2          # gitignored, shared, NOT in the worktree
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"

# oracle
java -Xmx1g -cp "$CP" org.h2.test.scripts.TestScript

# subject
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "$CP" org.h2.test.scripts.TestScript
```

**`TestScript` writes a scratch database into the current working directory.**
Two arms started from the same directory collide on
`data/test/script.mv.db`, and the second one dies on the file lock. Clear
`data/test` and wait for the previous process to exit between arms.

## The measurement trap this census set, twice

A run that dies on that file lock exits in seconds having printed almost
nothing, and `grep -c '^ERROR'` over a log that stops early returns a *small*
number. During this closure that produced a reading of **"99s, 4 errors"** which
looked like a fast, mostly-passing baseline. The real baseline at that moment
was **1083s, 9 errors**. The same shape appeared a second time before the driver
was changed to wait for the previous `TestScript` to exit and clear the data
directory first.

Any future reading of this suite that is dramatically *faster* than ~1100s under
`--nojit` should be treated as an aborted run until the log is checked for
`file is locked`.

## What the census was worth

Five root causes, and only two of them were the kind the symptom suggested.

* **Three were a JDK method not meaning what its name implies.**
  `BigDecimal.valueOf(double)`, `String.codePoints()`, and
  `Locale.getDisplayLanguage` — the last of which was returning a value chosen
  deliberately, with a comment explaining that a code was "good enough for any
  caller that just wants a non-null human-readable string". It was not good
  enough for the caller that used it as a lookup key.

* **One was an H2 method CratonVM natively overrides**, whose Rust
  reimplementation had dropped a check the Java original performed as a *side
  effect* of preparing a query. Instrumenting the Java source could not see it,
  because the Java source was not what ran. When a divergence lands in a
  framework CratonVM has natives for (H2, Hibernate, Netty, …), check
  `vm/src/runtime/interpreter/native_override.rs` and
  `native-builtins/src/apps_*.rs` before assuming the application's own code is
  what executes.

* **One was a number that had been correct when it was written.** The CHM
  reorder masked with the summed capacity of our segments, which equals the
  JDK's flat table size at construction and diverges from it at the first
  segment resize. Nothing re-derived it once the two could differ.

The census's own closing prediction — that the `ConcurrentHashMap` cluster
"is the one that will cost the most to close" — was wrong, and instructively so.
It cost one expression plus the constructor bookkeeping to record the JDK's
table size, because the reorder it needed already existed for an unrelated Spring
fix. The estimate assumed the work was "reimplement the JDK's table layout";
the actual work was to stop using the wrong number in machinery that was already
there.

## One residual, named rather than left implicit

The CHM order fix is exact for maps that have never resized and
bucket-correct-only for maps that have. CHM's `transfer` reuses the `lastRun`
suffix of a bin and prepends everything before it, so a resize reverses part of
each bin, and reproducing that needs a per-node insertion sequence the
implementation does not carry. H2's schema map in this suite holds eight
constraints and never resizes, which is why the suite is at zero — a schema with
twelve or more constraints in a `SCRIPT` golden-file test could still diverge.
`probes/ChmOrderCensus` is the ratchet for the part that is fixed; see the CHM
record for the numbers.
