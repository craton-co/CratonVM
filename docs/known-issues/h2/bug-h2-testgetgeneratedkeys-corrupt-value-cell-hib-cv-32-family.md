# `TestGetGeneratedKeys` hits the known (partially-fixed) `HIB-CV-32` heap-cell-corruption family — root cause of the corruption itself still open

## Status
**OPEN** — new occurrence, in H2 rather than Hibernate, of an
already-tracked defect family whose crash-prevention half is fixed but
whose actual corruption *source* is not. Found while investigating H2
suite regressions in the twelfth-pass `TestUpgrade` session's follow-up
full-suite run (2026-07-30/31).

## Severity
**LOW-MEDIUM** for this specific manifestation (defensively degrades to a
wrong-but-not-crashing result — a `null` where a real value was expected —
so it fails a test assertion rather than corrupting data or crashing), but
it's evidence the underlying heap-cell corruption this family describes is
still live on current `dev` and can now reach H2 workloads, not just
Hibernate ones.

## Affected test class (H2 suite, this run)
`org.h2.test.jdbc.TestGetGeneratedKeys` (`testColumnNotFound`)

## Symptom
```
[ERROR] gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)
  — returning null instead of a UB-on-match Value. Heap reference-integrity
  defect (see HIB-CV-32). slot=0x20055382c88 raw0="0x00000200100246d8" raw1="0x00000000000003ac"
[ERROR] gen_heap::read_slot: corrupt Value cell ... (fires 3x total, two different slots)

Exception in thread "main" java.lang.AssertionError:
  Expected an SQLException or DbException with error code 42122, but got a null
	at org/h2/test/jdbc/TestGetGeneratedKeys.testColumnNotFound(TestGetGeneratedKeys.java:414)
	at org/h2/test/TestBase.checkErrorCode(TestBase.java:1689)
```
The test expects a specific SQL error (42122, "column not found") to be
thrown when requesting a nonexistent generated-key column; instead it gets
a plain `null` back with no exception at all.

## Root cause — mechanism understood, source not
The `[ERROR] gen_heap::read_slot: corrupt Value cell` diagnostic is an
**already-existing, already-shipped** defensive guard
(`types/src/value.rs`'s `read_value_checked` / `gen_heap.rs::read_slot`),
added as the "defense-in-depth" half of the
[HIB-CV-32](../../internal/fixed-suite-bugs/hibernate/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md)
fix (`dev@c9258e17`, 2026-06-23): rather than blindly constructing a
`Value` enum from a raw heap cell's bit pattern (which, for a corrupt cell,
could produce an out-of-range discriminant and crash with a wild-jump
SIGSEGV in `CompactValue::from_value`), it now validates the discriminant
first and substitutes a benign `null` plus this diagnostic whenever the
cell doesn't decode to a valid `Value` shape.

That fix's own root-cause half (`gen_heap.rs`'s `promotion_oom_risk`
diverting `--nojit` young collections into a corrupting non-moving sweep
under GC-root-visibility uncertainty) was root-caused and fixed for the
**specific reproduction** that motivated HIB-CV-32 at the time (a boxed
`java.lang.Byte` reclaimed-then-reused during a Hibernate BLOB bind). This
H2 failure is the **same detection mechanism firing again** — i.e. the
heap is still producing corrupt cells somewhere, just now safely degrading
to `null` instead of crashing — but in a completely different workload
(`TestGetGeneratedKeys`, no BLOB binding, no Hibernate involved at all).
This strongly suggests either:
1. A **different** trigger for the same `gen_heap` corruption class that
   the June 23 fix's specific `promotion_oom_risk` change doesn't cover
   (i.e. root cause #1 from HIB-CV-32 was fixed for its own repro, but the
   general "a reclaimed-then-reused object's field slot decodes as a
   corrupt `Value`" defect has more than one path into it), or
2. The **same** trigger, reachable from a much wider range of workloads
   than originally scoped (young-generation promotion/GC-root-visibility
   races are not inherently Hibernate/BLOB-specific).

Neither was investigated further this session — this doc exists to flag
that the defensive guard is firing on a genuinely new, non-Hibernate
workload, which is useful signal for whoever continues the HIB-CV-32-family
root-cause work.

## Suggested next step
Re-run this exact test with the corrupt-cell diagnostic's surrounding
context captured (it already logs `slot`/`raw0`/`raw1` — cross-reference
`class_id`/`real_field_count`-style context the way the sibling
[HIB-CV-31](../../internal/fixed-suite-bugs/h2-suite-bugs/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md)
investigation did for its own corrupt-receiver finding) to identify what
object/field is actually corrupt here, then check whether it fits the
already-diagnosed `promotion_oom_risk`/non-moving-sweep mechanism or is a
genuinely new path into the same symptom family.

## Possibly related, not confirmed
`org.h2.test.synth.TestDiskFull` (same full-suite run) failed with
`AbstractMethodError: method org/h2/value/Value.getValueType()I has no
Code attribute` during `ScriptCommand.add()`'s row serialization — an
abstract-method dispatch failure that is architecturally impossible for
correct bytecode (H2's `Value` class is abstract; every real instance must
be a concrete subclass with its own `getValueType()` body). This has the
same *shape* as `HIB-CV-31`'s own finding (a corrupt/wrong-class receiver
producing a "dispatch" symptom that's actually downstream of heap
corruption, not a genuine dispatch bug) but was not traced far enough this
session to confirm it's the same `HIB-CV-32` family rather than an
unrelated defect — flagging for whoever picks this up rather than claiming
it as the same root cause. `TestDiskFull` is also a fault-injection
("disk full") stress test, which is exactly the kind of environment likely
to expose GC-pressure/allocation-failure-adjacent races, consistent with
(but not proof of) the same general defect class.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbc.TestGetGeneratedKeys
```
Not yet confirmed deterministic across multiple runs this session (GC-timing
-dependent corruption is inherently probabilistic, per HIB-CV-32's own
history).
