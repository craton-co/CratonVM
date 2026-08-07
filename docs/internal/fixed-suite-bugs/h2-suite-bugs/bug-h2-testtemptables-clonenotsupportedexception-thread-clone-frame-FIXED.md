# `TestTempTables` `CloneNotSupportedException` via a `java.lang.Thread.clone` frame

## Status

**FIXED 2026-08-07** (`fix/h2-clone-cnse-residual-20260807a`).

The receiver of `Arrays.copyOf(long[], int)`'s `original.clone()` was being
**replaced by a `java.lang.Thread` mirror, by the VM, on purpose**.
`execute_invoke_kind` carries a stale-thread-mirror recovery that substitutes a
live mirror when the receiver's address appears in the thread registry's
`former_mirror_addrs` table. Its gate was

```rust
if shared.mem.heap.class_id_of(recv) == ClassId::new(0) {
```

justified in-comment as *"we consult it only when the receiver header is
genuinely all-zero (`class_id == 0`)"*. **Those are not the same test.**
`Instruction::Newarray` allocates with `ClassId::new(0)` because an array header
carries its COMPONENT class id (JVMS §4.4.1) and `long[]`/`int[]`/`byte[]` have
none — so **every primitive array in the VM** passed the gate. A live `long[]`
that happened to sit on an address some thread mirror had previously occupied —
young addresses are recycled constantly — matched the table and was swapped for
that mirror. `clone()` then ran the mirror's inherited `Thread.clone`, whose
body is `throw new CloneNotSupportedException()` and nothing else.

That is the whole of this page's H2 symptom. It is **not** a GC bug, and the
`ClassId(0)` stale-address family it was twice attributed to is not involved:
`object_degradations = 0` on the failing run, nothing had been reclaimed, and
the correct `long[]` was still sitting live in `BitSetHelper.flip`'s `local[0]`
at the moment of failure.

The fix is one term:

```rust
// `class_id == 0` is worn by a reclaimed span, by a genuine `new Object()`,
// and by every primitive array. Only the first has an all-zero HEADER.
class_id == ClassId::new(0) && *header == [0u8; cratonvm_types::HEADER_SIZE]
```

Regression test:
`runtime::interpreter::tests::stale_mirror_recovery_skips_a_live_primitive_array`.
Differential-verified — restoring the old gate fails it with
*"a live long[] must never be mistaken for a reclaimed span and replaced by a
java.lang.Thread mirror"*.

Landed alongside it, because both were needed to see the bug at all:

* **array-typed call sites now resolve statically** (JVMS §4.4.1) instead of
  from the receiver's header, in the interpreter and the JIT;
* **a dispatch terminal that reports an impossible receiver**, in
  `memory::reclaim_guard`.

Both are described below and both stay.

---

## How it presented, and why it cost two sessions

```text
Caused by: java/lang/CloneNotSupportedException
	at org/h2/mvstore/tx/TransactionStore.registerTransaction(TransactionStore.java:499)
	at org/h2/mvstore/tx/VersionedBitSet.<init>(VersionedBitSet.java:25)
	at org/h2/mvstore/tx/BitSetHelper.flip(BitSetHelper.java:34)
	at java/util/Arrays.copyOf(Arrays.java:3617)
	at java/lang/Thread.clone(Thread.java:1037)          <- innermost
```

Every frame is genuine and every line number is exact (see the appendix). The
2026-07-31 analysis read it as an array receiver dispatched through its
component class id and fixed that — a real defect, still fixed, but **it cannot
produce this trace**: `class_id_of(long[])` is `ClassId(0)`, which resolves to
`java.lang.Object`, so component-class-id routing can only ever select
`Object.clone()` for a `long[]`. It has no path to `Thread.clone()`.

That page's own verification is the tell in hindsight: **7/7 clean runs on the
pre-fix binary** with a diagnostic armed for exactly the array-dispatch shape.
Read at the time as "the H2 symptom is rare"; it actually meant "the H2 symptom
is not this mechanism". The page then reopened on 2026-08-07 when the fixed
binary reproduced it.

The `java.lang.Thread` frame was the strongest clue and looked like the
weakest. Nothing in `Arrays.copyOf` selects for threads, so a "reclaimed block
re-served to a random object" story has to explain why a *Thread* twice — and
it cannot. It was a Thread every time because the substituted object is a
**thread mirror by construction**.

## The witnesses

`org.h2.test.db.TestTempTables`, `--java-home <jdk25> --Xmx 1g --nojit`, one
class per process, 6-8 concurrent workers. **5 occurrences in ~50 `--nojit`
runs**, 0 in 30 JIT-on runs of the same class (the JIT reaches dispatch through
`invoke_or_native`, not through `execute_invoke_kind`).

| receiver address | substituted `receiver_class` |
|---|---|
| `0x20010039668` | `jdk/internal/misc/InnocuousThread` |
| `0x200100e37f8` | `org/h2/mvstore/FileStore$BackgroundWriterThread` |
| `0x200100e1dd0` | `java/lang/Thread` |
| `0x2001003bb50` | `java/lang/Thread` |
| `0x20010117b00` | `org/h2/mvstore/FileStore$BackgroundWriterThread` |

All five are `java.lang.Thread` or a subclass — `Thread.clone()` is inherited,
so any of them produces a `java/lang/Thread.clone` frame. All five are live,
old-generation objects with a valid header; none is an all-zero wipe.

The decisive line, from the terminal added here:

```text
site="array-typed call site, non-array receiver"
target_method=[J.clone()Ljava/lang/Object;
receiver_class=org/h2/mvstore/FileStore$BackgroundWriterThread
receiver_kind=Object   in_young=false   object_degradations=0

…the receiver as the OPERAND STACK handed it over, before `load_and_forward`:
pre_refresh_obj="0x20054759670"   barrier_rewrote=true
barrier_src="0x0"  barrier_mark_at_read="0x0"  barrier_dst="0x0"
pre_refresh_kind=Array   pre_refresh_class=java/lang/Object

…where 0x20054759670 stood in the owning thread's bookkeeping:
holder=frame#14 org/h2/mvstore/tx/BitSetHelper.flip pc=38 local[0] kind=0 live=true
```

Read it in order:

* `pre_refresh_kind=Array`, `pre_refresh_class=java/lang/Object` — the value the
  operand stack handed over **is a primitive array**, intact;
* `holder=… local[0] live=true` — and the caller's frame still holds it. Nothing
  was lost, moved or reclaimed;
* `barrier_src="0x0"` — the forwarding read barrier
  (`load_and_forward`) recorded no rewrite, so it is not the barrier;
* yet the receiver that reached dispatch is a `BackgroundWriterThread`.

The only writer of `args[0]` between those two points is the mirror recovery.
An earlier witness, before `barrier_src` existed, had pointed at
`load_and_forward` on the strength of `barrier_rewrote=true` alone; recording
the barrier's *own* decision is what moved the blame off it. That is worth
keeping as a method note: `pre != post` says the value changed, not who changed
it.

## What else landed, and why it stays

### 1. An array-typed call site resolves against `java.lang.Object`, statically

Every guard in the tree asks the *receiver's header* whether it is an array.
That is correct only while the header is trustworthy. When the CONSTANT POOL
names an array type, JVMS §4.4.1 settles the target with no header at all: an
array class declares no methods, its method table is `Object`'s, and there is no
subclass of `[J` that could override `clone()`. Four sites now decide from the
call site:

* `vm/src/runtime/interpreter/invoke.rs` — `execute_invoke_kind`'s
  `invoke_class` selection;
* `vm/src/runtime/interpreter/dispatch_virtual.rs` —
  `execute_invokevirtual_vtable_fast` cedes an array-typed site to the slow
  dispatcher instead of resolving it against the receiver's vtable;
* `vm/src/jit/helpers.rs` — `virtual_dispatch_target_for_receiver` and
  `virtual_dispatch_target_cached`.

This is what made the bug legible. With it in place the same event stopped
running `Thread.clone`'s body and reported

```text
java.lang.ClassCastException: jdk.internal.misc.InnocuousThread cannot be cast to [J
	at java/util/Arrays.copyOf(Arrays.java:3617)
```

— the object, named, at the `checkcast [J` that follows the call site. It is
also correct independently of this bug, and it costs nothing on the healthy
path: an array-typed site never populated an inline cache in the first place.

### 2. A terminal that reports an impossible receiver

`vm/src/memory/reclaim_guard.rs`'s `report_impossible_dispatch_terminal`, reached
from three places unreachable in well-formed code:

* `java.lang.Thread.clone` and `java.lang.Enum.clone` dispatch — bodies that are
  `throw new CloneNotSupportedException()` and nothing else;
* **an array-typed call site with a non-array receiver** — a provable verifier
  violation, and the one that fires *before* the target is chosen, so it does
  not depend on which wrong body a bad receiver happens to select. This is the
  one that caught all five witnesses.

It reports, in one place: the receiver's full header (sized off `HEADER_SIZE`,
so it survives the next header shrink), which generation it is in, the
pre-barrier receiver and whether the barrier rewrote it *and what mark word the
barrier read*, root-snapshot provenance for both addresses, the interpreter
frames, and `object_degradation_count()`.

Three instrument gaps closed with it, each of which had cost a run:

* `report_reclaimed_receiver` asked the free-list question only for
  `actual_cid == 0` (it has callers that fire in bulk on healthy runs), which is
  blind to precisely the re-served face. The clone terminals use a `_forced`
  variant;
* the `Thread.clone` reporter lived only in `execute_invoke_kind`, a path a
  JIT-compiled call site never takes. It now also sits in
  `try_stackless_invoke`, covering `Thread` as well as `Enum`;
* `object_degradation_count()` — the counter the `ClassId(0)` page's closing
  instruction tells the next reader to check — had **no consumer anywhere in the
  VM**. It is now on the verdict line.

## Reproduction

* `apps/h2database-suite-runner` fixture, `org.h2.test.db.TestTempTables`,
  `--java-home <jdk25> --Xmx 1g --nojit -c <h2 testcp>`, own scratch CWD.
  ~5 events in 50 runs; a passing run is 500-1300 s on a loaded host, a failing
  one aborts at 90-850 s. No flag needed — the verdict is unconditional.
* `docs/internal/repros/h2-clone-spin-20260807/CloneSpinProbe.java` — the
  negative control. It drives the identical `BitSetHelper.flip` →
  `Arrays.copyOf` → `original.clone()` shape with the database removed:
  **28.4 M executions of the failing bytecode in 90 s, zero failures.** The call
  shape was never the variable, which is the measurement that redirected the
  hunt from the dispatch code to the receiver.

## Verification

* `runtime::interpreter::tests::stale_mirror_recovery_skips_a_live_primitive_array`
  — differential: FAILS with the old `class_id == 0` gate restored, passes with
  the fix.
* `vm/tests/array_receiver_dispatch.rs` —
  `array_receiver_dispatches_through_object_not_component_class` passes.
* `cargo test --release -p cratonvm-vm --no-fail-fast`: **4063 passed / 11 failed
  on BOTH arms** — this tree and a contemporaneous pristine rebuild in the same
  worktree. Ten failures identical on both:
  `connection_methods_carry_signatures`,
  `memory::addr_keyed::tests::the_address_keyed_table_census_is_complete` (its
  own message names `native-builtins/src/net_phase_e.rs`, untouched here),
  `no_new_test_only_public_api`, `probe0_jboss_module_class_reachable`,
  `t14_all_system_natives_registered`, `t14_all_vm_natives_registered`,
  `t15_define_class_not_stub`,
  `test_jit_exception_in_handler_not_recaught_by_same_handler`,
  `test_jit_indy_after_side_effect_no_double_execution`,
  `test_precise_handler_frame_catches_a_throw_at_the_end_of_its_try`. The
  eleventh differs in **opposite directions** —
  `socket_input_stream_timeout_is_typed_and_never_eof` on the fixed arm,
  `test_pgo02_guarded_virtual_inline` on the pristine one — and both pass in
  isolation, so both are the known process-global flakes.
* `CloneSpinProbe` on the fixed binary: 9.2 M `[J.clone()` dispatches, 6
  threads, `--Xmx 512m`, JIT on — `failed=0`.
* `H2InsertScaleProbe 25 1000` — this page's own "Possible residual observed
  2026-07-31", recorded there as failing **all 25 threads**: 16 runs on current
  `dev`, `failed=0` every time, plus one run at `25 3000`, `failed=0`. That
  residual is withdrawn; the mirror substitution is the only mechanism this page
  ever had.

## Known residual of the fix, stated

The all-zero-header test is exact for a primitive array and for any object with
a minted identity hash. It is **not** exact for a bare `new Object()` on the
current 16-byte header: since the 2026-08-07 header shrink folded the identity
hash into the mark word, a freshly allocated zero-field `java.lang.Object`
whose hash has not been minted has an all-zero header too. So a `new Object()`
receiver whose address is in `former_mirror_addrs` could still be substituted.

That is a far narrower window than "every primitive array" (it needs a
zero-field `Object` receiver of a non-special invoke, on an address a thread
mirror previously vacated), and closing it properly means asking the heap
whether the address is still on a free list — `GenerationalHeap::reclaimed_hole_at`,
which is the unambiguous discriminator but is generational-only and would
silently disable the recovery on G1/ZGC. Left as-is deliberately rather than
traded for a backend-dependent behaviour change; recorded here so the next
reader does not have to re-derive it.

## Related

* `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-classid0-stale-address-family-FIXED.md`
  — the family this was twice mistaken for, with a note on how the two were told
  apart.
* `vm/src/jit/helpers.rs` — the "KC26 `array.clone()` bug" comments record the
  JIT-side instance of the component-class-id family (`Enum.clone() →
  CloneNotSupportedException` for enum-array clones, and the `ResolvableType[]`
  / `ResolvableType` Spring Boot `ClassCastException` behind the machine-code
  `OBJECT_KIND_OFFSET` guard).
* `vm/src/runtime/interpreter/invoke.rs`'s `try_stackless_invoke` "T15" comment
  (array class names rewritten to `java/lang/Object`).
* `fixed-suite-bugs/gc-blocked-thread-frame-stale-thread-mirror-RESOLVED.md` —
  the Tomcat `TestDigestAuthenticator` case the mirror recovery was written for.
  It dispatches on a genuinely *zeroed* object, which is exactly what the
  tightened gate still admits.

## The dispatch defect the title names (real, fixed 2026-07-31, retained)

`Anewarray` stores the **component** class id in the array's object header and
`Newarray` stores `ClassId::new(0)`, so

```text
class_id_of(Foo[])   == class_id_of(Foo)
class_id_of(long[])  == ClassId(0) == class id of java/lang/Object
```

Per JVMS §4.4.1 an array type's method table comes from `java.lang.Object`, so a
dispatch path keyed on `heap.class_id_of(receiver)` must not select the
component class's body — `someArray.m()` would otherwise run `Foo`'s body with
the array as `this`, reading array *element* 0 as *field* 0.

2026-07-31 added `kind_of(receiver) == Array → CacheMiss` to the three
receiver-validating arms of `execute_invokevirtual_cached`, made
`invoke_or_native`'s NoSuchMethod receiver-class-chain rescue skip array
receivers, and made `invoke_on_class_shared_inner`'s virtual retarget yield
`None` for them. Differential-verified then and still covered by
`vm/tests/array_receiver_dispatch.rs`:

| | `Foo[].toString()` | `Foo[].hashCode()` |
|---|---|---|
| HotSpot jdk-25 | `[LFoo;@7ad041f3` | identity hash |
| CratonVM (before) | `Foo-toString-v0` | `0x5eed0000` (Foo's override) |
| CratonVM (after) | `[LFoo;@52` | identity hash |

All of that is correct and stays. It simply was not this page's H2 failure.

## Appendix — the 2026-07-31 frame analysis, preserved

The original report claimed the two innermost frames "don't form a plausible
real call chain" and were therefore evidence of a stack-trace-construction bug.
That refutation still stands:

* `java.base/java/lang/Thread.java:1037` is
  `throw new CloneNotSupportedException();`, the whole body of
  `protected Object clone()`; `javap -c` confirms
  `new / dup / invokespecial / athrow`.
* `java.base/java/util/Arrays.java:3617` is `return original.clone();` inside
  `copyOf(long[] original, int newLength)`, compiled to
  `aload_0 / invokevirtual #319 "[J".clone:()Ljava/lang/Object; / checkcast "[J"`.
* `org/h2/mvstore/tx/VersionedBitSet.java:25` is
  `bits = BitSetHelper.flip(other.bits, bitToFlip);` and `BitSetHelper.java:34`
  is `bits = Arrays.copyOf(bits, Math.max(length, wordIndex) + 1);` — both
  checked against the fixture's own sources.
* CratonVM's exception printer emits frames **outermost-first**, so
  `java/lang/Thread.clone` is the innermost frame.
* No Rust code constructs `CloneNotSupportedException`, so it came from real
  bytecode.

What the analysis got wrong was the next step: it concluded the receiver was the
array, dispatched through a component class id that named `Thread`. The receiver
was never the array — the VM had replaced it with a thread mirror.
