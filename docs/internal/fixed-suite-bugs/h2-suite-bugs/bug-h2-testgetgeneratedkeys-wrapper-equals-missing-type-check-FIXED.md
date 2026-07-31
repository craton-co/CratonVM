# `TestGetGeneratedKeys` "corrupt Value cell" — FIXED; it was never heap corruption, it was `Integer/Boolean/…equals(Object)` skipping the `instanceof` test

## Status
**FIXED** — root-caused and fixed 2026-07-31 on branch
`fix/h2-genkeys-corrupt-cell-20260731` (commit `7656ad39`), merged to `dev`.
Supersedes the original OPEN write-up, which attributed the failure to the
`HIB-CV-32` heap-cell-corruption family. **That attribution was wrong.**

## What the original report said
`org.h2.test.jdbc.TestGetGeneratedKeys.testColumnNotFound` failed with

```
Expected an SQLException or DbException with error code 42122, but got a null
	at org/h2/test/jdbc/TestGetGeneratedKeys.testColumnNotFound(TestGetGeneratedKeys.java:414)
```

immediately after three `[ERROR] gen_heap::read_slot: corrupt Value cell
(out-of-range discriminant)` diagnostics. Because that diagnostic is the
defence-in-depth half of the `HIB-CV-32` fix, the report concluded the heap was
still producing corrupt cells and filed it as a new occurrence of that family,
with the root cause "still open". It also recorded the failure as not confirmed
deterministic.

## What it actually was
`native-builtins/src/lang_math.rs`'s wrapper `equals(Object)` natives —
`native_wrapper_int_equals` (registered for `Integer`, `Boolean`, `Character`,
`Byte`, `Short`) plus the `Long`/`Float`/`Double` variants — **never tested the
argument's type**. They took `args[1]`, read its field 0, and compared it to
the receiver's field 0:

```rust
let a = ctx.get_field(this, 0);
let b = ctx.get_field(other, 0);      // `other` can be ANY object
Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
```

The JDK contract is an instance test first —
`obj instanceof Integer i && value == i.value` — and these natives own the
whole method (the real JDK bytecode never runs for them), so the type test has
to live in the native. It didn't.

### How that produced the exact failure
H2's `CommandContainer.update` gates the entire generated-keys path on

```java
if (generatedKeysRequest != null && !Boolean.FALSE.equals(generatedKeysRequest)) {
```

and the test's first assertion calls `stat.execute("INSERT …", new int[] {0})`.
So CratonVM evaluated `Boolean.FALSE.equals(new int[] {0})`:

* `a` = `Boolean.FALSE`'s field 0 = `Value::Int(0)`
* `b` = field 0 of the `int[]` — an **array**, read through the *object-field*
  path, i.e. the array body decoded as a 16-byte tagged `Value` cell

The array body is zero, so `b` also decoded as `Value::Int(0)`, `equals`
answered **true**, `!true` skipped the whole `executeUpdateWithGeneratedKeys`
branch — including its `idx < 1 || idx > cnt → COLUMN_NOT_FOUND_1` check — and
`execute` returned normally with no exception at all. Exactly "got a null".

### The corrupt-cell diagnostic was a symptom, not the disease
Reading field 0 of a `String[]`/`int[]` through `gen_heap::get_field` reads raw
array element bytes as a tagged `Value`, which of course does not decode to a
valid discriminant — so the `read_slot` guard fired and substituted a benign
`null`. `CRATONVM_DBG_CELLCORRUPT=1` made this unambiguous: the holder of every
"corrupt" cell was an **array**, and the caller was the wrapper `equals`
native.

```
[CELLCORRUPT] holder=0x201125f0570 (young_from=true old=false) class_id=6
              class=java/lang/String kind=0x01 num_slots=2 array_len=2 index=0
              raw0=0x0000020040425660 raw1=0x0000020040433468
   1: get_field                    gc/src/gen_heap.rs:2563
   4: get_field                    vm/src/vm/vm_exec.rs:7632
   5: native_wrapper_int_equals    native-builtins/src/lang_math.rs:3715
  15: execute_invokevirtual_cached
```

`kind=0x01` is the array kind: a two-element `String[]` read with object-field
semantics. The heap was intact the whole time. **No part of this failure
belongs to the `HIB-CV-32` family.**

Contrary to the original report the failure is also fully **deterministic** —
3/3 identical reproductions before the fix, `--nojit` and with the JIT on.

## Blast radius beyond H2
A differential probe against stock JDK 25 (`WrapperEqualsProbe`, 26 cases)
found **7** wrong answers on pre-fix `dev`, identical under `--nojit` and with
the JIT:

| expression | before | correct |
|---|---|---|
| `Boolean.FALSE.equals(new int[] {0})` | true | false |
| `Boolean.FALSE.equals(Integer.valueOf(0))` | true | false |
| `Boolean.TRUE.equals(Integer.valueOf(1))` | true | false |
| `Integer.valueOf(1).equals(Short.valueOf((short) 1))` | true | false |
| `Integer.valueOf(65).equals(Character.valueOf('A'))` | true | false |
| `Integer.valueOf(1).equals(Byte.valueOf((byte) 1))` | true | false |
| `Byte.valueOf((byte) 1).equals(Short.valueOf((short) 1))` | true | false |

Any `Map`/`Set`/`List.contains` holding mixed boxed-key types could return the
wrong entry because of this, so the practical reach is much wider than the one
H2 assertion that surfaced it.

## The fix
`native-builtins/src/lang_math.rs` — a `wrapper_same_class` helper (class-id
compare, with a class-name compare on the cold mismatch path so multiple loaded
copies of a class still match) called before either operand's field 0 is read,
in all four wrapper `equals` natives. Every wrapper class is `final`, so "same
class" is exactly `instanceof`.

Unit tests in the same file:
`wrapper_int_equals_rejects_a_different_class_with_the_same_payload`,
`wrapper_int_equals_rejects_an_array_argument`, and one each for
`Long`/`Float`/`Double`.

## Verification
* `WrapperEqualsProbe` — 7 FAIL → PROBE PASS, `--nojit` and JIT.
* `org.h2.test.jdbc.TestGetGeneratedKeys` — failed 3/3 before; passes in both
  `--nojit` and JIT after, with zero `corrupt Value cell` diagnostics.
* `cargo test -p cratonvm-native-builtins --lib wrapper_` — 10 passed.
* **Full 218-class H2 suite, `jit-real`, `--Xmx 1g`, A/B pre-fix vs fixed**
  (two concurrent runs, ~4.2 h each):

  | arm | PASS | FAIL | HANG | CRASH |
  |---|---|---|---|---|
  | pre-fix | 155 | 24 | 38 | 1 |
  | fixed | 154 | 26 | 36 | 2 |

  11 classes changed status. Only one is attributable to this change:
  **`TestGetGeneratedKeys` FAIL → PASS**. The rest are pre-existing
  instability, confirmed rather than assumed:
  - 6 are `HANG`↔`FAIL`/`CRASH` flips of classes that are unstable in both arms
    (`TestLob`, `TestRunscript`, `TestSynth`, `TestPgServer`,
    `TestMultiThreaded`, `TestOutOfMemory` — the last already has its own
    known-issue doc).
  - `TestKeywords` FAIL → PASS is **not** a second win from this fix: the
    pre-fix failure was `CloneNotSupportedException`, i.e. a flake of
    `bug-h2-testtemptables-clonenotsupportedexception-thread-clone-frame.md`.
  - `TestMvccMultiThreaded`, `TestSort`, `TestValueMemory` went PASS → worse,
    so all three were re-run **interleaved A/B, 3 reps each**: `TestSort`
    (147/149/149/145/140/138 s) and `TestValueMemory` (79/84/66/54/48/49 s)
    passed 3/3 in both arms — their suite `HANG`s were 300 s-timeout artifacts
    of host load (the box ran at load average 12–113 with up to 1075 other
    sessions during the window). `TestMvccMultiThreaded` failed 1/3 on the
    **pre-fix** binary and 0/3 on the fixed one, with
    `JdbcSQLTimeoutException: Timeout trying to lock table "TEST"` — flaky
    under contention in both arms, not a regression.

## Audit of the same defect class elsewhere
`BigInteger`, `BigDecimal`, `LocalDate`, `LocalTime`, `LocalDateTime`,
`ZonedDateTime`, `Instant`, `Duration`, `Period` and `java.util.Date` all have
`equals` natives written in the same shape — read the other operand's fields
with no type test. A second differential probe (`ValueEqualsProbe`, 31
cross-type and same-type cases) **passes on pre-fix `dev`** in real-JDK mode:
those registrations do not shadow the real JDK bytecode, so nothing is
reachable there in the supported configuration. They stay latent hazards for
`--synthetic-jdk` mode only, and were deliberately left alone.

If they are ever hardened, note that the fix is **not** a copy-paste of
`wrapper_same_class`: `java.util.Date` and `java.util.Calendar` are **not
final** (`java.sql.Timestamp extends java.util.Date`, and `Date.equals` is
specified to compare `getTime()` across subclasses), so those two need a real
subtype test, not an exact-class one. The nine `final` classes above can use
exact-class.

## `TestDiskFull` — the "possibly related, not confirmed" note
The original doc flagged `org.h2.test.synth.TestDiskFull` failing with
`AbstractMethodError: org/h2/value/Value.getValueType()I has no Code attribute`
inside `ScriptCommand`'s row serialization, wondering whether it was the same
family. Investigated this session and **not reproduced**:

* Long-form runs (the `write op count` loop actually engaging, 461 and 331
  iterations, ~19 min each, `--Xmx 1g`, JIT on): **pass**, pre-fix and fixed.
* Short-form runs matching the suite runner's fresh-scratch-CWD shape: 60
  pre-fix + 60 fixed. **Zero** `AbstractMethodError` in either arm.

So that specific `AbstractMethodError` is a single unreproduced observation; it
is not evidence for the `HIB-CV-32` family and should be filed on its own if it
resurfaces.

`TestDiskFull` *is* genuinely unstable for other reasons, which are now
recorded separately in
`docs/known-issues/h2/bug-h2-testdiskfull-classid0-corruption-segv-cce.md`
— `SIGSEGV` after a `class_id=ClassId(0)`/`num_slots=0` guard burst, a
`cratonvm.synthetic.AnonymousObject$3 cannot be cast to [Ljava.lang.String;`
`ClassCastException`, and >300 s hangs, none of which occur on stock HotSpot.
Its other failure mode, `MVStoreException: Chunk N not found`, **does** occur
on stock HotSpot JDK 25 (3/28 runs, same message) and is upstream H2
fault-injection flakiness, not a CratonVM defect.

## Wider lesson
`gen_heap::read_slot: corrupt Value cell` says a cell did not decode as a
tagged `Value`. That is **not** synonymous with heap corruption: reading an
*array* with object-field semantics produces the identical diagnostic. Run with
`CRATONVM_DBG_CELLCORRUPT=1` and read the holder's `kind=` field and the
backtrace before attributing it to a GC/reference-integrity defect. The same
caution applies to the sibling report
`bug-h2-testmvstorecacheperformance-sigsegv-hib-cv-32-family.md`, which cited
this one as corroborating evidence for its `HIB-CV-32` attribution — that
corroboration has been withdrawn in place.

## Repro (pre-fix)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestGetGeneratedKeys
```
Deterministic (3/3). Minimal repro: `Boolean.FALSE.equals(new int[] {0})`
should be `false`.
