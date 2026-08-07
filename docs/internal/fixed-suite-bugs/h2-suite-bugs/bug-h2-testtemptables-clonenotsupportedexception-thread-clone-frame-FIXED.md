# `TestTempTables` `CloneNotSupportedException` via a `java.lang.Thread.clone` frame — array receivers dispatched through their COMPONENT class

## Status

**FIXED 2026-08-07** (`fix/h2-clone-cnse-residual-20260807a`). Two separate
things lived under this title and both are now closed *for this page*:

1. **The dispatch defect this page is named for** — array receivers routed onto
   their component class's method body. Fixed twice: the 2026-07-31
   interpreter-inline-cache fix (below, still in the tree, still covered by
   `vm/tests/array_receiver_dispatch.rs`), and a 2026-08-07 change that stops
   *any* array-typed call site consulting the receiver's header at all.
2. **The H2 symptom this page was opened for** — `TestTempTables` failing on
   `Arrays.copyOf(long[], int)`. That was **never** defect 1. It is a heap
   defect: a live `long[]` reference is redirected, by the invoke's own
   forwarding read barrier, onto an unrelated old-generation object. Four
   witnesses, quoted below. It is **re-homed** to
   [`../../../known-issues/h2/bug-h2-classid0-stale-address-family.md`](../../../known-issues/h2/bug-h2-classid0-stale-address-family.md),
   which that page's own closing instruction asks for, and which is REOPENED as
   part of this change.

The user-visible effect of the 2026-08-07 fix on the surviving heap defect is
that the same event now reports what is actually wrong:

```text
before: java/lang/CloneNotSupportedException
            at java/util/Arrays.copyOf(Arrays.java:3617)
            at java/lang/Thread.clone(Thread.java:1037)

after:  java/lang/ClassCastException:
          jdk.internal.misc.InnocuousThread cannot be cast to [J
            at java/util/Arrays.copyOf(Arrays.java:3617)
```

That is not cosmetic. The first message sent two sessions looking for a
dispatch bug; the second names the object, and arrives alongside a
`cratonvm::gc::guard` verdict that says which barrier produced it.

---

## What the 2026-08-07 witnesses show

`org.h2.test.db.TestTempTables`, `--nojit --Xmx 1g`, real JDK 25, one class per
process, 6 concurrent workers on the Azure host. Four occurrences:

| binary | receiver address | `receiver_class` | `receiver_kind` |
|---|---|---|---|
| fix3 | `0x20010039668` | `jdk/internal/misc/InnocuousThread` (cid 733) | `Object` |
| fix3 | `0x200100e37f8` | `org/h2/mvstore/FileStore$BackgroundWriterThread` (cid 899) | `Object` |
| fix3 | `0x200100e1dd0` | `java/lang/Thread` (cid 29) | `Object` |
| fix6 | `0x2001003bb50` | `java/lang/Thread` (cid 29) | `Object` |

Every one is `java.lang.Thread` **or a subclass**, every one has
`gc_flags = 0x01` (old generation) and `gc_age = 2`, and every one is a *valid,
live* object — not an all-zero header. That is the whole "why `Thread` twice?"
puzzle this page carried for a week: `Thread.clone()` is inherited, so a
receiver of *any* `Thread` subclass produces a `java/lang/Thread.clone` frame.

The fourth witness carries the decisive line, from the instrument added by this
change:

```text
site="array-typed call site, non-array receiver"
target_method=[J.clone()Ljava/lang/Object;
receiver_class=java/lang/Thread  receiver_kind=Object  in_young=false
header="[1d,00,00,00, 00, 00, 02, 01, 08,65,00,00, 13,00,00,00, 52,d8,9f,35,00,02,00,00]"
object_degradations=0

…the receiver as the OPERAND STACK handed it over, before `load_and_forward`:
pre_refresh_obj="0x20042853308"   barrier_rewrote=true
pre_refresh_kind=Array            pre_refresh_class=java/lang/Object
pre_refresh_header="[00,00,00,00, 01, 0b, 00, 00, 06,17,ce,01, 01,00,00,00, 00×8]"

…where 0x20042853308 stood in the owning thread's bookkeeping:
holder=frame#14 org/h2/mvstore/tx/BitSetHelper.flip pc=38 local[0] kind=0 live=true
```

Read the pre-refresh header against `ObjectHeader`: `class_id=0`, `kind=1`
(`Array`), `element_type=0x0b` (`Long`), `gc_age=0`, `gc_flags=0`,
`shape=1`. **It is a perfectly good `long[1]`** — exactly the
`VersionedBitSet.bits` the call site is holding — and `BitSetHelper.flip`'s
`local[0]` still points at it, live.

So the frame is innocent, the root scan is innocent, and the array is intact.
What is wrong is the single statement between them:
`execute_invoke_kind`'s forwarding read barrier

```rust
*obj = shared.mem.heap.load_and_forward(*obj);
```

turned `0x20042853308` (a live `long[1]`) into `0x2001003bb50` (an old-gen
`java.lang.Thread`). `load_and_forward` does exactly one thing — reads the
source's mark word, and if its low two bits are `MARK_FORWARDED` follows the
upper 62 as a relocation target, accepting any destination that `is_object_address`
likes. A re-served old-gen `Thread` passes that check.

That is a heap/reference-integrity defect, not a dispatch defect, and it is the
`ClassId(0)` family's RE-SERVED face. See the re-homed page for the hunt.

## Rates, and the negative controls that matter

| arm | runs | events |
|---|---|---|
| `TestTempTables` `--nojit`, binaries with the array-site fix | 46 | **4** |
| `TestTempTables` JIT-on, same binaries | 30 | 0 |
| `TestTempTables`, binaries WITHOUT the array-site fix (this page's own instrumented dev builds) | 24 | 0 |
| `H2InsertScaleProbe 25 1000` — this page's own 2026-07-31 residual repro | 16 | 0 (`failed=0` every run) |
| `H2InsertScaleProbe 25 3000` | 1 | 0 |
| `CloneSpinProbe` — the `[J.clone()` bytecode alone, 8 threads | 28.4 M dispatches / 90 s | 0 |

Two of those rows are load-bearing:

* **`CloneSpinProbe`** (`docs/internal/repros/h2-clone-spin-20260807/`) drives
  the H2 shape with the database removed: an `AtomicReference` holding an
  immutable `long[]`-backed bit set, replaced by a CAS'd
  `BitSetHelper.flip` → `Arrays.copyOf` → `original.clone()` copy. 28.4 million
  executions of the exact failing bytecode, under the JIT, at `--Xmx 512m`,
  zero failures. **The dispatch is not the fragile part.** Whatever breaks needs
  H2's heap, not H2's call shape.
* **`H2InsertScaleProbe 25 1000`** is the repro this page's own "Possible
  residual observed 2026-07-31" section recorded as failing **all 25 threads**.
  Sixteen runs on current `dev` and it does not reproduce at all. That residual
  is withdrawn as an independent data point; it is not evidence of anything
  this page can still act on.

The `--nojit`-only distribution (4 in 46 vs 0 in 30) is recorded as observed.
It is not established as a JIT-vs-interpreter *property* — the interpreter arm
executes far more interpreted invokes per second, and the barrier this defect
runs through is on the interpreter's slow dispatch path.

## Correcting this page's own root-cause claim

The 2026-07-31 analysis said the H2 trace was an array receiver dispatched
through its component class id, and that producing *this* frame "additionally
requires the `long[]` in `VersionedBitSet.bits` to carry `Thread`'s class id in
its header". **That requirement can never be met**, and the source says so
outright:

```rust
// vm/src/runtime/interpreter/opcodes.rs — Instruction::Newarray
let arr = gc_alloc_array(shared, thread, ClassId::new(0), element_type, length as usize)?;
```

`Newarray` — the only bytecode that makes a `long[]` — stamps `ClassId(0)`.
`class_id_of(long[])` is therefore *always* `java/lang/Object`, so
component-class-id routing can only ever select `java.lang.Object.clone()` for
a `long[]` receiver. It has no way to reach `java.lang.Thread.clone()`. Only
`Anewarray` stores a real component class id, and `long[]` is not an
`Anewarray`.

This is also why the 2026-07-31 verification found **7/7 clean runs on the
pre-fix binary** with a diagnostic armed for exactly this: there was nothing
for it to catch. That result was read at the time as "the H2 symptom is rare";
it was actually "the H2 symptom is not this mechanism".

The mechanism itself is real and the fix for it is real — see "The dispatch
defect" below, whose regression test still fails on a pre-fix binary. It simply
never produced this page's trace.

## The dispatch defect (real, fixed, retained)

`Anewarray` stores the **component** class id in the array's object header and
`Newarray` stores `ClassId::new(0)`, so

```text
class_id_of(Foo[])   == class_id_of(Foo)
class_id_of(long[])  == ClassId(0) == class id of java/lang/Object
```

Per JVMS §4.4.1 an array type's method table comes from `java.lang.Object`, so
a dispatch path that keys on `heap.class_id_of(receiver)` must not select the
component class's body. `someArray.m()` otherwise runs `Foo`'s body with the
array as `this` — reading array *element* 0 as *field* 0.

**2026-07-31** added `kind_of(receiver) == Array → CacheMiss` to the three
receiver-validating arms of `execute_invokevirtual_cached`, made
`invoke_or_native`'s NoSuchMethod receiver-class-chain rescue skip array
receivers, and made `invoke_on_class_shared_inner`'s virtual retarget yield
`None` for them. Reproduction and differential verification:

| | `Foo[].toString()` | `Foo[].hashCode()` |
|---|---|---|
| HotSpot jdk-25 | `[LFoo;@7ad041f3` | identity hash |
| CratonVM (before) | `Foo-toString-v0` | `0x5eed0000` (Foo's override) |
| CratonVM (after) | `[LFoo;@52` | identity hash |

`vm/tests/array_receiver_dispatch.rs` still covers this and still passes.

**2026-08-07** closes the same family one level up. Every guard above asks the
*receiver's header* whether it is an array — which is correct only while the
header is trustworthy. When the CONSTANT POOL names an array type, JVMS §4.4.1
settles the target statically: an array class declares no methods, there is no
subclass of `[J` that could override `clone()`, and the header has nothing to
contribute. Four sites now decide from the call site instead:

* `vm/src/runtime/interpreter/invoke.rs` — `execute_invoke_kind`'s
  `invoke_class` selection;
* `vm/src/runtime/interpreter/dispatch_virtual.rs` —
  `execute_invokevirtual_vtable_fast` cedes an array-typed site to the slow
  dispatcher rather than resolving it against the receiver's vtable;
* `vm/src/jit/helpers.rs` — `virtual_dispatch_target_for_receiver` and
  `virtual_dispatch_target_cached`.

This is what turns the H2 witness from `CloneNotSupportedException` into
`ClassCastException: … cannot be cast to [J`: the VM no longer runs
`Thread.clone`'s body, it runs `Object.clone` (as JVMS requires), and the
`checkcast [J` that follows the call site then reports the truth.

## The instrument

`vm/src/memory/reclaim_guard.rs` gained
`report_impossible_dispatch_terminal`, reached from three places that are
unreachable from well-formed code:

* `java.lang.Thread.clone` and `java.lang.Enum.clone` dispatch (bodies that are
  `throw new CloneNotSupportedException()` and nothing else);
* **an array-typed call site with a non-array receiver** — a provable verifier
  violation, and the one that fires *before* the target is chosen, so it is
  independent of which wrong body a corrupt header happens to select.

It reports, in one place: the receiver's full 24-byte header (including the
mark word — a 16-byte dump stops one field short of `is_forwarded`), which
generation it is in, the pre-barrier receiver and whether `load_and_forward`
rewrote it, the owning thread's root-snapshot provenance for *both* addresses,
the interpreter frames, and `object_degradation_count()` — the counter the
`ClassId(0)` page's closing instruction asks for and which nothing in the VM
read until now.

Two gaps it closes along the way:

* `report_reclaimed_receiver` asks the free-list question only for
  `actual_cid == 0`, because it has callers that fire in bulk on healthy runs.
  That gate is blind to precisely the re-served face. The clone terminals now
  use a `_forced` variant.
* the `Thread.clone` reporter lived only in `execute_invoke_kind`, which a
  JIT-compiled call site never passes through (it reaches
  `invoke_or_native` → `invoke_on_class_shared_inner` → `try_stackless_invoke`).
  That reporter now also sits in `try_stackless_invoke`, covering `Thread` as
  well as `Enum`.

## Reproduction

* `apps/h2database-suite-runner` fixture, `org.h2.test.db.TestTempTables`,
  `--java-home <jdk25> --Xmx 1g --nojit -c <h2 testcp>`, run in its own scratch
  CWD. ~4 events in 46 runs; a passing run is 500-950 s on a loaded host, a
  failing one aborts at 90-850 s.
* `docs/internal/repros/h2-clone-spin-20260807/CloneSpinProbe.java` — the
  negative control described above.
* `docs/internal/repros/h2-insert-scale-20260731/H2InsertScaleProbe.java`
  invoked as `H2InsertScaleProbe <abs-dir> 25 1000` — this page's own 2026-07-31
  residual repro, now `failed=0`.

## Verification

* `vm/tests/array_receiver_dispatch.rs` —
  `array_receiver_dispatches_through_object_not_component_class` passes on the
  fixed tree (and its differential result against a pre-fix binary is
  unchanged from 2026-07-31).
* `cargo test --release -p cratonvm-vm --no-fail-fast`: **4063 passed / 11
  failed on BOTH arms** — the fixed tree and the pristine one (same worktree,
  `git checkout -- vm/src`, same rebuild, contemporaneous). Ten failures are
  identical on both:
  `connection_methods_carry_signatures`,
  `memory::addr_keyed::tests::the_address_keyed_table_census_is_complete`
  (its own message names `native-builtins/src/net_phase_e.rs`, which this
  change does not touch), `no_new_test_only_public_api`,
  `probe0_jboss_module_class_reachable`, `t14_all_system_natives_registered`,
  `t14_all_vm_natives_registered`, `t15_define_class_not_stub`,
  `test_jit_exception_in_handler_not_recaught_by_same_handler`,
  `test_jit_indy_after_side_effect_no_double_execution`,
  `test_precise_handler_frame_catches_a_throw_at_the_end_of_its_try`.
  The eleventh differs, in **opposite directions**:
  `socket_input_stream_timeout_is_typed_and_never_eof` failed only on the fixed
  arm and `test_pgo02_guarded_virtual_inline` only on the pristine arm. Both
  pass in isolation on the fixed tree (`ok. 1 passed` each), so both are the
  known process-global/parallel flakes, not a signal either way.
* `CloneSpinProbe` on the fixed binary: 9.2 M `[J.clone()` dispatches, 6
  threads, `--Xmx 512m`, JIT on — `failed=0`. The site-driven dispatch does not
  regress the healthy path.

## Related

* [`../../../known-issues/h2/bug-h2-classid0-stale-address-family.md`](../../../known-issues/h2/bug-h2-classid0-stale-address-family.md)
  — where the surviving defect lives, REOPENED 2026-08-07 with these witnesses.
* `vm/src/jit/helpers.rs` — the "KC26 `array.clone()` bug" comments record the
  JIT-side instance of the component-class-id family (`Enum.clone() →
  CloneNotSupportedException` for enum-array clones, and the `ResolvableType[]`
  / `ResolvableType` Spring Boot `ClassCastException` behind the machine-code
  `OBJECT_KIND_OFFSET` guard).
* `vm/src/runtime/interpreter/invoke.rs`'s `try_stackless_invoke` "T15" comment
  (array class names rewritten to `java/lang/Object`).

## Appendix — the 2026-07-31 analysis, preserved

The original report claimed the two innermost frames "don't form a plausible
real call chain" and were therefore evidence of a stack-trace-construction bug.
That was wrong, and the refutation still stands — every frame is genuine and
the line numbers are exact:

* `java.base/java/lang/Thread.java:1037` is
  `throw new CloneNotSupportedException();`, the whole body of
  `protected Object clone()`; `javap -c` confirms
  `new / dup / invokespecial / athrow`.
* `java.base/java/util/Arrays.java:3617` is `return original.clone();` inside
  `copyOf(long[] original, int newLength)`, compiled to
  `aload_0 / invokevirtual #319 "[J".clone:()Ljava/lang/Object; / checkcast "[J"`.
* `org/h2/mvstore/tx/VersionedBitSet.java:25` is
  `bits = BitSetHelper.flip(other.bits, bitToFlip);` and
  `BitSetHelper.java:34` is
  `bits = Arrays.copyOf(bits, Math.max(length, wordIndex) + 1);` — both checked
  against the fixture's own sources.
* CratonVM's exception printer emits frames **outermost-first**, so
  `java/lang/Thread.clone` is the innermost frame.
* No Rust code constructs `CloneNotSupportedException` — `grep -r
  CloneNotSupported --include=*.rs` finds only registration lists — so it came
  from real bytecode.

What that analysis got wrong was the next step: it concluded the receiver was
the array, dispatched through a component class id that named `Thread`. The
receiver was never the array. See "Correcting this page's own root-cause claim".
