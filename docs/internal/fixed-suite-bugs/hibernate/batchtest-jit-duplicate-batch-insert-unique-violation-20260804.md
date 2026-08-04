# `batch.BatchTest` — JIT-only unique-constraint violation on `DataPoint(xval,yval)`

**Status: FIXED (2026-08-04).** The defect was not in the JDBC batch path at
all, and both mechanisms this doc originally proposed are **refuted by direct
evidence** (see "What the parameter trace actually showed").

CratonVM's post-`<clinit>` fixup injected the nine
`jdk.internal.misc.Unsafe.ARRAY_*_BASE_OFFSET` statics with the wrong width.
That is invisible to the interpreter and reads as garbage from JIT-compiled
code. The visible consequence was that `java.util.Arrays.equals` / `mismatch` /
`compare` answered **wrongly for `int[]`, `long[]`, `short[]` and `double[]`** —
`Arrays.equals(long[],long[])` returned `true` for arrays that differ, silently.
H2's index machinery then believed two distinct keys were equal and a unique
index reported a collision that did not exist.

* Fix: `vm/src/vm/vm_util.rs` — `coerce_static_to_descriptor`.
* Guard: `regression-suite/src/RArraysMismatch.java` (in `CORE_CLASSES`).
* Reproducers: `probes/H2BatchBindProbe.java` (Hibernate-free, ~20 s),
  `probes/MismatchShapeProbe.java`, `probes/MismatchShapeReplicaProbe.java`,
  `probes/ArraysMismatchProbe.java`, `probes/BatchXyProbe.java`.

Filed the same day as OPEN with the symptom below; root-caused and fixed in the
follow-up session. The original symptom write-up is kept because it is accurate
and is what a future reader will search for.

---

## Original symptom

`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/shard-2/raw.log`
(dev tip `a43a74ded`), real-JDK, JIT on, default flags:

```
@@RESULT org.hibernate.orm.test.batch.BatchTest found=4 started=4 ok=0 failed=4 aborted=0 skipped=0 ms=92671
```

All 4 `@Test` methods failed — not with the `TimeoutException` this class was
historically filed under, but with

```
org.hibernate.exception.ConstraintViolationException: could not execute batch
  [Unique index or primary key violation: "PUBLIC.XY INDEX PUBLIC.XY_INDEX_9
   ON PUBLIC.DATAPOINT(XVAL NULLS FIRST, YVAL NULLS FIRST)
   VALUES ( /* key:10 */ 0.90000000000000002220, 0.62160996827066440000)";
   SQL statement: update DataPoint set description=?,xval=?,yval=? where id=?]
  at org.hibernate.engine.jdbc.batch.internal.SingleStatementBatchImpl.performExecution
  at org.hibernate.orm.test.batch.BatchTest.lambda$doBatchInsertUpdate$1(BatchTest.java:93)
```

100% reproducible under JIT, 0% under `--nojit` for the three `N=50` methods.

`doBatchInsertUpdate` inserts `nEntities` rows with `x = i*0.1`, `y = cos(x)`
(so `x` is unique per row by construction), then scrolls them in `x` order
setting `description`, flushing every `nBeforeFlush` rows. Hibernate's
unconditional dirty check re-issues `xval`/`yval` with the row's *unchanged*
values, so the unique index can only fire if two rows genuinely carry the same
pair.

Because the exception aborts the transaction before the delete phase, the first
failing method leaves its 50 rows behind in the class-scoped
`jdbc:h2:mem:db1` database, and every later method collides with those
leftovers — which is why all 4 failed while only the first is evidence.

## Root cause

`Unsafe.ARRAY_BYTE_BASE_OFFSET` and its eight siblings are declared **`long`**
(`descriptor: J`) in JDK 25. The similarly-named `ARRAY_*_INDEX_SCALE` fields
really are `int`; only the descriptor distinguishes them.

`post_clinit_fixup` (`vm/src/vm/vm_util.rs`) repopulates all eighteen because
`Unsafe.<clinit>` computes them by calling natives that are not yet registered
during real-JDK bootstrap, which leaves them latched at 0. It wrote every one
as `Value::Int(16)`.

That leaves the slot holding a well-formed `Value` of the **wrong width**:

* the **interpreter** widens an `Int` where a long is wanted, so `getstatic`
  returned 16 and everything worked — for as long as this fixup has existed;
* **JIT-compiled code** lowers `getstatic …:J` to a 64-bit load of the slot,
  and read `0x7ff700000000` (a Windows image base) instead of 16.

Nothing throws and nothing logs; the wrong value just propagates.

## How that becomes a unique-index violation

`Arrays.mismatch`/`equals`/`compare` over primitive arrays funnel through
`jdk.internal.util.ArraysSupport.mismatch`:

```java
public static int mismatch(int[] a, int[] b, int length) {
    int i = 0;
    if (length > 1) {
        if (a[0] != b[0]) return 0;
        i = vectorizedMismatch(a, Unsafe.ARRAY_INT_BASE_OFFSET, b,
                               Unsafe.ARRAY_INT_BASE_OFFSET, length,
                               LOG2_ARRAY_INT_INDEX_SCALE);
        if (i >= 0) return i;
        i = length - ~i;            // ~i = number of UNCHECKED tail elements
    }
    for (; i < length; i++) if (a[i] != b[i]) return i;
    return -1;
}
```

Compiled, it handed CratonVM's `vectorizedMismatch` native an offset of
`0x7ff700000000`. The native converts the offset to an element index and
range-checks it against the array length; that check failed, so it returned
`-1`. The caller decodes `-1` as `~0`, i.e. "zero elements left to check", sets
`i = length`, and **skips the scalar tail loop entirely** — so `mismatch`
returns `-1`, meaning "identical".

Measured shape of the wrong answer: for `int[]`/`long[]`/`short[]`/`double[]`,
every mismatch position *except* 0 answered `-1`, at every length. Position 0
was right only because of the `if (a[0] != b[0]) return 0` early-out — so a
vector that only tested position 0 would have passed on the broken VM.

`byte[]` and `char[]` were unaffected for an unrelated reason: those four
`mismatch` overloads have native overrides
(`vm/src/runtime/interpreter/native_override.rs`) and never execute this
bytecode. **A type-partial symptom like that is a signal to check the override
list before concluding anything about element width.**

## What the parameter trace actually showed

Re-running the failing method with H2's own tracing enabled —

```
-Dhibernate.connection.url="jdbc:h2:mem:db1;…;TRACE_LEVEL_SYSTEM_OUT=3"
```

— logs every `setBigDecimal` / `setLong` / `addBatch` / `executeBatch` with its
values. Extracting all 100 bindings (50 inserts, 50 updates) shows:

* every insert binds a **distinct** `(xval,yval)` with a distinct `id`;
* every update binds the row's **own, unchanged** `(xval,yval)` with its
  matching `id` — `id=10` is bound `x=0.9000000000000000222`,
  `y=0.6216099682706644`, exactly what row 10 already holds;
* **no duplicate `(x,y)` pair appears anywhere**, and no id repeats in a phase.

So hypothesis 1 (a batch executed twice) and hypothesis 2 (a batch entry
carrying an earlier row's values) are both excluded: the parameters reaching H2
are correct, and the corruption is inside H2's own comparisons.

`probes/BatchXyProbe.java` additionally excludes a third possibility the
original filing did not consider — that the JIT miscomputed
`new BigDecimal(i * 0.1d).setScale(19, DOWN)` or `Math.cos` and produced two
genuinely equal rows. It does not; the per-row arithmetic is exact.

## How it was isolated

1. **Hibernate-free reproducer.** `probes/H2BatchBindProbe.java` performs the
   same insert/update/delete batch shape directly against H2 — no Hibernate, no
   JUnit. It fails under JIT in ~20 s and passes under `--nojit`, removing a
   ~90 s fixture from every later iteration. (Its symptom is a lock timeout
   rather than the unique violation; same cause, different downstream victim.)
2. **Bisect — with the lever's own control first.**
   `CRATONVM_DBG=jit-bisect-only=zzzNoSuchPrefix` (a prefix matching nothing, so
   nothing may compile) must go green, or the lever is a silent no-op and every
   arm reads as a false exoneration. It did. Then `jdk/` →
   `jdk/internal/util` → `jdk/internal/util/ArraysSupport`, and
   `CRATONVM_JIT=deny=ArraysSupport.mismatch` alone made the failure vanish.
   Note `deny=org/h2/` did **not** help — the miscompile was never in H2.
3. **Naming the bad argument.** `CRATONVM_DBG=jit-dispatch` prints each
   dispatched callee with its first four argument words:

   ```
   [JIT_DISPATCH] jdk/internal/util/ArraysSupport.vectorizedMismatch(Ljava/lang/Object;JLjava/lang/Object;JII)I
        kind=3 num_args=6 arg0=0x25049d81478 arg1=0x7ff700000000
                          arg2=0x25049d814c0 arg3=0x7ff700000000
   [JIT_DISPATCH_RET] … ret=0xffffffffffffffff (-1)
   ```

   `arg1`/`arg3` are the two `long` offsets; both should be 16.
4. **Splitting "bad native args" from "caller miscompiled".** Both faults
   produce the identical visible answer, so the symptom cannot separate them.
   `probes/MismatchShapeReplicaProbe.java` rebuilds the exact method shape in
   pure Java with a plain Java stand-in for the native; it is **correct**, which
   rules out the caller's branch and its `~i` arithmetic and leaves the native
   call.

## Blast radius

Every JIT-compiled read of those nine statics was wrong, not only this one.
Known consumers include `jdk.internal.util.ByteArray` / `ByteArrayLittleEndian`
and the `Unsafe.get*Unaligned` decoders behind `java.util.zip` header parsing
and serialization. **The Hibernate failure is the symptom that happened to be
caught, not the extent of the defect** — a broader suite re-run after this
lands is worthwhile and may retire unrelated open failures.

The audit of the other fixup call sites is bounded and clean: `UnsafeConstants`
(`ADDRESS_SIZE0:I`, `PAGE_SIZE:I`, `BIG_ENDIAN:Z`, `UNALIGNED_ACCESS:Z`,
`DATA_CACHE_LINE_FLUSH_SIZE:I`) and `File.separatorChar:C` /
`pathSeparatorChar:C` all take integer-shaped values legitimately; every other
site writes a reference. The nine base-offsets were the only mismatch.

## The fix

`post_clinit_fixup`'s `set_static_by_name` now coerces the injected value to the
field's declared descriptor, and **refuses** — with a warning, and without
writing — when the value cannot represent that type (which then shows as a
shortfall in the `populated (n/18)` line rather than a silent success).

Checked once in the helper rather than asked of ~30 call sites that would each
have to track the JDK's field types forever: the descriptor is right there next
to the name. A fixup that writes the wrong width is worse than no fixup at all,
because a swallowed `<clinit>` at least leaves a well-typed zero.

## Test coverage

* `vm/src/vm/vm_util.rs::post_clinit_fixup_typing_tests` — four unit tests over
  `coerce_static_to_descriptor`. They assert the `Value` **variant**, not the
  numeric value: an `assert_eq!(as_i64(…), 16)` passes just as happily on the
  broken one.
* `regression-suite/src/RArraysMismatch.java` — warms `ArraysSupport.mismatch`
  on preallocated arrays so the assertions run against **compiled** code
  (interpreted execution is correct, so a cold vector passes on the broken VM),
  then scans every length 1..24 × every mismatch position for six element types,
  plus the ranged overload. It deliberately does not settle for "unequal arrays
  compare unequal": the bug flipped the mismatch *index*, and
  `Arrays.equals(int[],int[])` does not use this helper, so an equals-only
  vector stays green on the broken VM.

  **Confirmed to FAIL on the pre-fix binary** (`int[] len=2 flip=1 got=-1`)
  rather than passing vacuously.

## Verification (2026-08-04, Windows, real-JDK 25)

| Check | Pre-fix | Fixed |
|---|---|---|
| `RArraysMismatch` | `AssertionError: int[] len=2 flip=1 got=-1` | **PASS**, `CK acc=18109415` — byte-identical to HotSpot |
| `regression-suite/run.sh` (25 vectors) | — | **25 passed, 0 failed** |
| `ArraysMismatchProbe` (689,997 checks) | 177,935 wrong | **0 wrong** |
| `MismatchShapeProbe` length×position scan | 276 divergent rows | **0 divergent rows** |
| `H2BatchBindProbe` (300 reps) | fails in rep 0 | **0 failures** |
| `BatchTest`, JIT, all 4 methods | `ok=0 failed=4`, 4 unique-index violations | **`ok=3 failed=1`, zero `XY_INDEX_9` anywhere** |
| `cargo test -p cratonvm-vm --lib` | — | 2383 passed; 3 failures pre-existing on this tree and unrelated (see below) |

The one remaining `BatchTest` failure is `testBatchInsertUpdate` (`N=5000`)
hitting the plain Hibernate-internal 120 s `@Timeout` — the long-tracked
throughput margin this class was originally filed under
(`hib-120s-junit-timeout-cluster-20260716.md`), not a constraint violation.

**That timeout is not caused by this fix, and it is worse than the record.**
Solo, on a quiet box:

| Arm | `testBatchInsertUpdate` |
|---|---|
| HotSpot | 5.6 s |
| CratonVM, fixed | 273.0 s |
| CratonVM, pre-fix binary + `CRATONVM_JIT=deny=ArraysSupport.mismatch` (correct answers, old code) | 382.7 s |

The fix makes it **faster**, so it is exonerated as the cause. But 273 s is
~2.6× the 101–107 s this same method recorded solo on 2026-07-17, and 49× the
HotSpot control — so the throughput margin has itself regressed on dev since
July, independently of this bug. That belongs to the hib-120s cluster doc and
was not investigated here; it is recorded so the next reader does not have to
re-derive it.

The 3 `cargo test -p cratonvm-vm --lib` failures
(`native::jni::tests::jni_function_table_extended_to_234`,
`native::jni::tests::jni_nio_slots_not_stub`,
`redefine_immunity_tests::layout_immunity_is_not_open_coded`) were baselined on
this same tree with the change stashed and fail identically there.

## Reproducing (pre-fix)

```bash
# Hibernate-free, ~20 s
cratonvm.exe -cp "<probes>;h2-2.4.240.jar" H2BatchBindProbe 50 20 20

# The original fixture, run from apps/hib-suite-runner (the runner is CWD-sensitive)
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 cratonvm.exe --java-home "<jdk25>" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.batch.BatchTest
```

## Separate, unfixed observation

Under CratonVM, H2's lock-timeout message renders as `Timeout trying to lock
table {0}` with the `MessageFormat` placeholder unsubstituted, where HotSpot
fills in the table name. Different defect; not investigated here.

## Not a reopening of HIB-CV-38/39 or the array-header-corruption OOM

Checked before filing: `HIB-CV-38-boolean-type-field-static-slot-corruption-FIXED.md`
and `jit-inline-alloc-array-header-corruption-hibernate-batch.md` both describe
`DynamicBatchFetchTest` (a different class), and their symptoms — a
`Boolean.FALSE`-is-null NPE at JUnit-launcher bootstrap, and a `GC: inconsistent
header … kind=Object but array_length=N` corruption storm ending in
`OutOfMemoryError` — are absent from every `BatchTest` failure here.
