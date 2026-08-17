# `org.h2.test.scripts.TestScript` — the SQL-level divergences, censused against a HotSpot oracle

## Status
**OPEN as a group, mostly fixed (updated 2026-08-17).** 16 errors were reported
on `dev` @ `0d8c5f077`; **15** of them still reproduce on `dev` @ `496bc3c2c`
(`functions/numeric/cosh.sql:7` was fixed in between, see below). Of those 15,
**6 are now fixed** (worktree `/data/cvm-h2sql-20260816`, Azure host
`azureuser@20.80.105.49`) under three root causes; **9 remain open** under two:

* [`testscript-concurrenthashmap-iteration-order-20260816.md`](testscript-concurrenthashmap-iteration-order-20260816.md) — 4 errors, **open**
* [`testscript-collation-turkish-and-locale-display-names-20260816.md`](testscript-collation-turkish-and-locale-display-names-20260816.md) — 5 errors, **open**
* [`testscript-foreign-key-existing-data-check-not-run-20260816.md`](testscript-foreign-key-existing-data-check-not-run-20260816.md) — 1 error, **fixed 2026-08-17**

The fixes are written up in
[`../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-bigdecimal-valueof-double-and-string-codepoints-FIXED-20260816.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-bigdecimal-valueof-double-and-string-codepoints-FIXED-20260816.md)
(`BigDecimal.valueOf(double)`, `String.codePoints()` — 5 errors) and
[`../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-fk-array-comparability-skipped-by-rowcount-shortcut-FIXED-20260817.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-fk-array-comparability-skipped-by-rowcount-shortcut-FIXED-20260817.md)
(the H2 `checkExistingData` native's empty-table shortcut — 1 error).

None of this was previously recorded anywhere under `docs/known-issues/`.
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-parsedatetime-german-locale-month-name-FIXED-20260816.md`
names these failures as explicitly out of its own scope; this is the record it
was pointing at. Its own root cause — CratonVM answering locale queries for
English only — turns out to be the *same* root cause as the `SET COLLATION
TURKISH` cluster here, one layer over (display names rather than calendar
field names).

## The oracle

Real HotSpot JDK 25 (`/data/toolchain/jdk-25`), same classpath, same working
directory shape: **0 errors, exit 0, empty stdout**. Every divergence below is
therefore CratonVM-specific, not an H2 bug and not a test bug.

```
java -Xmx1g -cp "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
     org.h2.test.scripts.TestScript      # -> exit 0, no output
```

## The census

`--nojit`, `--Xmx 1g`, real-JDK mode, one process. 15 errors on `dev` @
`496bc3c2c`; the four "script" rows are `TestScript`'s separate stack-trace
records for a statement that threw where the script expected success, so they
are counted as errors by the runner even though they duplicate a `line:` row.

| # | script:line | Symptom | Root cause | State |
|---|---|---|---|---|
| 1–4 | `testScript.sql:6425` | `SCRIPT` emits the `ALTER TABLE ... ADD CONSTRAINT` rows in a different order (`A_TEST`/`B_TEST` and `DATE_UNIQUE`/`DATE_UNIQUE_2` swapped pairwise) | `ConcurrentHashMap` iteration order | open |
| 5 | `datatypes/json.sql:46` | `CAST(1e100::FLOAT AS JSON)` renders 101 literal digits, not `1.0E100` | `BigDecimal.valueOf(double)` | **fixed** |
| 6 | `datatypes/json.sql:49` | same for `::DOUBLE` | `BigDecimal.valueOf(double)` | **fixed** |
| 7 | `datatypes/varchar-ignorecase.sql:147` | `SET COLLATION TURKISH STRENGTH IDENTICAL` → `INVALID_VALUE_2` | `Locale.getDisplayLanguage` returns the code | open |
| 8 | *(script-level)* | the same statement's `Invalid value "TURKISH" for parameter "collation"` stack trace | same | open |
| 9 | `datatypes/varchar-ignorecase.sql:153` | `INSERT INTO TEST VALUES 'I', 'i'` → `DUPLICATE_KEY_1` instead of `update count: 2` | cascade of #7 (collation never took) | open |
| 10 | *(script-level)* | that statement's `Unique index or primary key violation` stack trace | cascade of #7 | open |
| 11 | `datatypes/varchar-ignorecase.sql:156` | `INSERT ... CHAR(0x0130)` → `update count: 1` instead of `DUPLICATE_KEY_1` | cascade of #7 | open |
| 12 | `ddl/alterTableAdd.sql:166` | `ALTER TABLE B ADD FOREIGN KEY(C) REFERENCES A(C)` (`INTEGER ARRAY` → `TIME ARRAY`) is accepted instead of raising `TYPES_ARE_NOT_COMPARABLE_2` | CratonVM's `checkExistingData` native skipped the type check with its empty-table shortcut | **fixed** |
| 13 | `functions/aggregate/percentile.sql:451` | `MEDIAN` over `DOUBLE` gives `1.5` where `1.50` is expected | `BigDecimal.valueOf(double)` | **fixed** |
| 14 | `functions/aggregate/percentile.sql:457` | same | `BigDecimal.valueOf(double)` | **fixed** |
| 15 | `functions/string/btrim.sql:22` | `BTRIM` with a 3-code-point astral trim set removes nothing | `String.codePoints()` | **fixed** |

### The 16th, and the two that the original report did not name

The report was taken on `dev` @ `0d8c5f077` and listed six clusters covering 11
of its 16 errors. Reconciling against a measured run:

* **`functions/numeric/cosh.sql:7`** (`1.5430806348152437` vs
  `1.543080634815244`) **no longer reproduces.** `Math.cosh(1.0)` now returns
  `0x3ff8b07551d9f551` on both VMs. It was fixed between the two commits by
  `755affa6c` *"java.lang.Math must be fdlibm wherever HotSpot has no
  intrinsic"* — HotSpot has no `cosh` intrinsic on x86-64, so its answer is
  FDLIBM's, one ulp above the correctly-rounded `(e + 1/e)/2` that CratonVM used
  to compute. Worth noting because the value CratonVM returned was the *more*
  accurate one; matching HotSpot here means matching FDLIBM, not matching the
  real number.
* The report did not name `varchar-ignorecase.sql:153`/`:156` (rows 9 and 11
  above) or `ddl/alterTableAdd.sql:166` (row 12). 9 and 11 are cascades of the
  collation failure it did name; 12 is an independent divergence.

## Repro

From `apps/h2database/h2` (the checkout is gitignored and shared — it is not
inside the worktree):

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```

`TestScript` writes a scratch database into the *current* working directory, so
run each VM from its own empty directory if you want the two runs to be
independent.

The oracle is the same command with `/data/toolchain/jdk-25/bin/java -Xmx1g -cp
...` in place of the CratonVM binary.

## Next steps

Per-cluster next steps live in the linked records. As a group: nothing here is a
throughput or GC issue, and nothing here is flaky — all 15 reproduce on every
run.

Two of the three root causes fixed so far were a JDK method not meaning what the
JDK spec says it means. The third was different and worth remembering: an H2
method that CratonVM **natively overrides**, whose Rust reimplementation had
dropped a check the Java original performed as a side effect. Instrumenting the
Java source could not see it — see that record's "Why the Java source was a dead
end". When a divergence lands in a framework CratonVM has natives for (H2,
Hibernate, Netty, ...), check `vm/src/runtime/interpreter/native_override.rs` and
`native-builtins/src/apps_*.rs` before assuming the application's own code is
what runs.

Of the two that remain, the `ConcurrentHashMap` cluster is the only one that is
arguably not a spec violation at all (iteration order is unspecified), and it is
the one that will cost the most to close.
