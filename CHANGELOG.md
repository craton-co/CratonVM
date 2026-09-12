# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### 2026-09-12 `CopyOnWriteArraySet` was backed by a LinkedHashMap, and `removeIf` was the only method that said so

`register_hashset_natives` mirrors the whole `java.util.HashSet` surface onto
`CopyOnWriteArraySet`, on the stated premise that it is one of "HashSet's
real-JDK subclasses that share the same `field 0 = backing map` layout". It is
neither: it does not extend `HashSet`, and the ONE instance field the real class
declares is `private final CopyOnWriteArrayList<E> al` — at exactly that slot 0.
So `native_hs_init` stored a `LinkedHashMap` in a slot declared to hold a list.

Nineteen methods, twenty registered triples, and `removeIf` in neither set — so
its real body ran:

```text
  cowSet.removeIf(p)
    -> NoSuchMethodError: java.util.LinkedHashMap.removeIf(java.util.function.Predicate)
```

Every other method looked correct because a native stood in front of it. Which
ones are silent is a function of which triples the registrar happens to carry,
which is why the guard below is the whole declared surface rather than the one
method that broke.

It cost the class its size as well. Retained heap against HotSpot on the same
probe:

```text
  empty          112.0 -> 72.0     HotSpot 56.1     (2.00x -> 1.28x)
  four entries   512.0 -> 120.0    HotSpot 88.3     (5.80x -> 1.36x)
```

The four-entry row is the larger half and had never been measured:
`probes/CollectionShapeCause.java`'s filled table had no `CopyOnWriteArraySet`
row, so a `LinkedHashMap` holding four entries — 488 bytes where HotSpot holds a
six-element `Object[]` — was invisible. It has a row now.

`cow_set_route` takes a real receiver to its own bytecode, method by method,
from the top of each of the twenty natives — the placement `ksv_route` already
uses, so the interface-level registrations (`java/util/Set.size()` and friends)
are guarded by the same test as the exact-class ones. "Real" is a by-NAME
question, not a mode flag: `is_real_cow_array_set` asks whether the receiver's
class resolves `al`, so a fabricated stub keeps the map surface, where that
surface IS the implementation. Delegation rather than a second implementation
because the object underneath is already right — `CopyOnWriteArrayList` writes
`lock` and `array` by name, runs the JDK's own constructor, and measures 48.0
against HotSpot's 40.0. The one method that cannot go that way is `stream()`, a
`Collection` default whose body builds a real `java.util.stream` pipeline; it
takes the elements through the delegated `toArray()` and builds this VM's
carrier, as every other `*_stream` native does.

`probes/CowSetBacking.java` is the coverage that made the move safe — every
method `javap -p` lists plus the four it inherits, asserting insertion ORDER
throughout, because that is the observable separating a list-backed set from a
hash-backed one. `vm/tests/cow_array_set_backing.rs` drives it as a gate and was
mutation-checked: it FAILS on the pre-fix binary with exactly the
`NoSuchMethodError` its message describes.

Verified on both arms from the same commit: real-JDK `PASS CowSetBacking` and
`PASS CollectionSlotFloor`, regression-suite 95/95, tier1 58/58;
synthetic-JDK verdict-identical to an unchanged tree on both probes (the same 15
and 14 outcomes, the same 127 `field index OOB`), which is the acceptance
criterion rather than green.

### 2026-09-12 The synthetic slot floor was ONE number for TWO layouts, and six collection classes paid for it

`synthetic_stub_fields` is read in two places that mean different things. It
DEFINES the layout of a fabricated stub, and it FLOORS the layout of a class
defined from real class-file bytes. The second reading is load-bearing for
`java.net.InetSocketAddress` — one declared field, and a native `<init>` that
writes raw synthetic indices on the real class. It was a fiction for six
collection classes, and padding them by even one slot costs the WHOLE object:
`ClassStore::build_compact_layout` refuses any padded class, because a padded
slot has no descriptor and its oop-map entry would be a guess, so every slot
falls back to the legacy uniform 16-byte tagged cell.

Retained heap per empty instance, against HotSpot on the same probe:

```text
  java.util.Properties                          544 -> 224   (4.5x -> 1.9x)
  java.util.concurrent.ConcurrentLinkedQueue    112 ->  64   (2.3x -> 1.3x)
  java.util.concurrent.ConcurrentLinkedDeque    120 ->  72   (2.5x -> 1.5x)
  java.util.ArrayDeque                          232 -> 184   (2.1x -> 1.6x)
  java.util.LinkedHashSet                       152 -> 112   (1.9x -> 1.4x)
  java.util.HashSet                             128 ->  88   (2.0x -> 1.4x)
  java.util.concurrent.CopyOnWriteArraySet      136 -> 112   (2.4x -> 2.0x)
```

Six of the seven land in the 1.2x-1.7x band the rest of the collections occupy,
which is reference width and a separate subject. `CopyOnWriteArraySet` does not,
and its object IS compact now — the remainder is that this VM's Set surface
backs it with a `LinkedHashMap` (88 B) where the JDK backs it with a
`CopyOnWriteArrayList` (48 B). That is a different change.

`ClassManager::apply_synthetic_floor` is now the one place the floor is applied;
both callers used to open-code the same `max`. `FLOOR_EXEMPT_CLASSES` beside it
carries the six with the real extent each was screened against, and a class
whose loaded shape disagrees with that number is reported rather than silently
exempted. `java.util.ArrayDeque` is NOT in that list: it needed a correction,
not an exemption — its fourth slot held a count `ad_state` stopped reading on
2026-08-30, so the table now declares the three the real class declares.

**Six factories had to be converted first, and each was a live defect on its
own.** They built an array-backed set by writing absolute slots 0/1/2 on a real
`HashSet` receiver — the MAP layout, on a class whose one real field is
`map` — so every real `Set` method dereferenced an `Object[]` and answered for
an EMPTY set: `Selector.selectedKeys()` and `.keys()` (twice, in `net_channels`
and in `servlet`), `ModuleLayer.modules()` (the twin of the registrar whose
identical shape NPE'd Tomcat's web-fragment scan),
`ZoneId.getAvailableZoneIds()`, and the JMX `queryNames`/`queryMBeans`
fallback. All six now go through one helper that allocates the real width and
runs the class's own `<init>` and `add`, which is correct in both modes.

**A screen, so the population is a list rather than an argument.**
`t9d_floor_exempt_classes_have_no_oversized_factories` is T9C run the other way:
T9C asserts a fabricated table is at least as wide as its own factories, T9D
asserts a floor-EXEMPT class has no factory wider than its real layout. A site
that has already asked `is_class_synthetic_stub` is excused, because it knows
its receiver is fabricated. It also refuses a stale exemption — one that no
longer pads anything is a claim about a class, not a live exemption.

**`probes/CollectionSlotFloor.java` had been hiding its own tail.** In the
synthetic-JDK arm — the arm that matters when a floor moves — a missing
`LinkedList.indexOf` threw at section six of fourteen, so the eight sections
after it were never reached and their silence read as agreement. Each section
now runs under a wrapper that records an ERROR row instead of ending the run,
and the file grew the coverage these six classes needed: deque and FIFO ORDER
(which `size`/`contains` cannot see), `ArrayDeque` past its ring-buffer wrap,
and the `Properties` `defaults` chain.

Validated on both arms against binaries built from the same commit: real-JDK
`CollectionSlotFloor` PASS with the extended sections and an identical
descriptor-coercion census; synthetic-JDK **verdict-identical** to an unchanged
tree (the same 14 outcomes, the same 127 `field index OOB` warnings), which is
the acceptance criterion rather than green. `regression-suite` 93/93, tier1
58/58, `cratonvm-native-builtins` 4253/0, `cratonvm-classloading` +
`cratonvm-native-collections` 1179/0.

### 2026-09-11 The receiver species is mostly ARRAYS: 20 more fixed, and a screen so the population cannot grow quietly

The BindableTests residual fixed earlier today was one native holding a
receiver across a `<clinit>`. Screening the same shape across every native
crate says it is neither rare nor mostly about `set_field`: the commonest form
is building a Java array and filling it, which holds the ARRAY's address across
every element's allocation.

Twenty of those are fixed, each by rooting the reference in a
`NativeHandleScope` and reading it back after the allocation —
`Throwable.getStackTrace()` and `Thread.getStackTrace()` (array, element,
three strings and a class mirror per frame), `fill_stack_trace_element`,
`System.getenv()`, `Properties.setProperty`'s growth path, `String.lines()`,
`ConcurrentSkipListMap.put` (its `compareTo` runs Java on every probe of the
search loop), the JSON tree builder, both StAX readers,
`Locale.getAvailableLocales`, `InetAddress.getAllByName`, `Module.getModules`,
`ChoiceFormat`, `BigInteger(int, byte[])`, `ClassLoader.getResources`,
`PriorityBlockingQueue`, JNDI `list`, the charset map, `ServiceName`,
`MBeanServer.unregisterMBean` and `XnioWorker.getIoThreads`.

Two screens close behind them:

* `[deadref-recv]` now covers `set_array_element`, `get_array_element` and
  `get_field`, not just `set_field` — the array store is where this species
  lives, and a READ through a vacated receiver silently answers whatever the
  pre-move copy held.
* `scripts/stale-handle-across-alloc-audit.py` is the static half, a sibling of
  `stale-receiver-audit.py` (which screens a callee shape and structurally
  cannot see a body that reuses its own local). Baseline: 283 sites in 202
  functions — a ratchet, not a target, since a match is not a defect. It scans
  each closure of a `register_*` function as its own body (without that, 290 of
  an apparent 573 sites were an allocation in one closure paired with a use in
  another), and it ships with a selftest that fails if a hazard token stops
  matching, because a dead token looks exactly like a clean tree.

`docs/internal/audits/natives-stale-handle-across-allocation-20260911.md` has
the site table, the triage rules and the honest limits.


### 2026-09-11 The BindableTests residual was a stale RECEIVER, and the whole probe family only ever screened values

`docs/known-issues/springboot/bindabletests-assertj-objects-field-null-under-gc-stress-20260909.md`
is retired into
`docs/internal/springboot/bindabletests-assertj-objects-receiver-stale-across-clinit-20260911.md`.

`Assertions.assertThat(Comparable)` and `assertThat(String)` are CratonVM
natives: `native_assertj_lightweight_comparable_assert` builds the assertion
object field by field instead of running `AbstractAssert.<init>`. The store into
`objects` reused an `assertion` handle read BEFORE the call that runs
`org/assertj/core/internal/Objects.<clinit>`, so on the first comparable
assertion in a process — the only call where that `<clinit>` is still pending —
the `<clinit>`'s allocations moved the assertion and the store landed in a copy
nothing would read again. The surviving object kept `objects == null`, and
`BindableTests`'
`whenTypeCouldUseJavaBeanOrValueObjectJavaBeanBindingCanBeSpecified` failed an
AssertJ `NullPointerException` at every `CRATONVM_DBG_GC_STRESS <= 262144`.
Every store in that native now re-reads the pin; three more `ObjectRef`s carried
across an allocation in the same file are fixed with it.

Why it survived the sweep that fixed ten siblings on 2026-09-09, in two layers.
Every arm of `CRATONVM_DBG_DEADREF_STORE` screens the VALUE being stored, and
the value here (`Objects.INSTANCE`, in old gen) was live throughout —
**`[deadref-recv]`** is the new receiver-side arm of the same switch, pinned by
a unit test. And this store never reached the heap at all:
`NativeContext::set_field_by_name` resolves the field against the class of
whatever is AT the address it is handed and **drops the store silently** when it
does not resolve, one level above `set_field`, where no heap probe can see it.
**`[field-by-name-dropped]`** is that arm — counted always, with a non-zero
total printed at exit, and named per distinct `(class, field)` under the same
switch. On the unfixed binary it prints
`recv_class=java/lang/Object recv_class_id=0 field="objects"` with
`native_assertj_lightweight_comparable_assert` in the backtrace. The dedup is
load-bearing: the same run drops 2 710 stores, ~2 700 of them one benign shape.

Three instrument gaps closed alongside it. The compact reference store was the
one heap write primitive with no `cell_watch_check`, so
`CRATONVM_DBG_WATCH_CELL` answered "nobody wrote it" for a compact object's
reference field. `[GETFIELD-WATCH]`/`[PUTFIELD-WATCH]` printed the receiver's
address and nothing else, which cannot tell "the same object, moved" from "a
different object at a recycled address"; they now print its class, `num_slots`,
`gc_flags`, whether the heap still calls it an object start, and the field's
layout-aware byte address. And **`CRATONVM_DBG_OBJ_WATCH=<class-substring>`** is
new: it follows an OBJECT rather than an address — one `[OBJWATCH]` line per
evacuation with the source body words, plus one per `set_field` with the Rust
caller — which is what an address watch cannot do when a semispace is re-served
from the same base every cycle.

`BindableTests` 27/27 at 65 536, 131 072, 262 144, 262 144 `--nojit`, 393 216,
524 288 and unset; every `[deadref-*]`, `[tlab-audit]`, `[heap-stale]` and
`[RESID-DIAG]` counter zero and 42 589 `[rset-verify]` reports with `missing=0`
on the 262 144 run.


### 2026-09-11 The loop control: a folded immediate, and `LEA` for the increment

The residue `c2-a-fused-compare-can-read-its-operands-where-they-are-20260910.md`
left behind, finished and the page retired.

A fused compare now has four forms instead of two. It already read two resident
operands in place, and a resident one against a frame slot; it now also folds a
CONSTANT second operand into the instruction, with the first operand read from
either its register or its frame slot. `i < 100` is the shape of most Java
loops, and it used to cost `mov rax,rbx ; mov ecx,64h ; cmp eax,ecx` where
`cmp ebx,64h` does. `CRATONVM_JIT_IR_CMP_IN_PLACE=0` remains the kill switch for
all four.

`x + k` and `x - k` lower to one `LEA` where `k` is a constant and `x` is
resident — `lea eax,[rbx+1]` for `mov rax,rbx ; add eax,1`, or
`lea r14d,[rbx+1]` for the three-instruction form when the result has a register
of its own. New flag `CRATONVM_JIT_IR_ADD_LEA=0`.

Instructions AND bytes fall wherever either fires — 189/933 to 186/922 on
`LoopCtl.spin`, 200/1190 to 196/1178 on `PollReach.hotLoop`, and the two levers
are additive to the instruction. **No speedup is claimed**: the build host's
noise floor between two identical binaries reached 14.6% during the A/B, so the
timing is a null. See
`docs/internal/retired/c2-a-fused-compare-can-read-its-operands-where-they-are-RETIRED-20260911.md`
§8 for why, in numbers.

Four `CRATONVM_*` names that were read by code but declared nowhere —
`CRATONVM_JIT_IR_ADD_LEA`, `CRATONVM_JIT_IR_CMP_IN_PLACE`,
`CRATONVM_JIT_IR_CARRY_2ND` and `CRATONVM_JIT_IR_PAIR_OPERANDS` — now have rows
in `flag_groups.rs::INVENTORY` and the generated flag docs, so they are served
from the latched `VmFlags` snapshot rather than a live `getenv`.

New probes `probes/CmpImm.java` (timing and census) and `probes/CmpImmProbe.java`
(differential against HotSpot: the `imm8`/`imm32` boundary in both signs, a
`long` constant outside `i32`, `Integer.MIN_VALUE` as a bound and as an addend,
a spilled first operand, and a reference against `null`).


### 2026-09-10 The safepoint poll's flag byte, from the code cache's own allocator

The second half of the placement problem `CRATONVM_JIT_CODE_NEAR_GLOBALS`
opened. That strategy moves the CODE to the globals: it hints `mmap` to place
each buffer within 1.5 GB of `layout_replace_epoch_guard()`, and the safepoint
flag comes along because it is a few hundred megabytes away in the same
mimalloc band. It is **default OFF**, so on a default Linux run the code buffer
is still ~130 TB from the flag and every back-edge poll and method-entry poll
in the process still emits `MOV R11, imm64 ; TEST BYTE [R11], 0FFh` — 15 bytes
and a clobbered register — instead of the 7-byte `TEST BYTE [rip+disp32], 0FFh`
the 2026-09-02 work added. Nothing fails; the fallback reads the same byte and
branches the same way, which is why it went unnoticed.

For that default configuration the flag now comes from
`platform::alloc_code_adjacent_cell` — a bump allocator over 64 KiB chunks
taken from the same `mmap(NULL, …)` / `VirtualAlloc(NULL, …)` that
`alloc_executable` hands the code cache, carving 64-byte cache-line-isolated
cells that are never unmapped. `CacheLineFlag` becomes a `&'static AtomicBool`
into one; its four methods are unchanged, so all 75 `stw_requested` call sites
are untouched.

**The two strategies now compose, where `82bf52efd` correctly said they could
not.** `alloc_code_adjacent_cell` returns `None` when
`CRATONVM_JIT_CODE_NEAR_GLOBALS` is engaged, and `CacheLineFlag` falls back to
the leaked `Box` — so with that flag on, the flag stays in the allocator band
its anchor lives in and behaviour is bit-identical to before this change. The
same reasoning that made `alloc_epoch_page` wrong for the epoch counter makes
declining the cell right here: whoever owns the placement must own it for every
cell at once, and `near_globals` owns it whenever it is on.

Placement is a hint either way — the OS picks — so both emitters keep their
per-site ±2 GB test and their fallback. Two tests assert the reach, one of them
through `stw_requested_flag_addr` itself, and both skip when `near_globals` is
engaged.

`execute_frame` hoists the flag REFERENCE once, beside the existing
`async_exception_slot` hoist. This is not the hoist the loop-top comment
refuses: that one caches the flag's VALUE at frame entry and would cut poll
frequency, which is time-to-safepoint. Every poll still loads the byte; what is
resolved once is the address, which is now a pointer indirection and one whose
source word shares a line with `gc_generation`, `threads_blocked` and the
barrier mutex.

Measured Windows x86-64, before and after, same tree: 489 MiB apart before,
**128 KiB** after, short form on both — Windows already lands in reach, which is
why `near_globals` does not build there either. **The Linux confirmation is
still owed and is the one that matters.** See
`docs/internal/performance/safepoint-poll-flag-was-on-the-rust-heap-FIXED-20260910.md`.

Found while verifying it: `CRATONVM_JIT_RIP_SAFEPOINT_POLL=0`, the lever for
pricing the two encodings inside one binary, reached only the single-pass
backend. `ir_lower.rs::emit_safepoint_poll` — the optimizing tier, where
everything hot is compiled — called its RIP emitter unconditionally, so on a
real workload the switch moved **2 of 394** poll sites. Both gates in
`x64/licm.rs` are now `pub(crate)` and the lowerer calls them; the switch moves
398 of 398, and both arms print the same answer.

### 2026-09-09 `checkcast` / `instanceof` in a spliced callee — the third rebase, and the bug it uncovered

The third instance of one pattern, after `ldc` and `getstatic`: `IrBuilder` has
had `0xc0`/`0xc1` arms since cov-05, and the splice scanner refused the shape
for both tiers in one arm — so the optimizing tier inherited a refusal that
belongs to the single-pass emitter, which genuinely has no arm for either. It
fell on the commonest accessor in typed Java: the survey that motivated those
arms counted 306 events on this pair, the largest single whole-method refusal it
found, more than every opcode gap combined.

Unlike `getstatic`, there were no rows to rebase — `InlineSite` gains
`ir_typecheck_info` and the resolver fills it against the CALLEE's constant
pool, the only pool that can name the target. An unresolved target refuses the
callee, because a missing row bails the whole method. A spliced `checkcast`
also carries the `has_dispatch` obligation its caller-side twin does: a
definitive refusal publishes its `ClassCastException` through the `JIT_THREAD`
TLS the no-dispatch fast entry never sets.

Reach: spliced bodies 2 → 6 on `bench/SpliceCastProbe.java`. Throughput: the
body it produces is ~30% faster (~133 ms against ~193 ms on
`bench/SpliceCastArrayProbe.java`, 34 interleaved rounds, a mode the off arm
never reaches), and the run median is NEUTRAL because that body is installed in
about a quarter of runs — the tier race, not this lane.

Found in passing and NOT fixed: on `SpliceCastProbe` the optimizing body is ~3x
slower than the single-pass one in BOTH arms, and `ir blind dispatches:
own_code=0 in_splice=1` names the suspect — a surviving `invokevirtual`
(`ArrayList.elementData`) inside a relocated body that got neither a direct bind
(correctly — it is virtual) nor the MIC/PIC cascade it should have. Written up
as the next thing to look at. `CRATONVM_JIT_IR_SPLICE_TYPECHECK=0` restores the
refusal.

### 2026-09-09 A `getstatic` in a callee cost the optimizing tier the inline, and the calls a splice left behind were name resolutions

The C2 tier published a body **15x slower than the C1 body it replaced** on the
callee shape framework code is mostly made of — a hot method whose accessors
read statics — and the default acceptance gate only avoided it by abandoning
the supersede for an unrelated reason on most runs.

Two causes, both plumbing. A callee containing `getstatic` was refused for
splicing because nothing rebased its already-resolved rows into
`IrInlineTables`; and a statically-bound call that SURVIVED a splice got no
`ir_direct_calls` row, so it fell through to `jit_invoke_dispatch` and resolved
its callee by name on every execution. The second is the expensive one and it
is worse than not splicing at all: the resolver had bound the callee entry and
registered it on the artifact's keep-alive list, so the compile paid to pin a
target for a direct call it never emitted.

Measured per BODY, because whether the optimizing body is installed before a
timed loop starts is a race and a run median mixes the two: on
`bench/SpliceStaticProbe.java` the optimizing body goes **~830 ms → ~23 ms**
(from 15x worse than the single-pass body to 2.4x better), and on
`bench/SpliceCallProbe.java` **~455 ms → ~57 ms** (from 8.5x worse to parity).
Single-pass is 56 ms in every arm, every sample checksum-matched to Temurin
JDK 25. All seven `CratonBench` phases are inside 1% with matching checksums;
92 of 92 fast-regression vectors match HotSpot. `CRATONVM_JIT_IR_SPLICE_GETSTATIC=0`
and `CRATONVM_JIT_IR_SPLICE_DIRECT_CALL=0` restore the old behaviour arm for
arm. `putstatic` stays refused — the builder has no arm for it, and a static
reference write owes an SATB pre-barrier the single-pass path carries.
`[c2-supersede] ir blind dispatches: own_code=N in_splice=M` gives the failure a
reading: a non-zero `in_splice` says that method's optimizing body is very
likely slower than its single-pass one.

### 2026-09-08 `new Object()` published sixteen zero bytes, so `System.gc()` kept every one of them

`java/lang/Object` is `ClassId(0)`, a field-less object's `shape` is `0`, and
`MARK_NEUTRAL` / `ObjectKind::Object` / `ArrayElementType::Reference` all encode
as `0` — so the commonest object in Java reached the heap as sixteen zero bytes,
byte-for-byte identical to reclaimed, zeroed, unlisted arena space. The young
non-moving sweep, which every `System.gc()` diverts to, cannot parse that: runs
of such objects were either stepped over without being freed or treated as a
walk desync that unwound every reclaim decision since the last anchor. An
allocation-only workload retained ~100% of its garbage under
`-XX:+UseGenerationalGC`, ~2.1 MB a round, monotonic, until the collector
thrashed — `ChurnLoop 40 125000` did not finish inside 300 s.

`GC_FLAG_HEADER` (mark-word bit 59) now says *these bytes are a published object
header*. It is set by `ObjectHeader::new` and by both JIT inline-allocation
emitters, never cleared, and preserved by every mark-word transition. HotSpot has
never had the problem for the same reason it needs no such bit: its unlocked mark
word is `0b01`. `ChurnLoop` is flat and finishes 40 rounds in 1.4 s;
`zero_spans`, `zero_empty_runs` and the sweep's `live_inside` refusals all go to
zero, and `SWEEP_NO_HEADER_FLAG` — new, printed unconditionally in the
young-sweep census — measures the allocator invariant rather than assuming it.

Second, separable defect on the same page: `Runtime.freeMemory()` answered from
the young arena's raw bump cursor, which the in-place sweep never retreats, so
the reported heap filled once and never emptied. `heap_allocated_bytes` now
answers from `live_bytes_estimate` (`young.used − young.free_list + old.used`),
which is what its own doc always described.

`org.h2.test.unit.TestValueMemory` under `-XX:+UseGenerationalGC` goes from FAIL
at Type 0 (`Used memory: 7018`, 7.2x a 3x threshold) to PASS on all 40 types with
a worst row of 2.30x. The remaining distance to HotSpot's 0.5x is measured and
attributed: it is conservative JIT-frame root retention, and `--nojit` reads
976-977 on every arm. That also turned up a failure nobody had run for — the same class
fails under `-XX:+UseG1GC`, identically on the binary before this work, because
G1's conservative roots retain at region granularity; split out as
`docs/known-issues/h2/testvaluememory-fails-under-g1-on-conservative-jit-roots-20260908.md`
rather than folded in here. Full write-up:
[`docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md`](docs/internal/fixed-bugs/h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908.md).

The VM had already met these sixteen bytes three times and written each one down
as a cost rather than a defect, none of them naming the collector: `invoke.rs`
demoted its "Stale pointer detected" WARN to `debug!` for `java/lang/Object`
call sites because "a bare `new Object()` IS all-zero, legitimately";
`h1_tlab_object_header_has_nonzero_hash_at_allocation` was *inverted* from
`assert_ne!` to `assert_eq!` on exactly that array; and `init_object_header`'s
doc claimed an eager identity hash that had not existed since the 24 → 16
shrink. All three are corrected, and the WARN is restored for `Object`
(`ClassLoader` keeps its separately-justified demotion) — measured at zero
"Stale pointer detected" lines across the suite, the H2 corpus and the probes on
all three collectors. The eager hash all three reach for is the wrong repair:
minting one at allocation makes every `synchronized` block lose its thin-lock
CAS and inflate a monitor.

Adding the flag also inverted two "list of every defined flag" screens that had
to grow it in the same commit: `concurrent_mark_object_size`'s `known_flags` —
without which G1's concurrent mark refused every gray entry as a torn header,
took `cleanup`'s retain-everything fail-safe and stopped unloading classes — and
`header_reserved_fields_plausible`, whose `gc_flags` clause became a tautology
and which now screens the mark word's two reserved bits instead.

### 2026-09-02 `String` is `final`, and that is what killed its own intrinsic — 170x on `charAt`

`String.charAt` in a compiled counted loop cost ~400 ns/char while a
byte-identical body elsewhere in the same binary cost 3, and no documented
lever moved it. `string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901`
tested five hypotheses about the METHOD, refuted all five, correctly located
the discriminator as the compile DOOR, and stopped one question short of why
the doors differ.

They differ because `java/lang/String` is `final`.
`invokevirtual_site_final_owner` therefore answers for every
`String.charAt`/`length`/`isEmpty`/`hashCode` site in the tree, and
`try_compile_inner`'s invoke loop rewrites `invoke_kind` 0 -> 1 on that
answer. Correct about dispatch, disastrous about codegen: the instance
call-site intrinsic gate is `invoke_kind == 0 || invoke_kind == 2`, and a
kind-1 site enters the inline/direct-bind ladder first and leaves the loop
through its `continue`. So the site was bound to a real `CALL` into
`charAt -> isLatin1 -> StringLatin1.charAt -> checkIndex ->
Preconditions.checkIndex` and never offered the inline decode — silently, past
all three `string-intrinsic` diagnostics and invisible to the pin's four
counters. The OSR door runs no such rewrite, which is the whole of the 100x.

The rewrite now yields: a site `try_resolve_intrinsic` or
`try_resolve_string_intrinsic` would take stays at kind 0 for the gate to
claim. The JVMS 5.4.6 rule the rewrite exists for is untouched — no intrinsic
matches a private method, so `String.isLatin1`, `coder` and `checkIndex` stay
pinned, and a test asserts it.

`probes/CharAtCostCurve.java`'s `charAt` rows go from **~560 to ~3.3 ns/char**
one binary, one flag (`CRATONVM_JIT_NO_DEVIRT_INTRINSIC_YIELD`) — ~1700x
HotSpot to ~14x. `probes/CharAtDoorProbe.java`, added here, puts five
byte-identical bodies in one class: the affected arm moves 262.70 -> 1.88 and
the four unaffected arms do not move at all.

Two things this also settles. The pin is **not** retirable: with the intrinsic
actually reaching the emitter, `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` is now
30x WORSE (~100 ns/char against ~3.3), where the page had measured it 3-5x
better — both arms of that comparison were pricing a program the pin was no
longer protecting. And the IR expander's design premise is false: measured with
`CRATONVM_DBG_LICM=1`, `loop_has_hard_barrier=true` on every header, so the
`value`/`coder` loads it depends on hoisting never leave the loop.

Counted (`DEVIRT_YIELDED_TO_INTRINSIC`), printed beside the pin census. See
`string-charat-loop-cost-and-the-unsteerable-intrinsic-FIXED-20260902.md`.

### 2026-09-02 The generational young collector's copy phase can run in parallel

`GenerationalHeap`'s moving (Cheney) young cycle copied its survivors on one
thread, and the reason was structural rather than incidental:
`forward_object` takes `&mut Arena` for to-space and `&mut OldGen` for
promotions, so the borrow checker enforced a single copier. The mark closure
and the sweep's span zeroing had already gone parallel, which left `cheney_drain`
as the one serial phase of a moving pause — and on a survivor-heavy cycle it
*is* the pause.

New `gc/src/gen_evac.rs` supplies the pieces that let N workers copy at once:

* **Copy-then-CAS forwarding.** A worker copies speculatively, then claims the
  object with a tagged compare-exchange on the source mark word. The loser
  abandons its copy and adopts the winner's address, so every reference
  converges. This is the protocol `g1::SharedEvac::evacuate` already uses,
  carried over with the lesson its two `DEFECT-2` sites paid for: a CAS loser,
  and an already-forwarded fast-path hit, must still RECORD `old -> new`, or a
  root naming `old` is never remapped and dangles once from-space is reset.
* **Per-worker to-space buffers**, carved out of the arena's un-bumped tail by
  one atomic `fetch_add` (`Arena::parallel_evacuation_region` /
  `commit_parallel_evacuation`). A retired buffer's tail is stamped with the
  TLAB retire path's existing `TLAB_FILLER` / `GAP_FILLER` sentinels, so the
  arena stays walkable object-by-object when the next cycle reads it as
  from-space. Objects at or above an eighth of a buffer bypass it entirely so
  one large object cannot displace a buffer's worth of small ones; what bounds
  the wasted tail is `ParEvac::plan`'s spendable allowance (see below).
* **Per-worker output shards** for the forwarding map, the deferred dirty
  cards and the copy tally, merged by the driver after the completion barrier.
  The copy tally's thread-local carried a comment asserting the copy phase was
  single-threaded; it now says which half of that is still true and the driver
  folds each shard in with `copy_tally_merge`.

Five things were wrong on the first cut and are worth recording, because every
one of them was invisible to the correctness tests — the object graph came out
right each time. The last two were invisible to the unit tests ENTIRELY and
took an end-to-end run to surface:

* **The helpers did nothing.** Measured on a 6144-node DAG, 96 layers deep,
  eight workers: the driver copied all 6144 and the helpers copied zero. The
  drain only published work once a local stack passed 2048 entries, and a
  transitive closure over a graph like that keeps a frontier of about `width`.
  The load-bearing rule is now the other one — publish half the local stack the
  moment any worker is idle, off a relaxed `idle_hint` load — plus a fair
  `len / threads` acquire share instead of letting the first waking worker
  swallow a seed set smaller than the chunk.
* **The threads were spawned per pause.** With the sharing rule fixed the
  helpers still scanned nothing across twelve consecutive collections: the
  driver drained the whole closure in about a millisecond while seven fresh OS
  threads were still on their way to their first lock. Switching to the
  persistent `evac_pool` — which exists for exactly this, and whose module note
  makes the same argument for G1 — took helper participation from 0 to ~85% of
  destinations scanned, and the test that measures it from 3–5 s of retries to
  0.07 s on the first attempt.
* **The buffers were a constant.** Eight workers each holding a fixed 64 KiB
  buffer retired **327 KiB of filler for 393 KiB of survivors**. Sizing the
  buffer against the live set (`from_used / (workers * 4)`, clamped to
  4 KiB…64 KiB) brought that to tens of KiB with no loss of participation.
* **The reservation was worst-cased, and that made the feature unreachable.**
  Only a real workload could show it. `bench/BinT.java` at depth 18,
  `-Xmx512m`, first moving cycle: `to_headroom=134217728` against
  `from_used=134096736` — a young GC triggers with from-space **99.91% full**,
  so the Cheney invariant's `to_headroom >= from_used` left 120,992 bytes and
  nothing more. The budget asked for `from_used/7` (19 MB) of worst-case
  abandoned buffer tails on top, so it declined — and would have declined on
  every cycle of every real workload, with all twenty unit tests green because
  each sizes its to-space generously. The waste is now BUDGETED instead:
  the slack that exists is split half to in-flight buffers and half to a
  spendable `waste_allowance`, and once that is gone `plab_alloc` serves
  objects from exact per-object spans rather than abandoning another tail.
  Region consumption is then provably `<= from_used + allowance +
  workers * plab`, i.e. exactly the headroom. `plan_accepts_the_measured_shape_of_a_real_young_collection`
  freezes those two numbers so the regression cannot come back.
* **And the fix for that was still not enough**, which only a second real run
  showed: with buffers sized from the slack, eight workers need
  `8 * 4 KiB` of it, and the slack at the trigger point lands either side of
  that from one collection to the next — so the copy phase engaged on roughly
  half of bt18's cycles and fell back to serial on the rest, invisibly.
  Buffers are an OPTIMISATION, not a precondition: with `plab_bytes == 0`
  every object takes its own exact span off the shared cursor, consuming
  exactly the survivors, which `to_headroom >= from_used` already guarantees.
  The cycle now runs bufferless rather than serial when the slack is thin, and
  `declined_for_slack` is reserved for the one case the collection's own
  backstop should have caught first. Measured after: `cycles=1`,
  `declined_for_slack=0` on five consecutive bt18 runs, against roughly one in
  two before.

`PAR_EVAC_HELPER_SCANS` exists so the first of those is a number rather than a
wall-clock mystery next time: "it engaged" and "it spread the work" are
different claims, and the first held while the second did not.

The three seed phases (precise roots, overlay-held edges, dirty-card
old→young slots) are now `seed_roots` / `seed_overlay_roots` /
`seed_dirty_card_roots`, called by BOTH evacuators. Seeding is where the two
could have drifted invisibly — a card slot the parallel path forgot surfaces
as a live object reclaimed, a week later, on the other collector.

`CRATONVM_GC_PAR_EVAC=0` forces the serial evacuator. It is default-on because
it cannot engage on its own: the cycle must be a moving one and the existing
`CRATONVM_GC_PAR_THREADS` policy must already want two or more workers.
`gen_evac::par_evac_census()` reports
`(cycles, cas_losses, declined_for_slack, filler_bytes, helper_scans)`, so
"it never engaged" and "it engaged and did nothing" are distinguishable.

Covered by eight end-to-end young-GC tests (each asserting on a PER-THREAD
cycle counter that the parallel path really ran — the process-global one is
bumped by every other test in the binary) and nine unit tests of `evacuate`'s
refusal and convergence arms, the buffer sizing, and the reservation. The CAS-loser arm needed
a `cfg(test)` seam: on the first cut, measured across the whole young-GC suite,
a 500-parent fan-in produced **zero** CAS losses — every second reader took the
already-forwarded fast path — so a test relying on a real race would have been
asserting nothing. (With the drain sharing work properly the arm now also fires
naturally, a few times per cycle on the wide DAG; the seam stays because that
is a schedule, not a guarantee.)

End-to-end on `bench/BinT.java` depth 18 (`--XX:UseGc Generational -Xmx512m
CRATONVM_MOVING_YOUNG=1 CRATONVM_GC_PAR_THREADS=8`), five consecutive runs: the
copy phase engages every time (`cycles=1 declined_for_slack=0`), helpers scan
~1.3M destinations, and the program's checksum is identical to the
`CRATONVM_GC_PAR_EVAC=0` run. The census is printed by `--verbose:gc` as
`[GC] par_evac:`, unconditionally, including the all-zero line — which is how
both of the reservation defects above were found.

**Soak.** Driven by `CRATONVM_DBG_GC_STRESS` so the copy phase runs hundreds of
times per process rather than once, and checked against closed-form oracles
(each benchmark's own documented checksum, never a second run of this VM):

| dimension | coverage |
|---|---|
| object shapes | `BinT` two-reference nodes; `HashMapOnly` boxed Integers over a resizing REFERENCE ARRAY plus compact-layout nodes; `StringRegexOnly` primitive arrays, Strings and a stateful `Matcher` |
| heap sizes | 128m / 256m / 512m |
| GC stress | 250 KB and 1–2 MB per forced collection |
| worker counts | 2, 3, 4, 8, 16 |

**45 runs, 45 correct checksums, ~12,950 parallel copy cycles**, and
`helper_scans` rises monotonically with the worker count (29,855 at 2 workers
to 49,855 at 16) — so the extra workers really do take work rather than merely
existing. Two runs reported `cycles=0`; the census says so rather than letting
a vacuous run read as a pass.

**Measured on the merged tree**, `BinT` depth 18 at `-Xmx512m` — one large
cycle copying ~1.5M objects out of a 128 MB from-space. ABBA-interleaved
(P S S P), first pair discarded as cold, `objects_copied` checked equal within
every pair, and runs that took the NON-moving sweep retried rather than counted
(only about half of them take the moving path).

| | arm | n | min | median | max |
|---|---|---|---|---|---|
| `cheney_drain` | parallel, 8 workers | 8 | 2,304 ms | **3,008 ms** | 3,601 ms |
| `cheney_drain` | serial | 8 | 8,693 ms | **11,560 ms** | 14,098 ms |
| whole pause | parallel, 8 workers | 6 | 2,035 ms | **2,664 ms** | 2,901 ms |
| whole pause | serial | 5 | 6,316 ms | **9,760 ms** | 12,412 ms |

Both pairs of ranges are DISJOINT — the parallel arm's worst sample beats the
serial arm's best — which is what makes this a result on a shared host rather
than a ratio between two noisy medians. Copy phase: median 3.84x, pessimal
pairing 2.41x. Whole pause: median 3.66x, pessimal 2.18x. Taken with the host
at 100% CPU throughout, and the parallel arm's spread was TIGHTER than in an
earlier quiet-host run (1.6x against 2.4x), so contention is not what produced
the separation.

The pause tracks the phase now because `pre_evacuate` — the from-space
object-start walk, 3,174 ms and the largest phase when this change was first
measured on its own branch — is **0 ms** on the merged tree: another lane
parallelised it. An earlier draft of this entry named it as the next place to
work; that was true of the branch and is not true of `dev`.

Two caveats the numbers do not state. This is a **debug build**: the
per-object copy is unoptimised on both arms, and release is likely to narrow
the ratio, since optimisation makes the copy cheaper while the coordination
stays. And it is **one workload**, chosen because it is survivor-heavy and
therefore the best case for parallel copying.

An earlier wall-clock A/B under `CRATONVM_DBG_GC_STRESS` was discarded rather
than reported: it showed a 6x spread WITHIN one arm against an 11% median gap.
GC stress is the right instrument for a soak and the wrong one for a throughput
measurement — it maximises the number of cycles while minimising the work in
each, which is exactly where a parallel copy has least to offer.

### 2026-09-02 The loop-invariant `arraylength` is hoisted, and it was worth 5.4x

`for (int i = 0; i < a.length; i++)` re-evaluates `a.length` at the top of
every iteration, because that is what javac emits and neither x86-64 backend
moved it. Both do now — `ArrayLenHoist` in the single-pass backend
(`jit/src/x64/licm.rs`), `Op::ArrayLength` support in the optimizing tier's
LICM (`jit/src/ir_optimize.rs`) — and one bounds-checked `char[]` element read
in a compiled counted loop goes from **3.46 to 0.64 ns/char** — onto the
hand-hoisted control, and from 24.7x HotSpot to 4.6x on the same host and run.

The size is the finding. `array-element-load-baseline-codegen-20260901` sized
this at ~5 instructions of 21 by reading the emitter, concluded "44x to about
14x", and never ran the one-method control — the same loop with `a.length` in a
local — that prices it. It was the whole gap. Two things the reading could not
see:

* the traced emitter is not the one that ran. The optimizing tier is *better*
  than the single-pass backend on the local-bound loop and **3x worse** on the
  `arraylength`-bound one, and the routing sends the second shape to it;
* in that tier, `Op::ArrayLength` is impure, so `loop_has_hard_barrier` counted
  it and a javac counted loop **disqualified its own LICM by the very node it
  needed hoisted** — `CRATONVM_DBG_LICM=1` reported `hard_barrier=true,
  0 load(s)` on every candidate header.

That pass also never saw an inner loop at all: it classified a header's inputs
by forward reachability, which for a nested inner header wraps round the outer
back edge and makes every input look like a back edge. Dominance answers it —
one reachability walk with the header deleted — applied only where the old test
found no pre-header, so every loop it already handled keeps its answer.

Neither hoist needs a deopt: both take only sites that run unconditionally on
the first pass through the header, so a null receiver throws the NPE the body
would have thrown, at the same instant and through the same stub.

Also on this path: the array bounds check's length load moved into its cold
stub (`CMP ECX,[RAX+len] ; JAE` — one instruction and four bytes fewer on every
emitted bounds check), the safepoint poll became a single RIP-relative `TEST`
where the flag is in reach, and `ARRAY_LENGTH_OFFSET`'s comment — which said
"8, not 12" above a value of 4 — is now a const assert against
`offset_of!(ObjectHeader, shape)`.

Default-on with a switch each: `CRATONVM_DISABLE_ARRAYLEN_LICM`,
`CRATONVM_JIT_LICM=0`, `CRATONVM_JIT_RIP_SAFEPOINT_POLL=0`,
`CRATONVM_JIT_FUSED_BOUNDS_LOAD=0`. Probe:
`probes/ArrayElemLoadCost.java`. See
`array-element-load-baseline-codegen-FIXED-20260902.md`.

### 2026-08-06 The `ThreadPoolExecutor.execute` receiver-shape special case is gone

Nine dispatch sites across four files decided whether to run
`ThreadPoolExecutor.execute`'s real bytecode by reading the receiver's
`workers` field — eight receiver-shape probes plus the one receiver-blind
`force_native_over_real_jdk_bytecode` arm they existed to override. All nine
and the probe helper are deleted.

What replaced them is class-scoped and lives at registration:
`native_es_execute` is tagged `NativeKind::SyntheticStub` and
`java/util/concurrent/ThreadPoolExecutor` joins the real-protected-stub
allow-list, so the one centralised arbitration yields it to the real
`execute()` body for every receiver, on both the warm and the cold dispatch
path. That became correct once the entry above removed CratonVM's ability to
mint a fabricated executor at all. **The native is not deleted** — strict mode
declines to admit it, and the `--features synthetic-jdk` build still runs it,
which is the only build where the real `execute()` bytecode is absent.

No behaviour change on a real JDK image: `probes/L10ThreadPoolInitProbe`,
`JdkOnlyCensusLoadProbe` and the three-arm strict-corpus gate are unchanged in
both modes, and the registry census moves by exactly the two retagged
registrations (`bridge` 10,434 → 10,432, `synthetic-stub` 755 → 757, total
unchanged). Stub ratchet re-frozen 553 → 555 — no new fake; two registrations
that were mis-tagged `Bridge` are now counted where they belonged.

See `jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`.

### 2026-08-06 `Class::is_synthetic_stub` is deleted; `ClassOrigin` is the only answer

The bool answered two different questions — *is this a compatibility
substitution?* (the census and `--jdk-only` policy question) and *does this
class have no class file, so dispatch must look for a native under its own
exact name?* Splitting them is what let `java/lang/reflect/Proxy$Instance` be
reclassified honestly: it is `ClassOrigin::VmInternal`, a generation artefact,
not a stand-in for bytes that were never found — while keeping the three
dispatch sites that genuinely need it, which now ask
`Class::dispatch_lacks_class_file`.

A fabricated `$$Lambda` / `$ProxyN` / `Generated*Accessor*` is likewise
reported as what generated it, the same answer the define-from-bytes path
already gave those names.

`--dump-class-origins` on a dynamic-proxy workload: 420 rows before and after,
`compatibility-stub` 14 → 13, `vm-internal` 1 → 2 — exactly one class moved,
and under `--jdk-only` that probe now fabricates none at all.

See `jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md`.

### 2026-08-06 `Executors.new*` returns real JDK executors in real-JDK mode

`java.util.concurrent.Executors`' pool factories are no longer intercepted when
CratonVM runs against a real JDK image: the real `Executors` bytecode constructs
every executor, so a factory-made pool is built by the genuine
`ThreadPoolExecutor.<init>` rather than by a native that allocated the object and
then tried to reproduce the constructor.

**User-visible fix.** `Executors.newSingleThreadExecutor()` returned a bare
`ThreadPoolExecutor` where the JDK returns
`Executors$AutoShutdownDelegatedExecutorService` wrapping one. Every
`instanceof ThreadPoolExecutor` on the result flipped, and the pool the JDK
guarantees is unconfigurable accepted `setCorePoolSize`. It now matches HotSpot.

Also removed: two fallbacks in the old construction path that wrote a
two-slot placeholder shape onto a real-layout object and returned it as if
construction had succeeded. Nothing observed them firing, but while they existed
an executor could be half-built, which is the receiver shape nine dispatch sites
in the interpreter exist to detect.

`probes/L10ThreadPoolInitProbe` (new) is byte-identical to HotSpot 25 under both
`--real-jdk` and `--jdk-only`. A diagnostic added with it, `CRATONVM_DBG_TPE_SHAPE=1`, reported every
`ThreadPoolExecutor.execute` receiver-shape decision; it was removed the same
day together with the predicate, when L11 item 7 deleted all nine dispatch
sites (see below). The
`--features synthetic-jdk` build is unaffected — it has no real `Executors`
bytecode to fall back to and keeps its own factories.

See `L10-blocker-threadpool-init-DONE-20260806.md`.

### 2026-08-05 CPU benchmark table re-measured in a quiet window; Sieve at parity

All seven CratonBench rows re-taken in one interleaved series on `dev`
@ `ded183df8` against JDK 25.0.3, in a window opened only once the load fell
below 2.5 **and** nothing else was pinned to the measuring core. CratonVM's
run-to-run spread is under 1% on five of the seven rows.

| | ratio | was 2026-07 |
|---|---|---|
| Arithmetic | 1.95x | 2.44x |
| Fibonacci(44) | 5.89x | 2.79x |
| **Sieve** | **0.99x** | 2.28x |
| **Matrix** | **0.99x** | 2.93x |
| HashMap | 2.07x | 1.75x |
| String/Regex | 5.37x | 7.7x |
| Binary Trees | 9.46x | 8.34x |

Two rows are now at parity with HotSpot C2. Sieve's 6.50x of 2026-08-04 was a
live regression and is fixed; see the entry below.

**Sieve's HotSpot arm is bimodal** — ~2,369 ms or ~2,739 ms with nothing
between, so a 9-sample median reports whichever mode won, and two consecutive
series on an unchanged binary read 2,386 ms and 2,734 ms. That row's figure is
the median of 18 pooled samples; the cleanest single series would have claimed
0.87x, i.e. CratonVM 14% faster than HotSpot, which the data does not support.
CratonVM's own samples on that phase are unimodal.

### 2026-08-04 The optimizing tier stops taking methods the single-pass backend does better

`cov-02` taught `IrBuilder::build` to lower `bastore`. The side effect was that
`CratonBench.sieve([ZI)I` stopped falling through to the single-pass backend —
which *vectorises* its `boolean[]` loops — and started getting a scalar IR
body. **2,462 ms became 15,823 ms**, on a phase where CratonVM had been faster
than HotSpot C2, with an unchanged checksum and no failing test.

The general problem: the optimizing tier installs its body whenever it *can*,
and nothing checks that the body is faster than the one the single-pass backend
would have installed.

#### Added
- `jit/src/x64/single_pass_only.rs` — the enumeration of what the single-pass
  backend can do that the optimizing tier cannot: seven classes, each consumed
  by a single-pass emitter at a loop header, each without a counterpart in
  `ir_optimize`/`ir_lower` (three bulk byte-array lowerings, four vectorising
  ones). The admission chain consults it and its verdict names which lowering
  it protected. **The IR tier has no vectoriser at all**, so `cov-02` hitting
  one of these was not bad luck — four more of the same shape were waiting.
- `CRATONVM_JIT='-c1-vector-veto'` — hand those methods back to the IR tier.
  Default on; the switch exists so the veto is bisectable and so its blast
  radius can be measured on one binary rather than argued across two.

#### Changed
- `x64/driver.rs`'s three inlined bulk-byte detector loops are now one call to
  `escape_analysis::detect_bulk_byte_loops`, shared with the veto, so the
  emission path and the admission chain cannot disagree about what the backend
  would emit.
- Corrected two stale comments in `ir_optimize.rs`: `unroll` and `licm` are
  default-**ON**, not "Default-OFF while it soaks". They are why the
  single-pass unroller and hoists are *not* on the veto list, so the stale
  claim was load-bearing in the wrong direction.

#### Notes
- Blast radius, measured with the off-switch on one binary across all ten
  benchmark phases: **exactly one** IR body, the one that was 6.4x slower.
- Loop unswitching was in the first draft of the list and is not in it: its
  emitter's own contract says the sequence is additive and "removing the
  emission yields identical final state". Vetoing on it would have cost IR
  bodies for every loop with an invariant branch to protect nothing.
- Still open: the enumeration catches an advantage somebody wrote down, not one
  nobody did. Closing that needs a backend-parity harness that compiles a
  corpus both ways and compares emitted bytes — see
  `perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md`.

### 2026-08-03 Perf gate: it records its own C2 reach, and it can compile its benchmark again

Two changes to `regression-suite/perf/`, from `docs/known-issues/c2/`'s MEAS-02.

**The gate could not compile `bench/CratonBench.java`.** Its own
`export LC_ALL=C` — correct, for the awk distribution arithmetic — makes a
`javac` that derives its source encoding from the platform charset (17 on the
bench host) default to US-ASCII, and the benchmark's header comment has
em-dashes. Every run died at setup with 30 `unmappable character` errors before
measuring anything. The bench host's ambient locale is `C.UTF-8`, so the same
command run by hand succeeded and the failure appeared only inside the gate.
Pinned with `-encoding UTF-8`.

**Every run now records the optimizing tier's per-phase reach.** Across all
seven CratonBench phases the C2/IR tier is asked 8 times, admits 3 and produces
**3** bodies — so the gate measures the single-pass backend, and a CratonBench
delta is not evidence about C2 in either direction. That fact now travels with
the numbers instead of having to be rediscovered.

#### Added
- `ir_requests` / `ir_admitted` / `ir_bodies` in `samples.tsv` and
  `summary.tsv`; one `ir_reach_<phase>` line per phase in `manifest.tsv`, plus
  `ir_reach_total`, `ir_reach_recorded` and `ir_reach_scrape_broken`; a reach
  summary on the console at the end of every run and of every `--calibrate`.
- `regression-suite/perf/c2-reach.sh` — any workload's C2 reach in one run,
  with two consistency checks that refuse rather than report a zero when the
  scrape is reading a log that no longer says what it expects.
- `bench/CratonBenchC2.java` — a candidate workload with a framework-shaped
  node mix, reaching the tier 36/17/11 across three phases. Deliberately **not**
  a gate phase and with no baseline; see
  `meas-02-bench-suite-c2-reach-RETIRED-20260803.md`.

#### Changed
- Results schema **1 → 2**; the default results directory is now
  `regression-suite/perf/results/v2/`. Every existing column kept its name and
  every consumer resolves columns by name, so a v1 reader reads a v2 directory
  correctly.
- `compare.py` reports `C2-tier compiles`, `C2 admitted` and `C2 bodies`
  separately, and names the phases whose delta is not evidence about the
  optimizing tier. `compiles_c2` alone never was that number: it counts
  compiles whose requested *tier* was C2, including every one the optimizing
  pipeline declined and handed back to the single-pass backend, and including
  OSR compiles.
- The gate asks for its VM summaries with the grouped `CRATONVM_DBG=` spelling,
  so a run no longer opens stderr with a legacy-variable deprecation line.

### 2026-07-31 JDK-only mode (`--jdk-only`) — provenance instrumentation, wave 1

A new **runtime** compatibility policy: `--jdk-only` declares that real JDK class
bytes are authoritative, so no non-array class is fabricated without real bytes
and no `NativeKind::SyntheticStub` native is registered or invoked. It is
orthogonal to `--real-jdk` / `--synthetic-jdk`, which select *which class
library* boots; this selects *which substitutions are permitted*. One binary
runs both policies, so a failure can be A/B'd in the same shell.

**This is an internal diagnostic, not a supported runtime mode.** Wave 1 is
instrumentation and measurement: only class fabrication and synthetic-native
*registration* actually enforce, while the remaining dispatch paths are counted
rather than blocked. A program that runs fine under `--real-jdk` may fail under
`--jdk-only` — that is the signal the mode exists to produce. The default
(`compatible`) behaviour is unchanged, on both the launcher and embedded entry
points, and is reached by doing nothing. Normative contract:
`docs/feature-designs/jdk-only-mode.md`; operator guide:
`docs/jdk-only-migration.md`.

#### Added
- `--jdk-only` launcher flag. Implies `JdkMode::Real` and requires a real JDK
  runtime image — there is no silent fallback, and the failure names the flag,
  the searched paths and the accepted JDK layout. Conflicts with
  `--synthetic-jdk` (that library *is* the set of substitutions the flag
  forbids), and the conflict is diagnosed as a policy error rather than a
  library error.
- Four diagnostic flags, all usable in either mode — under the default
  `compatible` mode they census what strict mode *would* reject:
  `--jdk-only-report <FILE>` (JSON violations plus class-origin and
  per-`NativeKind` invocation counters, `schema_version` 1),
  `--dump-class-origins <FILE>` (see below), `--trace-jdk-only` (log each
  recorded violation to stderr), and `--explain-jdk-only` (long-form
  operator-facing explanation per violation, and leaves absolute paths
  unredacted in every report file; they are redacted by default).
- `--dump-class-origins <FILE>` — a new class-origin census, one row per class
  the class manager holds: `{name, origin, reason, requested_by,
  real_bytes_found, loader_id}`, sorted by `(name, loader_id, origin)` for
  byte-stable output, with a `counts` block keyed by origin tag.
- A `ClassOrigin` provenance model on `Class` (`classloading/src/class_origin.rs`),
  replacing "is this a stub, yes or no?" with where the bytes actually came
  from: `BootImage`, `ApplicationClassPath`, `UserDefined`, `VmArray`,
  `HiddenClass`, `GeneratedLambda`, `GeneratedProxy`, `ReflectionAccessor`,
  `VmInternal`, `CompatibilityStub`. Only `CompatibilityStub` is rejected under
  the strict policy; arrays, hidden classes, lambdas, proxies and reflection
  accessors are products of a conforming JVM and are allowed, with their own
  distinct origins. The pre-existing `Class::is_synthetic_stub` bool is
  retained as a **derived mirror** of `origin.is_compatibility_stub()` (~160
  read sites across 17 files depend on it); both are written together through
  `Class::set_origin`.
- Shared policy token `cratonvm_types::compat` (`CompatibilityMode`,
  `ExecutionPolicy`) with `NativeKind::allowed_in` and `ClassOrigin::allowed_in`
  as predicates next to their own types, a structured `JdkOnlyViolation` error
  family in `types/src/error.rs`, and a single policy-aware
  `resolve_dispatch` / `DispatchDecision` native-vs-bytecode decision point in
  `vm/src/vm/vm_exec.rs` that the main interpreter path now routes through.
- Per-VM policy state: `VmConfig::compatibility_mode` (plus `is_jdk_only`,
  `execution_policy`, `validate_compatibility`), propagated into the native
  registry and the `ClassManager` at VM init. No process globals were added for
  this feature.
- C ABI (`libcratonvm`): `cratonvm_create_with_compatibility(args, mode)`,
  `cratonvm_compatibility_mode(vm)` (read back what the live VM actually got),
  and `cratonvm_compatibility_mode_supported(mode)` (a capability probe that
  needs no VM, so a host can avoid a failed create). The mode constants are
  `CRATONVM_COMPATIBILITY_COMPATIBLE = 0` and
  `CRATONVM_COMPATIBILITY_JDK_ONLY = 1`. **These numeric values are a published,
  append-only part of the ABI** — a value may be added, never renumbered — and
  they are `cratonvm_jint` rather than a boolean so a third posture can be added
  later without breaking a compiled host. An unrecognised value is *rejected*
  (`NULL` + `cratonvm_last_error()`), never clamped to `COMPATIBLE`;
  `cratonvm_compatibility_mode` returns `-1`, never a mode value, on a bad
  handle. The option string `"--jdk-only"` is the second route to strict mode
  and the only one available to `JNI_CreateJavaVM`; passing
  `CRATONVM_COMPATIBILITY_COMPATIBLE` alongside `--jdk-only` is a contradiction
  error, not a precedence rule.
- A 21-vector strict regression corpus (`regression-suite`, `SUITE=jdk-only`)
  indexed against the blocker rows in `docs/jdk-only-runtime-services.md`, and
  an advisory `jdk-only` CI job that runs the censuses. Some strict-mode vectors
  are **expected to fail** while fabrication enforcement is incomplete: that is
  the enforcement test working, not a regression.

#### Changed
- **`--dump-native-registry` output format changed (consumer-visible).** The
  native census now emits `"schema_version": 2`; the previous output carried no
  `schema_version` key at all, so any consumer that parsed the old shape needs
  updating. Each `natives[]` entry gains `registered_by` (the registration site,
  captured via `#[track_caller]`), `overwrote` (the `NativeKind` of the entry
  this registration replaced, if any — registration is last-write-wins), and
  `invocations` (times the slot was dispatched this run). A `real_declaring_method`
  field is present and is `null` on every row today; filling it in needs a
  *non-initiating* probe of the runtime image, because resolving it at shutdown
  through ordinary class loading would load classes the run never touched and
  change the very census the file reports. A top-level `"invocations"` block
  gives the per-`NativeKind` dispatch totals. Rows are sorted by
  `(class, name, descriptor, registered_by)` — `registered_by` is part of the
  key because a superseded row and the row that overwrote it share the triple.
  Absolute paths in `registered_by` are redacted unless `--explain-jdk-only` is
  passed.
- `NativeMethodRegistry` gained VM-scoped policy (`set_compatibility_mode`,
  `compatibility_mode`), a refusal log (`refused_registrations`), a
  `schema_version` 2 census (`census`), and hot-path-safe invocation counting
  (`record_invocation`, `invocations_of_kind`) that does not require `&mut self`.
  Under `JdkOnly`, `register()` refuses to insert a `SyntheticStub` and records
  a `SyntheticNativeRegistered` violation instead.

#### Deprecated
- **`CRATONVM_REAL=-stubs` in favour of `--jdk-only`.** The env token keeps
  working unchanged as a native-registry filter, but it now prints a one-time
  note recommending `--jdk-only`: the token can only drop stub *registrations*,
  and cannot express the class-loading or dispatch half of the policy. Strict
  mode is deliberately never inferred from `CRATONVM_REAL` / `CRATONVM_NO_STUBS`,
  from a Cargo feature, or from what the host machine has installed — a run must
  not end up enforcing rules nobody asked for.

#### Known follow-ups
- The residual synthetic-stub set is unchanged: `native-builtins/tests/stub_ratchet.rs`
  still freezes `BASELINE_SYNTHETIC_STUBS = 157` exactly with `SLACK = 0`, and
  the end-state `strict_mode_refuses_nothing` test is deliberately `#[ignore]`d
  until that baseline reaches zero. Wave 1 refuses those registrations under
  `--jdk-only`; it does not retire them. The path is reclassification first,
  deletion second — a previous global drop was reverted the same day it landed.
- The wave-2 backlog is ranked by danger in `docs/known-issues/jdk-only/README.md`;
  its first tier causes **silent wrong behaviour** rather than clean failure.
  Summarised with staging gates in [ROADMAP.md](ROADMAP.md#jdk-only-mode---jdk-only).

---

### 2026-07-11 GPU offload — first real-hardware validation and feature completion

First systematic validation of the GPU offload stack on real hardware (RTX
2060, sm_75, CUDA driver 591.86, `--features gpu-driver`). Two passes the same
day: a morning validation run that found and fixed two dispatch-correctness
bugs, and an evening feature wave that closed most of the follow-ups the
morning pass turned up. See
`gpu-offload-followups-20260711.md` for full detail and
remaining open items.

#### Fixed (morning validation pass)
- Offload-eligible `invokestatic` call sites were being promoted into the interpreter's invoke cache after their first dispatch (or first per-call `--gpu-min-work` rejection), permanently bypassing the GPU offload hook on every later call at that site — a cached target dispatches straight to the CPU body and never re-enters `try_dispatch`. Fixed by never promoting a `Handled`/`HandledWithValue`/`FallThroughKeepHooked` site into the invoke cache (`vm/src/runtime/offload.rs`, `DispatchOutcome`).
- A failed kernel's bounds-check `failure_flag` was drained *after* the kernel's array writebacks, so a bounds-check failure let partially-corrupted device state copy into the Java heap before the failure was observed — violating the documented "the interpreter observes no partial GPU state on kernel failure" guarantee. Fixed by draining `FailureFlag` writebacks first regardless of push order (`vm/src/runtime/offload.rs::finalize_submission`).
- Benchmarked the fixed dispatch path against HotSpot JDK 25 (C2) and TornadoVM 4.0.1 (PTX backend) on an RTX 2060, checksums matching HotSpot bit-for-bit at every size: div-chain kernel (48 data-dependent integer divisions/element, unvectorizable on x86) 204–235× over the best CPU; 96-multiply-add kernel (a shape HotSpot C2 *can* auto-vectorize) ties or beats vectorized HotSpot C2 and outruns TornadoVM ~2× on the same kernel. See `bench-gpu/results/` and the README "GPU offload benchmarks" section.

#### Added (evening feature wave)
- Transparent offload for integer/long reduction kernels (`)I`/`)J`-returning methods, e.g. `sum += a[i]*b[i]`) — the interpreter's void-return-only dispatch gate is lifted for proven reductions, with the scalar result pushed onto the operand stack. `)F`/`)D` reductions stay CPU-only by design (GPU float atomic-add is not bit-identical to Java's sequential fp accumulation). Found and fixed in the process: the reduction PTX epilogue emitted the 2-operand `atom.global.add` form, which `ptxas` rejects with "Arguments mismatch" — every reduction kernel had been silently failing module load and falling back to CPU since the epilogue was written; fixed to the 1-operand `red.global.add` accumulate form, with new `ptxas` round-trip tests added for all six lowering shapes (`jit-cuda/src/lowering.rs`). Measured (RTX 2060, N = 2²⁴, `bench-gpu/GpuDotBench.java`, checksum bit-exact): CratonVM-GPU 18 ms vs CratonVM-CPU 76 ms vs HotSpot C2 7 ms — the GPU beats CratonVM's own CPU 4.2× but not vectorized HotSpot C2 at this size (the kernel is PCIe-bound plus single-cell atomic contention); the value is completing the transparent-offload surface for a reduction shape TornadoVM 4.0.1's own PTX backend currently throws `TornadoInternalError: unimplemented` on (`bench-tornado/TornadoDotBench.java`).
- Offload eligibility and lowering for `ldc`/`ldc_w`/`ldc2_w` constant-pool loads (int constants outside `sipush` range, and any float/double/long literal) — previously any such constant killed eligibility for the whole method. Measured: `bench-gpu/GpuLdcBench.java` (96-step multiply-add chain, N = 2²⁴) warm 8 ms on GPU vs ~2,000 ms CPU-bound before, sample bit-exact vs HotSpot.
- A curated `Math`/`StrictMath` GPU-intrinsics table under the existing `ALLOW_INTRINSIC_CALLS` admission hint — `sqrt`(double), `abs`/`min`/`max`(int/long/float/double, NaN- and signed-zero-correct per Java's contract), `fma`(float/double) — replacing the previous analyzer hole where any `invokestatic` was admitted but the emitter had no lowering for any of them, so every such method silently blacklisted itself to the CPU. `sin`/`cos`/`exp`/`log`/`pow` are deliberately excluded: PTX only offers `.approx` transcendentals, which would silently violate Java's `Math`/`StrictMath` precision contract. See `docs/gpu/annotations.md`.
- `frem`/`drem` (IEEE remainder) lowering, gated behind the existing `ALLOW_DIV_BY_ZERO` admission hint (reused rather than adding a new hint for one opcode pair); exact only for bounded quotients.
- `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` value-form lowering (bit-exact `setp`/`selp` sequences, correct NaN-result asymmetry between the `l`/`g` variants). A compare that feeds a branch still rejects at the branch opcode, so no new false eligibility was introduced.
- Non-zero-start counted loops (`i = K; i < bound; i++` with `K >= 0`, `K` sourced from an `ldc`).
- A JIT-caller admission gate (`vm/src/runtime/offload_jit_gate.rs`) that denies JIT/OSR compilation of any caller method containing an offload-eligible call site while `--gpu` is active, wired into all 5 JIT/OSR admission checks in the interpreter — closes the "a hot caller's OSR silently degrades offload back to CPU" structural gap. Hardware-validated: 100 hot repetitions of a caller loop at N = 2²² hold steady at 2 ms warm per call.
- `dispatch_async` launch-configuration fixes: the thread-count floor no longer clamps every launch to a minimum of 2²⁰ threads (the real per-call array length wins when known), and block size now comes from `cuOccupancyMaxPotentialBlockSize` instead of a fixed 256-thread block.
- Async API surface: a non-blocking `poll_submission_status` now backs `Native.futureIsDone`/`futureStatus` (a real device probe via a best-effort `cuLaunchHostFunc` host callback, falling back to non-blocking `Event::query`/`cuEventQuery`), finalizing a submission inline the moment the device reports done — `isDone()` returning `true` now means the submission is really finalized, not just "probably." `Native.futureGetResult` now surfaces real scalar reduction results (boxed as `Integer`/`Long`/`Float`/`Double`) from the real submission registry instead of only the pre-Phase-6 synthetic stub map. `GpuExecutor`'s default CUDA stream is now real and shared across an executor's submissions instead of a fresh private stream per dispatch (`resolve_or_create_default_stream`); `newStream()` also now mints a genuine CUDA stream, though nothing yet routes a dispatch onto it explicitly. `GpuArray.allocate`'s Rust-side native shims (`arrayAllocateInt/Long/Float/Double`) landed; the `craton-gpu-java` jar binding is still pending. `i8`/`i16` bulk array marshalling. `--print-gpu-decisions` is now self-sufficient — it no longer requires a separate `RUST_LOG=info` to see any output.
- `.github/workflows/gpu-selfhosted.yml` + `bench-gpu/ci-gate.sh` — weekly self-hosted-GPU-runner CI scaffolding for the `bench-gpu/` benchmark suite (checksum-verified); runner enrollment against the workflow's runner label is still pending.

#### Known follow-ups
- `GpuFuture` completion is now poll-driven but still not push-driven: `isDone()`/`getNow()` do a real non-blocking device check and finalize inline, but nothing drives that check without an application thread calling it — no background thread or driver callback completes a future on its own yet.
- 2-D/nested loops and general (non-loop-guard) branches are still rejected by the analyzer; `)F`/`)D` reductions remain CPU-only by design.
- Full open-items list in `gpu-offload-followups-20260711.md`.

---

### 2026-06 multi-agent review remediation

A second, larger review-driven remediation pass (one Opus agent per finding, merged in
severity order with a build gate) closed the full critical/high/medium tier plus perf and
features. Highlights:

#### Security
- `SecureRandom` now draws from the OS CSPRNG (`BCryptGenRandom`/`getrandom`) instead of an invertible splitmix64 DRBG (`native-builtins/src/crypto_impl.rs`); RSA private-key ops gained base blinding.
- SSRF: the always-on cloud-metadata/link-local block now unwraps IPv4-mapped/compatible IPv6 (`::ffff:169.254.169.254`) (`native-io/src/outbound_policy.rs`); optional outbound-hostname DNS resolution closes the alias/rebind bypass.
- Built-in HTTP server honors `Transfer-Encoding: chunked` (request-smuggling/body-desync fix); HTTP client strips `Authorization`/`Cookie` on cross-host redirects (`native-builtins/src/{net_phase_e,http_client}.rs`).
- `X509Certificate.verify` fails closed; `Class.forName` rejects control-byte/separator/`..` injection; AOT cache integrity moved to SHA-256.
- New sandbox/egress knobs documented in `docs/SECURITY_HARDENING.md`.

#### Soundness (GC / JIT / memory safety)
- Closed the JIT/GC "register-resident root" use-after-free family: a uniform native-root registry (`vm/src/memory/native_roots.rs`) + per-subsystem scan/remap for native collections overlays, NIO selector keys, ScheduledThreadPoolExecutor runnables, XNIO IoFutures, the ClassFileTransformer chain, the `ObjectStreamClass` cache, and value-stack smuggled jobjects; JIT x64 now spills callee-saved operand-stack oops at safepoints.
- JNI: implicit local-reference frame around native calls + a refcounted GC pin set for `GetPrimitiveArrayCritical`/`Get*ArrayElements` (`gc/src/pinned.rs`).
- CompactHeader forwarding pointers no longer truncate above 4 GB; per-thread SATB buffers are drained at remark; concurrent-mark 16-byte slot reads are stripe-locked; ZGC backend runs reference processing.
- `vm-exec` JNI TLS cleanup is RAII (panic-safe); `<clinit>` failure no longer leaks the init claim; libcratonvm hands out validated opaque handles instead of raw heap pointers.

#### Correctness
- Bytecode verifier rejects unverified `jsr`/`ret` by default; `ldc`/`invokespecial` verifier-model fixes.
- `BigInteger.modPow`/`modInverse` honor signs and throw on non-invertible input; `AtomicXFieldUpdater` RMW ops no longer lose updates; `AbstractStringBuilder.getChars` bounds-checks; interpreter runs `finally`/catch-all on JIT-unknown-PC unwind.
- JNI `DefineClass` defines from the supplied buffer; `Call*MethodV`/`Call*MethodA` implemented.

#### Performance
- Thread-local scratch buffer for socket read/write (no per-syscall `Vec`); O(1) maps for JNI global refs, unified-logging handles, and the regex cache; bounded JIT code-cache + deopt history; metaspace bump fast-path.

#### Features
- Advisory `cargo-llvm-cov` coverage workflow (`.github/workflows/coverage.yml`, `docs/COVERAGE.md`).
- Container/cgroup-aware default heap sizing (`vm/src/runtime/container.rs`, `docs/CONTAINER.md`).
- `README.md` for `libcratonvm` and `cratonvm-embed` (crates.io pages); embedding guide (`docs/EMBEDDING.md`).
- Five L/XL design docs under `docs/feature-designs/` (precise-JIT-maps-default, deopt/OSR, concurrent-GC maturation, foreign-thread attach, differential fuzzer).

#### Build / OSS
- MSRV raised `1.77` → `1.80` (`Cargo.toml`, `clippy.toml`) to match the std APIs the code already uses; `gc`/`reader`/`craton-gpu` clippy cleaned.
- Untracked the gitignored `bench/` build artifacts and stray `dd1.out` (kept on disk); test-fixture `.class` files retained.
- Crate-count references corrected to **20** workspace members (`libcratonvm`, `cratonvm-embed`, and `cratonvm-difftest` present; `fuzz/` remains standalone); `docs/CRYPTO_STATUS.md` reclassified PBKDF2/ML-KEM/DESede as implemented.

---

### Earlier review round

A cross-crate review-driven fix orchestrator landed 50+ commits across security, soundness, correctness, and OSS-distribution hygiene. Highlights:

#### Security
- JEP-290 `ObjectInputFilter` honored with `maxdepth`/`maxrefs`/`maxbytes`/`maxarray` caps (`native-builtins/src/object_input_filter.rs`).
- JAR signer chain verified against the JCE/JDK trust store before classes load (`classloading/src/jar_signer.rs`).
- Panama / FFI host calls gated behind `--enable-native-access`; unauthorized callers throw `IllegalCallerException` (`native-builtins/src/panama_*.rs`).
- `ProcessBuilder.start` and Panama host calls now consult `SecurityManager.checkExec` (`native-builtins/src/process.rs`).
- Test-only TLS certs and keys moved behind `cfg(test)` so they cannot ship in release artifacts (`native-builtins/src/tls_test_certs.rs`).
- Outbound network calls (HTTP/Socket/URL) run through an SSRF policy hook with a per-connect timeout (`native-io/src/net.rs`).
- `RandomAccessFile`, `WatchService`, and `ProcessBuilder` now route paths through `validate_path` before opening (`native-io/src/*`, `native-builtins/src/process.rs`).
- New libfuzzer targets cover classfile reader, JImage parser, PKCS#12 keystore, and JAR signer (`fuzz/fuzz_targets/`).
- `vm` identity-validates resolution-cache keys so a forged class identity cannot poison lookups (`vm/src/runtime/resolution_cache.rs`).
- `vm` verifier-skip path is now gated on the bootstrap classloader identity, not just the loader pointer (`vm/src/runtime/verifier_gate.rs`).

#### Soundness
- SATB pre-barrier wired at remaining `aastore`/`putfield` sites plus a real stop-the-world for `newarray` (`vm/src/runtime/interpreter.rs`, `jit/src/runtime_helpers.rs`).
- `gc` mutating heap entry points now require a `StopTheWorldToken` witness (`gc/src/lib.rs`).
- Async-signal-safe SIGSEGV handler installed on Unix (no allocations, no locks) (`vm/src/runtime/signals.rs`).
- AArch64 icache flush on Linux and FreeBSD after JIT code emission (`jit/src/aarch64.rs`).
- `vm` hot locks reordered through `OrderedMutex` matching `docs/lock-order.md` (`vm/src/lock_order.rs`).
- JIT switch-target offsets are now overflow-checked; `try_patch` replaces panicking `patch_i32`/`patch_byte` (`jit/src/buffer.rs`).
- `reader::ByteView::try_new` returns `Result` on overflow / misalignment instead of UB (`reader/src/byte_view.rs`).
- `gc` bitmap clears use `AcqRel` ordering; the `SATB` write barrier is now part of the trait surface (`gc/src/g1.rs`).
- `jfr::SpscEventRing::Drop` performs a bounded shutdown and releases pending payloads (`jfr/src/ring.rs`).

#### Correctness
- JFR field emit validates variant against declared type per event (`jfr/src/event.rs`).
- Native collections rekey GC overlays on `identity_hash_code` so post-GC pointer remap keeps maps consistent (`native-collections/src/*`).
- Blocking queue park / notify discipline cleaned up with read-locks instead of unsynchronized shared state (`native-collections/src/blocking_queue.rs`).
- CUDA H2D → kernel → D2H now sequenced on the same stream; previous code raced (`cuda-bridge/src/stream.rs`).
- `jit-api` exposes a `validate()` loop, `repr(C)` golden offsets, and a fixed `NUM_FIELDS` constant for ABI lock-in (`jit-api/src/lib.rs`).
- `types::CompactValue::update_object_ptr` returns `Result`, and `as_long_unchecked` documents its lazy-decode invariant (`types/src/compact_value.rs`).
- `native-builtins` `--enable-native-access` audit; Panama host calls check the caller module against the allow-list.
- `reader` attribute shape validation propagates the signature depth-guard "sticky" flag (`reader/src/attribute.rs`).
- `native-builtins` JCA crypto routes AES / AES-GCM through `aes` / `aes-gcm` RustCrypto (constant-time).
- `vm-cli` rebuilt for HotSpot `-Xmx` / `-XX` parsing, `--nojit`, `String[] args` (`vm-cli/src/main.rs`).
- `native-api::allocate` no longer leaks on the error path; `init_level` is monotonic; `tcp_available` no longer clobbers state (`native-api/src/lib.rs`).

#### OSS / Distribution
- `vm-cli` produces the `cratonvm` binary by default; the `java[.exe]` alias is opt-in via `--features java-bin-alias` so `cargo install` does not shadow a real JDK (`vm-cli/Cargo.toml`).
- Added `SUPPORT.md`, `GOVERNANCE.md`, `MAINTAINERS.md`, `THIRD-PARTY-NOTICES.md`, and a GitHub issue-template config (top-level + `.github/`).
- SPDX `Apache-2.0` headers on every Rust source file across the workspace.
- MSRV bumped to 1.77 and synchronized across `README.md`, `BUILD_GUIDE.md`, `CONTRIBUTING.md`, and `docs/INSTALL.md`.
- Workspace version raised to `0.3.0`; every inter-crate `path = "../<crate>"` declaration now carries `version = "0.3.0"` so `cargo publish --dry-run` accepts the manifest.
- Per-crate `README.md` added for crates.io rendering across the then-current publishable crates and tooling crates.
- `fuzz/` has its own standalone nightly-only workspace and remains `publish = false`.
- Workspace crate-count references were aligned in `README.md`, `ARCHITECTURE.md`, and `BUILD_GUIDE.md`; later workspace additions bring the current count to 20.
- CI parked workflows reactivated with `clippy -D warnings` as a hard gate (`.github/workflows/ci.yml`).

#### Known follow-ups
- Re-enable JIT loop unrolling — previous byte-copy unrolling produced corrupt native code and was disabled (`jit/src/x64/unroll.rs`).
- Real-JDK boot via `java.base` JMOD remains opt-in; synthetic stubs cover the default path.
- Concurrent GC marking is still serialized under STW; G1 / ZGC remain experimental.

## [0.3.0] - 2026-05-24

### Added
- Real cryptographic signature verification in `x509_manager::validate_chain` for RSA-SHA256 (PKCS#1 v1.5) and ECDSA-with-SHA256 over P-256, replacing the previous structural-only "signature present" check. DSA-with-SHA1, RSA-PSS, and Ed25519 now report `TrustError::NotImplemented { oid }` so callers can choose to delegate to JCE.
- JIT XMM register allocation for float/double locals (callee-saved XMM8-XMM15 on Windows x64), eliminating frame spills for FP-heavy methods.
- JIT `Math.sqrt` intrinsic inlined as `SQRTSD` instead of going through interpreter dispatch.
- JIT `dup2` opcode support, enabling compound array assignments like `a[i] += x`.
- JIT `ldc2_w` opcode support for loading long/double constants from the constant pool.
- JIT OSR trampoline now transfers float/double locals into their assigned XMM registers.
- JIT `getstatic` caching: unique static field values are loaded once in the method prologue and cached in frame slots.
- JIT `StackSlot::Xmm` operand-stack variant so consecutive double operations chain in XMM registers without memory traffic.
- Extracted 10 crates from the monolithic vm: classloading, gc, jit, jit-api, types, native-api, native-builtins, native-collections, native-io, jfr.
- G1 and ZGC garbage collectors.
- AArch64 JIT backend (partial; 45% of x86-64 opcode coverage).
- Java Flight Recorder support.
- JVMTI event framework.
- Security hardening: checked arithmetic throughout GC and JIT.

### Changed
- MSRV bumped to 1.77 (was 1.75).
- Updated benchmark numbers against JDK 25.0.1 C2: QuickBench 1.50x, Fannkuch 1.57x, N-Body 20x (down from 464x interpreter-only).
- Added Binary Trees (CLBG) benchmark, exposing a GC allocation bottleneck (23.3x ratio).
- N-Body and Fannkuch-Redux benchmarks now run to completion with correct results.
- Rewrote roadmap with an honest production-readiness evaluation distinguishing real working features from Rust-side stubs.
- New tiered priority matrix (Tier 0 basic correctness through Tier 3 production grade) with measurable success metrics verified against real Java code.

### Fixed
- VM-generated exceptions (NPE, AIOOBE, ArithmeticException, ClassCastException, etc.) are now catchable by Java `try/catch` instead of being Rust-side errors that bypassed exception handling.
- `HashMap.entrySet()` iteration: synthetic inner-class types like `HashMap$Entry` now satisfy `checkcast`/`instanceof` against `Map.Entry`, `Iterator`, `Iterable`, `Collection`, and `Comparable`.
- `Thread(Runnable)` and `Thread(String)` constructors are now registered; `thread.start()` works as an alias for `start0()`.
- `Class.getName()` and `Class.getSimpleName()` are now registered.
- `java.io.FileWriter` constructors and write methods are registered, including append mode and `File`-path overloads.
- JIT-compiled methods returning `boolean`/`byte`/`char`/`short`/`float`/`double` now return the correct value instead of being treated as `void`.
- JIT call dispatch now preserves `float` and `double` argument bit patterns (previously collapsed to 0).
- JIT invoke dispatch now installs the thread context before executing compiled code, fixing `invokevirtual`/`invokeinterface` returning 0.
- `Stream.filter(...).count()` and `stream().filter(...).collect(...)` now return correct results (previously returned 0 or stack-overflowed).
- JIT register allocator rewritten to use instruction-level liveness, fixing Fannkuch miscompilations where two locals shared a register.
- JIT operand-stack canonicalization at forward-branch targets and dead-to-live transitions, fixing miscompilation on complex control flow.
- JIT `ifeq..ifle` now uses `TEST` instead of `CMP reg,reg`, correctly setting flags.
- JIT `if_icmpXX` codegen optimized to use direct register comparison.
- N-Body segfault root-caused to loop unrolling producing corrupt native code; N-Body now runs cleanly with unrolling disabled.
- JIT loop unrolling re-enabled behind a byte-copy-safety predicate. The byte-copy unroller is only correct when every opcode in the body is position-independent (or one of the rel32 patch flavours the duplicator now handles, namely `forward_patches`, `bounds_check_stubs`, and `null_check_store_stubs`). Bodies containing field/static accesses, invokes, allocations, throws, instanceof/checkcast, monitor ops, switches, or any other helper-call opcode are skipped. Set `CRATONVM_UNROLL_UNSAFE_BODIES=1` to re-enter the legacy unguarded path for bisection.
- Integer truncation in array allocation (security).
- Unchecked branch offsets in JIT (security).
- Path traversal in resource loading (security).
- StringBuilder `insert()` O(n^2) performance regression.
- Bytecode verifier now accepts `InterfaceMethodref` for `invokestatic`/`invokespecial` (Java 8+ static interface methods).
- `SSLEngine` handshake state machine: `wrap`/`unwrap`/`beginHandshake` transitions.
- Crypto `deriveKey`/`deriveData` now call the HKDF implementation instead of returning empty output.
- File descriptor leak in `fd_table`: rollback on overflow, `close()` returns `Result`.
- Serialization write methods now throw `UnsupportedOperationException` instead of silently succeeding.
- JIT negative cache: failed compilations are no longer re-attempted on every invocation.
- `vm-cli` args-array error handling uses `map_err` instead of `with_context` on non-`Error` types.

### Performance
- GC `alloc_array` no longer double-zeroes the data region; the redundant memset after young-gen allocation is removed.
- GC young-gen mutex is released before the zero-init memset, so large-allocation latency no longer holds the global allocation lock.
- N-Body FP arithmetic improved from 464x to 20x vs JDK 25 C2 via XMM stack slots, `Math.sqrt` intrinsic, and OSR XMM transfer.

### Known Issues
- JIT loop unrolling is now gated on a byte-copy safety predicate (above); pure-arithmetic and array-index-store kernels are unrolled, but loops with field accesses or invokes still execute unrolled-by-1 until the duplicator learns to clone deopt/exception/MIC/PIC stubs.
- BigDecimal/BigInteger arithmetic on post-clinit-populated statics returns 0 (`BigDecimal.ONE.add(BigDecimal.TEN)` yields 0). Boot paths that only reference these values work; numeric workloads (JDBC numeric, Jackson numeric) do not.
- `ForkJoinPool.invoke(RecursiveTask)` at recursion depth >= 10 returns 0 due to a JIT register clobber in deeply-recursive boxed-`Long` arithmetic. Workaround: disable the JIT for affected workloads.
- GC throughput is roughly 23x slower than JDK on allocation-heavy workloads (Binary Trees).

## [0.2.0] - 2025-06-01

### Added
- x86-64 JIT compiler with 26 optimization rounds (~140 bytecodes compiled)
  - AVX2 SIMD vectorization for integer reduction loops
  - On-Stack Replacement (OSR) at hot loop back-edges
  - Loop-Invariant Code Motion (LICM)
  - Array Bounds Check Elimination (BCE)
  - Magic number division (no IDIV)
  - SSE float/double arithmetic pipeline
  - SoA (Structure-of-Arrays) value layout for 44% memory reduction
- Generational garbage collector with write barriers and card table
- Multi-threading with monitors, ReentrantLock, CountDownLatch, Semaphore, CyclicBarrier
- Virtual threads (simplified carrier-based scheduler)
- Java 11 support: nest-based access control (JEP 181)
- Java 17 support: records (JEP 395), sealed classes (JEP 409)
- Java 21 support: pattern matching for switch, sequenced collections
- Java 25 support: stream gatherers, scoped values, structured concurrency
- Panama FFI: MemorySegment, Arena, ValueLayout, SymbolLookup, Linker (downcall/upcall)
- 3,100+ native method registrations across java.lang, java.util, java.io, java.time, java.nio
- Full reflection: Class.forName, Method.invoke, Field.get/set, Constructor.newInstance
- Lambda/invokedynamic via LambdaMetafactory and StringConcatFactory
- CONSTANT_Dynamic (condy) support
- Enhanced NPE messages (JEP 358)
- Hidden classes (JEP 371)
- Partial JNI function table (229 slots, 13 implemented)
- Module system basics: Module, ModuleDescriptor, ModuleLayer
- Class file versions 45-69 (Java 1.1 through Java 25)
- Dependabot for automated dependency updates
- CODEOWNERS for review routing
- GitHub Security Advisories for private vulnerability reporting
- ARCHITECTURE.md for contributor onboarding
- Release workflow for automated binary builds

### Changed
- Improved SAFETY documentation on unsafe blocks in heap allocator
- Added checked allocation methods (`alloc_object_checked`, `alloc_array_checked`)
- Replaced test `panic!()` calls with proper `assert!` macros in GC and JIT tests
- Updated test documentation references across the public docs.

### Performance
- Within 1.41x of JDK 25 C2 on QuickBench overall
- Fibonacci(42): 1.07x — within 7% of C2

## [0.1.0] - 2025-01-15

### Added
- Bytecode interpreter with 200+ JVM instructions
- `.class` file parser supporting all standard attributes
- Command-line launcher with classpath and heap size configuration
- CI pipeline with cross-platform testing (coverage and Miri jobs scaffolded but planned, not yet enabled)

[Unreleased]: https://github.com/craton-co/cratonvm/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/craton-co/cratonvm/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/craton-co/cratonvm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/craton-co/cratonvm/releases/tag/v0.1.0
