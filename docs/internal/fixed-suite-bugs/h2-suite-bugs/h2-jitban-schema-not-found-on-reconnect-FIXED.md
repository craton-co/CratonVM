> **RETIRED 2026-07-26 — archived, non-normative.** The `Schema  not found`
> corruption this doc was opened for is fixed and extinct (0 of 218 classes).
> `HIB-LONGTAIL.1` itself is still in place; what remains of it is three
> classes, tracked in
> [`docs/known-issues/h2/h2-jitban-residuals-20260726.md`](../../../known-issues/h2/h2-jitban-residuals-20260726.md).
> The 9-vs-10 discrepancy in the body below is corrected there: **ten** classes
> regressed, and the "9" was the net PASS delta (158 → 149), because
> `TestMvccMultiThreaded2` improved in the same run. Six of the ten are now
> closed. Do not cite this doc as current behaviour.

# H2 JIT-ban lift (`org/h2/`, HIB-LONGTAIL.1): the systemic "Schema  not found" corruption is FIXED; the ban stays for 9 enumerated classes

**Status:** the corruption this doc was opened for is **FIXED** (2026-07-26,
commit `13055f75c`) and no longer occurs anywhere in the suite. The
**disposition is unchanged — keep `HIB-LONGTAIL.1`** — but on completely
different evidence: a full 218-class same-binary A/B now shows 9 classes that
pass with the ban in place and stop passing without it, each with its own
signature. Those 9 are the residual, and they are listed below.

## What the original report said, and what it actually was

The 2026-07-25 partial run (99/218 classes) found `PASS=53 HANG=6 FAIL=40`,
with **16 of the 40 FAILs sharing one signature** — a blank schema name,
`org.h2.jdbc.JdbcSQLSyntaxErrorException: Schema  not found`, thrown while
replaying a persisted `CREATE SEQUENCE` metadata record during `Database`'s
constructor, across 15+ unrelated test classes. The report attributed it to the
"allocate-then-putfield" archetype and recommended keeping the ban.

The archetype guess was wrong; the "any test that reopens a persisted database"
observation was exactly right. Root cause, found by bisecting to a single
method and then to a single String object:

**`java/lang/String`'s inlined JIT intrinsics read `coder` and `hash` four
bytes past their real addresses in a COMPACT-laid-out instance.** Every
`x64.rs` String call site added its own `FIELD_CELL_PAYLOAD32_OFFSET` to the
offset `StringFieldLayout` handed it, but a registered `CompactLayout` offset
IS the payload address (real-JDK `String` packs `value@0 coder@8 hash@12
hashIsZero@16`). So `coder` read `hash`, and `hash` read `hashIsZero`.

`coder` is 0 — LATIN1 — for almost every string, and `hash` is 0 until someone
asks for it, so the misread was invisible **until a String's lazy hash cache
was populated**. At that point `length()` evaluated `value.length >> (hash &
31)`. H2's schema name is the interned `"PUBLIC"`, used as a key in
`Database.schemas`, so its hash was cached: `"PUBLIC".hashCode()` is
`-1924094359`, low five bits `9`, and `6 >> 9 == 0`. `StringUtils
.quoteIdentifierOrLiteral` therefore emitted `""` for it, H2 persisted
`CREATE SEQUENCE ""."SEQ1"` into its own metadata, and every subsequent open of
that database failed with the blank-name `Schema  not found`. The database file
was genuinely corrupted on disk — `org.h2.tools.Recover` on a failed run shows
the empty identifier — which is why it reproduced across so many unrelated
classes and only on reopen.

A second, independent x64 defect surfaced immediately behind it once the first
was fixed: a **reload-elision mirror leaking across a control-flow join**, so
`return s == null ? defaultValue : s` returned the fall-through arm's stale RAX
on the `goto` edge. H2 hit it in `ConnectionInfo.getProperty(key, "rw")`, which
answered `null` for every database open.

Both are general x64-backend bugs, not H2 bugs. Full write-up and regression
tests: commit `13055f75c` (`jit/src/lib.rs`, `jit/src/x64.rs`, five
compact-layout intrinsic tests, a ternary-join codegen test, and
`regression-suite/src/RJitStringLayout.java` — which SIGSEGVs on the pre-fix
binary and passes after).

## The 218-class A/B that replaced the partial run

Same binary for both arms (`dev` + the two fixes), same host, 4-way sharded,
real JDK, `--Xmx 1g`, 300s per-class watchdog. The lifted arm adds only
`CRATONVM_JIT_ALLOW_PACKAGES='org/h2/,org/antlr/v4/runtime/'`.

| | PASS | HANG | FAIL | CRASH |
|---|---:|---:|---:|---:|
| ban in place | **158** (72.5%) | 41 | 19 | 0 |
| ban lifted | **149** (68.3%) | 42 | 25 | 2 |

**`Schema  not found` occurrences with the ban lifted: 0 of 218** (was 16 of
the first 40 FAILs). The cluster is extinct, and the earlier ~40% FAIL rate is
gone with it — the lifted arm is now within 9 classes of the baseline rather
than 40 points behind it.

For reference, the pre-fix committed baseline
(`apps/h2database-suite-runner/RESULTS-20260724.md`, older binary, ban in
place) was PASS 143 / HANG 56 / FAIL 19; both arms above are better than that,
which is the two JIT fixes plus everything else that landed since.

## The residual: 9 classes that regress when the ban is lifted

18 classes change status; 2 improve (`TestMvccMultiThreaded2` FAIL→PASS,
`TestRunscript`/`TestMultiThreaded` shift between failure kinds). These 9 go
from PASS to not-PASS and are what keeps the ban:

| Class | lifted result | signature |
|---|---|---|
| `org.h2.test.unit.TestReopen` | CRASH | `java.sql.SQLException: GeneralError` |
| `org.h2.test.store.TestObjectDataType` | FAIL | `ClassCastException: java.lang.String cannot be cast to java.lang.String` |
| `org.h2.test.store.TestStreamStore` | FAIL | `MVStoreException: NullPointerException` |
| `org.h2.test.unit.TestUpgrade` | FAIL | `NoSuchMethodError: org.h2.util.IntArray.checkCapacity()V` |
| `org.h2.test.db.TestCompatibility` | HANG | timeout, no exception |
| `org.h2.test.mvcc.TestMvccMultiThreaded` | HANG | timeout, no exception |
| `org.h2.test.store.TestFreeSpace` | HANG | timeout, no exception |
| `org.h2.test.synth.TestKillRestart` | HANG | timeout, no exception |
| `org.h2.test.synth.TestNestedJoins` | HANG | timeout, no exception |
| `org.h2.test.unit.TestCache` | HANG | timeout, no exception |

Two of the four hard failures name a mechanism directly and are the obvious
next targets:

* `TestObjectDataType`'s **`java.lang.String cannot be cast to
  java.lang.String`** is a class-identity confusion, not a type error — the
  same shape as the loader-identity/class-id families already documented
  elsewhere in this tree. It should reduce to a small probe.
* `TestUpgrade`'s **`NoSuchMethodError: org.h2.util.IntArray.checkCapacity()V`**
  is a dispatch/resolution failure for a method that plainly exists; the
  `TestUpgrade` family already has a long history of loader-blind native and
  dispatch defects (see the memory index entries for its 11-pass saga), so
  check those first.

The six HANGs need a per-class timeout-free run before they can be classified
at all — with the ban lifted, JIT'd H2 code is throughput-competitive, so a
300s HANG here is more likely a livelock or a lost wakeup than slowness.

## Disposition

**Keep `HIB-LONGTAIL.1` (`org/h2/`).** Not for the reason the ban's own comment
gives (throughput — that framing is stale; the Hibernate longtail it cites was
root-caused to the executor bridge in 2026-07-15) and not for the reason the
first version of this doc gave (systemic metadata corruption — fixed). Keep it
because 9 classes still regress, four of them with concrete, individually
actionable signatures.

The `org/antlr/v4/runtime/` half of the ban is still untested in isolation: the
H2 suite does not exercise it, and it was lifted together with `org/h2/` in both
arms above. It needs a Hibernate-HQL run to say anything about.

## Reproduction

Standalone, ~1 second, no suite runner — this is the reduced form the root
cause was found with. It fails at round 7 on a pre-fix binary and runs
indefinitely after:

```java
// SeqRepro.java — create a sequence, then reopen the database N times.
import java.sql.*;
public class SeqRepro {
    public static void main(String[] args) throws Exception {
        String dir = args[0]; int rounds = Integer.parseInt(args[1]);
        String url = "jdbc:h2:" + dir + ";DB_CLOSE_ON_EXIT=FALSE";
        Class.forName("org.h2.Driver");
        try (Connection c = DriverManager.getConnection(url, "sa", "")) {
            Statement s = c.createStatement();
            s.execute("DROP ALL OBJECTS");
            s.execute("CREATE SEQUENCE SEQ1");
            s.execute("CREATE TABLE T(ID INT PRIMARY KEY, V VARCHAR(255))");
            s.execute("CREATE VIEW V1 AS SELECT * FROM T");
        }
        for (int i = 0; i < rounds; i++) {
            try (Connection c = DriverManager.getConnection(url, "sa", "")) {
                ResultSet rs = c.createStatement().executeQuery("SELECT NEXT VALUE FOR SEQ1");
                rs.next();
                System.out.println("round " + i + " seq=" + rs.getLong(1));
            }
        }
        System.out.println("OK");
    }
}
```

```bash
TMPDIR=/data/tmp CRATONVM_JIT_ALLOW_PACKAGES='org/h2/,org/antlr/v4/runtime/' \
  <cratonvm> --java-home /home/victor/jdk25 --Xmx 1g \
  -c <h2>/target/classes:. SeqRepro /data/tmp/db1 150
```

To see the on-disk corruption a failed run leaves behind:
`java -cp <h2>/target/classes org.h2.tools.Recover -dir <dir> -db db1`, then
grep the generated `.h2.sql` for `CREATE SEQUENCE ""`.

Full suite A/B (`TMPDIR=/data/tmp` is required on the Azure host — root `/` is
out of space and the runner's internal `mktemp` silently produces empty results
otherwise):

```bash
cd apps/h2database-suite-runner
TMPDIR=/data/tmp H2_ROOT=/data/data/h2database/h2 CRATONVM_BIN=<binary> \
  OUTROOT=<out> ./run-h2-suite.sh run --category all --shard 1/4 --tag <tag>
```

## Related

- `docs/internal/jit-ban-sweep-20260725.md` — the sweep this was found under;
  its "H2/ANTLR-runtime ban — RESULT: CONFIRMED STILL NEEDED" section describes
  the pre-fix state and should be read together with this update.
- `docs/known-issues/jit-skip-list-open-bans-20260725.md` — the cross-session
  ban tracker.
- `docs/internal/arch-2026-07-26/layout-constant-hazards.md` §3 — the
  layout-constant inventory that the compact-offset bug was hiding inside, now
  updated with it.
- `apps/h2database-suite-runner/RESULTS-20260724.md` — the older-binary
  baseline the original report compared against.
