# Binary Trees is 2–3.2x HotSpot (was 7.4x at depth 20) — where the time actually goes

**Characterised 2026-08-06, re-measured 2026-09-23. Levers taken 2026-09-23/24:
five single-pass ones, the 8-byte header (`Node` is 24 bytes like HotSpot's),
and both methods moved into the optimizing tier with bounded recursive
inlining. On the quiet Azure host, against the pre-header base, that cuts the
gap over HotSpot by 57% at depth 16, 61% at 18 and 48% at 20: 2.19x, 1.96x
and 3.21x (see "Both methods in the optimizing tier"). Not fixed.** This page
exists so the next attempt starts from measurements instead of from the same
four hypotheses.

## Current numbers (2026-09-23)

The README / `BENCHMARK.md` rows, `BinTreesClassic`, G1 on both sides,
`-Xmx8g`, one fresh process per measurement:

| depth | HotSpot 25 C2 | CratonVM | ratio |
|---|---:|---:|---:|
| 16 | 49 ms | 259 ms | 5.29x |
| 18 | 183 ms | 1,195 ms | 6.53x |
| 20 | 898 ms | 6,649 ms | 7.40x |

Everything below the "History" heading was measured on 2026-08-06 at depth 18
under the then-default collector, when the row read 1,700 ms against 176 ms
(9.66x). Its attributions still hold in shape; its absolute numbers do not.

## 2026-09-23: where the time goes now

`perf record` (cpu-clock; the Azure host exposes no hardware counters) with
`CRATONVM_JIT_PERF_MAP=1`, depth 18, G1: the mutator dominates.
`bottomUpTree` (single-pass tier) is 35–44 % of samples and `itemCheck` (IR
tier) 20–31 %.

The biggest lever is still **shadow-stack publication**, and it is not where
the 2026-08-06 section put it. One binary, d18, G1, 7 interleaved reps each at
host load 3–4 (`attr3`):

| arm | median | vs default |
|---|---:|---:|
| default | 1,238 ms | 1.00x |
| single-pass publication off (`CRATONVM_JIT_MY_SHADOW_EMISSION=0`) | 1,232 ms | 1.00x |
| IR publication off (`CRATONVM_JIT_IR_RELOC_EMIT=0`) | 1,001 ms | **1.24x** |
| both off | 846 ms | **1.46x** |

These are ceilings, not options: turning publication off withdraws the precise
roots. What they say is that the IR tier's publication — `itemCheck`'s push of
its live references before each self-call and the copy-back after it — costs
about a fifth of the whole run, and that the single-pass half only shows once
the IR half is gone.

### The IR frame block (landed 2026-09-23)

Per-call publication pushes every live reference's *value* before each call and
copies it back after, so a recursion pays a push and a reload per reference per
call. The IR tier's frames make a cheaper form possible, because every reference
the tier ever names is a fixed frame word: the reference-parameter homes plus
the planned slot of each `Ref` node, and the slot colouring never lets a
primitive into a reference colour.

So an IR method now publishes the **addresses** of those words once per
activation, as tagged indirect entries (`addr | 1`) — the *frame block*. The
first safepoint that names a reference pushes it; the saved-base word doubles as
the "already published" flag; every exit retracts it. The collector resolves an
indirect entry to the word it names and rewrites *that word*
(`ShadowStack::resolve_entry` / `remap`), so there is nothing to reload after a
call. `CRATONVM_JIT_IR_SHADOW_FRAME_BLOCK=0` restores per-call publication.

The pieces that make it sound, each pinned in code:

* **Nothing unzeroed is ever published.** The prologue zeroes every word the
  block names, without the entry-block refinement per-call publication uses
  (that argument — "a safepoint before the definition does not publish it" —
  does not hold for a block published whole). The OSR entry stub does the same.
* **A map outside the block refuses the compile** rather than mixing the two
  protocols over one saved-base word.
* **One rewriter per word.** `conservative_roots::remap_one_jit_frame` leaves a
  frame whose block is published to the shadow-stack remap; rewriting it in
  both places would take an already-moved address as a fresh key.
* **Every exit retracts the block — including the guard-deopt stub.** That stub
  historically skipped the `top` restore, correctly for per-call publication
  (a guard is never inside a call, so nothing is outstanding). A block *is*
  outstanding from the first safepoint to the return, and a leaked block names
  words in a frame that is dead once the stub returns.

Measured against the README's row definition — G1 on both sides, `-Xmx8g`,
HotSpot 25.0.4 C2 as the reference — with one binary pair (`41969f50e` vs the
same plus this change), interleaved, pinned to one core, in quiet windows
(load 2–3); medians, 7 reps at d16/d18 and 9 at d20:

| depth | HotSpot | before | after | speedup | ratio before → after |
|---|---:|---:|---:|---:|---:|
| 16 | 51 ms | 290 ms | **254 ms** | 1.14x | 5.69x → **4.98x** |
| 18 | 201 ms | 1,338 ms | **1,155 ms** | 1.16x | 6.66x → **5.75x** |
| 20 | 861 ms | 6,833 ms | **6,345 ms** | 1.08x | 7.94x → **7.37x** |

At d20 all 9 pairs favour the change and the ranges are disjoint
(after ≤ 6,377 ms, before ≥ 6,808 ms). The win shrinks with depth because d20
spends proportionally more in the collector, which this does not touch. An
earlier d16/d18 series on the default collector (ZGC) agreed: 296 → 263 and
1,312 → 1,179 ms. All checksums identical on every run. Of the 1.24x IR
ceiling above, the block keeps roughly half: it still pays the thread fetch,
the per-activation publication, and the entry-watermark capture and restore.

### What validated it — and what could not

**Bintrees cannot validate this change.** The obvious oracle — squeeze the heap
until it collects constantly, diff the checksum — is green on the fix (54/54
across ZGC, G1 and Generational at 128m/256m/512m, depths 14–18), and it is
*also* green on a positive control built with `ShadowStack::remap` sabotaged to
skip indirect entries (27/27). Two independent reasons, both measured:

1. In the shipped configuration no collector moves an object a live JIT frame
   references. ZGC reports `relocation_skipped_jit` equal to `collections`; G1
   reports root coverage incomplete on every pause and pins; Generational ran
   **0 moving cycles out of 52** on d16 at 128m (`nonmoving-coverage-incomplete`,
   `unregistered-jit-frame-on-stack`). The `minor=N` count in `gc-stats` is not
   a moving-cycle count. The non-moving sweep's selective promotion does
   evacuate shadow-published objects, but the conservative JIT-frame scan pins
   the same words first.
2. Even with moving forced, bintrees' IR method never holds a reference across
   a collection: `itemCheck` allocates nothing, and the methods that allocate
   (`bottomUpTree`) never reach the IR tier.

The workload that does see it is an IR-tier method that allocates nothing
itself and keeps reference parameters across a call whose callee allocates,
comparing its homes after the call against statics the collector rewrites
independently — `tools/bench/FrameBlockRemapProbe.java`. Under precise roots only (`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1
CRATONVM_DBG_FORCE_MOVING=1`, Generational, `-Xmx64m`, 171 moving cycles):

| binary | result |
|---|---|
| before | correct, 2/2 |
| after | correct, 5/5 (plus 9/9 on ZGC, G1 and Generational without the flags) |
| after, `remap` skipping indirect entries | **SIGSEGV, 5/5** |

So the indirect remap is load-bearing exactly where it has to be, and it works.
Neither flag is shippable; they are what makes the precise roots the only
thing standing between the frame and a stale home.

### The single-pass half: frame homes by address (landed 2026-09-24)

`bottomUpTree` stays in the single-pass tier (the IR tier declines methods
that allocate), and every recursive call site ran the per-call protocol in its
original form. At each of the two self-calls of a non-leaf node:

```text
~8   blind GPR spill          (safepoint.rs, unchanged)
~15  shadow push              the half-built Node, twice (new + dup)
 1   CALL
~17  shadow reload            validate savebase, copy both words back, pop
```

Two single-binary ceilings on the IR-frame-block build (d18, G1, noisy host):
removing single-pass publication outright (`CRATONVM_SHADOW_NOPUSH=1`) was
worth ~1.19x, and removing only the reload's copy-back (`CRATONVM_SHADOW_PIN=1`,
which pops without restoring) ~1.09x. `CRATONVM_JIT_MY_SHADOW_EMISSION=0` is
NOT a single-pass ceiling: it also turns the IR frame block off, and the two
effects cancel.

The block does not port — a single-pass operand-stack slot is an `int` in one
place and a reference in the next (`[rbp-58h]` holds `depth - 1`, then the left
child) — but the copy-back does not need a block. A frame-slot home is now
pushed as its **address** (`lea rax,[rbp-off+1]`), for the same one call as
before, and the reload only pops: the collector already rewrote the slot. The
entry's lifetime is unchanged, so none of the block's zeroing invariants are
needed. Register homes keep the value form. `bottomUpTree` goes from 4,671 to
4,549 bytes. `CRATONVM_JIT_MY_SHADOW_INDIRECT=0` restores the old bytes.

Measured on one binary, the switch as the A/B, G1 `-Xmx8g` both sides,
interleaved, pinned, quiet window (load 2.2), 7 reps each, medians:

| depth | HotSpot | before | after | speedup | ratio after |
|---|---:|---:|---:|---:|---:|
| 16 | 47 ms | 237 ms | 228 ms | 1.04x | 4.85x |
| 18 | 180 ms | 1,060 ms | **1,016 ms** | 1.04x | **5.64x** |
| 20 | 832 ms | 5,899 ms | **5,739 ms** | 1.03x | **6.90x** |

Ranges are disjoint at d18 (after ≤ 1,029, before ≥ 1,050) and d20 (after ≤
5,821, before ≥ 5,830), overlapping at d16. Smaller than the noisy `PIN`
estimate: the copy-back was two loads and two stores per call, and the
validation around it — which stays, because it guards the pop — was most of
the reload. Against the README row the gap is now 4.85x / 5.64x / 6.90x
(was 5.29x / 6.53x / 7.40x); the host ran ~7 % faster than the README's
night on both columns, so read the ratios, not the milliseconds.

With a second kind of address entry, the "one rewriter per word" rule moved to
where it belongs: `remap_active_jit_frames` takes the thread's
`ShadowStack::indirect_slots()` and skips every oop-map slot an entry names,
which replaced the IR-only "published block owns its homes" check.

Positive control, same setup as the frame block's (precise roots only, forced
moving, Generational, `-Xmx64m`, 53 moving cycles), on `BinTreesSized 16`
itself this time — `bottomUpTree`'s homes are exactly what is published by
address:

| binary | result |
|---|---|
| this change | correct, 5/5 |
| `remap` skipping indirect entries | **SIGSEGV, 5/5** |
| same sabotaged binary, both address forms switched off | correct, 5/5 |

The third row pins the cause: nothing but the address entries depends on the
sabotaged path. (`BlockProbe`, whose `walk` is single-pass, crashes 1 in 10
under these flags with the change on or off, in a VM `memmove` into a
decommitted heap span — pre-existing and unrelated, which is why the flags are
diagnostic only.)

### Constructor stores that start from null (landed 2026-09-24)

Each `Node` field store in the inlined constructor ran the single-pass
tier's *general* inline arm under G1, about 30 instructions:

```text
cmp [layout epoch],0 ; jne          test rax,rax ; je      and rcx,7 ; jne
mov rdx,<read bounds> + 6 x (cmp rax,[rdx+n] ; jb/jae)     ; three regions
test byte [rax+0Fh],4 ; je          ; compact layout
mov rcx,[rax+10h] ; test ; jne      ; old value must be null (SATB)
mov ecx,[rax+4] ; cmp ; jae         ; slot in bounds
mov [rax+10h],rdx                   ; the store
G1 filter: test rdx ; je ; sub ; sub ; xor ; and ; je      ; same region?
```

That was the arm for *every* store, leaves included, where `left = null` and
`right = null` write null over null. A cheaper constructor arm already existed,
but it was gated on the generational store table, which G1 never publishes.

The page used to say the receiver "is the object this frame just
bump-allocated", which is only true of leaves. javac emits `new Node` *before*
the two recursive calls, so an internal node's receiver has survived a whole
subtree of allocation, and possibly a collection, by the time its constructor
runs. What does hold is a verifier fact. In a method that is not itself a
constructor, the receiver of `invokespecial <init>` is typed
`uninitialized(offset)`, and nothing can write a field of an uninitialized
object. So when the constructor's own body is straight-line up to the store,
through `this`, with no call it could not prove empty, the field is still
null. `licm::inline_ctor_store_starts_from_null` is that proof, and
`spliced_ctor_receiver_is_a_new` is the caller half: not nested, and the
compiled method is not `<init>`.

It buys two things (`CRATONVM_JIT_CTOR_STORE_FROM_NULL`, default on, `0`
restores the old bytes):

* **a null value skips the store entirely**: `mov rdx,[val] ; test ; jz`,
  three instructions against ~30, on every collector;
* **under G1, a non-null value takes the constructor arm**, which has no
  old-value or bounds test, with a null test instead of the six-bound
  containment check. That check exists so the header read cannot fault, and a
  `new` result cannot fault. The G1 post barrier is still emitted: a young
  receiver in a pinned region still needs its remembered-set edge.

Engagement, d16, G1, `CRATONVM_DBG_SP_REF_STORE_TRACE=1`: flag on, 14.9M
stores take the constructor arm and the other 15.1M (the leaves' nulls) are
skipped; flag off, all 30.0M take the general arm.

Measured on one binary, the switch as the A/B, G1 `-Xmx8g` both sides,
interleaved, pinned, medians (d16/d18: 7 reps at load 1.2, repeated with 9 reps
at 0.5–2.6 to the same result; d20: the first pass at load 1.1, since the
second ran at 3.3):

| depth | HotSpot | before | after | speedup | ratio after |
|---|---:|---:|---:|---:|---:|
| 16 | 47 ms | 226 ms | 219 ms | 1.03x | 4.66x |
| 18 | 179 ms | 1,010 ms | **955 ms** | 1.06x | **5.34x** |
| 20 | 822 ms | 4,906 ms | **4,688 ms** | 1.05x | **5.70x** |

The d18 ranges are disjoint in both passes. At d20, six of the seven "after"
runs are below every "before" run. The d16 ranges overlap. The "before"
column is much faster than the previous section's "after" (d20: 4,906 against
5,739 ms). The binary is 57 dev commits newer, most of them collector work, and
the host was quieter, so the ratio column credits more than this change. Its
own share is the speedup column.

Correctness. `tools/bench/CtorStoreProbe` covers the proof's shapes and its
refusals: `other.left = null` through a receiver that is not `this`, and a
second store to a field. It was correct on ZGC, G1 and Generational at 16m and
256m, with the flag on and off. A build whose predicate accepts every first
store in a constructor got ~2 wrong results per iteration (`bad=3,998,8xx` of
2M) on all three collectors with the flag on, and none with it off.
`BinTreesSized` stress: 24/24 checksums on three collectors × 64m/512m ×
d14/d16. The regression suite passed 104/105; the one failure,
`RFileTimes`, fails the same way without this change.

### Allocation sinking (landed 2026-09-24)

javac compiles `new Node(bottomUpTree(d-1), bottomUpTree(d-1))` as
`new Node; dup; <call>; <call>; invokespecial <init>`, so the half-built node is
a live reference across both recursive calls: each call published it (twice,
`new` and `dup`) and reloaded it. Nothing can observe an uninitialized object
between `new` and its `<init>` (JVMS §4.10.1.9), so the single-pass tier now
allocates at the `invokespecial` instead (`op_object.rs`, "Allocation
sinking"):

* `new; dup` pushes two zeroed, unmarked placeholders. Before each later
  instruction the walk allocates for real unless the instruction is a plain
  push, `int` arithmetic, or this method's own recursive call that leaves the
  placeholders alone. The first instruction outside that set, in practice the
  `invokespecial`, allocates exactly as `new` would have into both slots.
  Sinking starts only when a recursive call would be passed (`new C()` and
  `new C(null, null)` are untouched), never in a try range or under precise
  exception frames, and only for classes needing no typed-zero initialisation
  or finalizer registration.
* The recursive call's deopt snapshot, taken while the allocation is pending,
  describes the placeholders as a virtual object with every field `Undefined`
  plus a second reference to it. The VM materialiser now leaves an `Undefined`
  virtual-object field at the fresh shell's default instead of storing
  `Int(0)` into it, so a resume rebuilds exactly the uninitialized object the
  interpreter's `new` makes.

That alone was worth ~1% (d18 957 → 945 ms, ranges overlapping). The
disassembly showed why: the two subtrees were now live at the allocation, and
its safepoint pushed them before the inline TLAB bump and reloaded them after
the merge, on the fast path, which makes no call. The cost had moved, not gone.
So when the allocation's blind register spill is sunk onto the slow edge
(which already guarantees a call-free fast path), the shadow push is too: the
slow edge emits the push, the `new_object` call, and the oop map and reload at
that call's return address. This applies to every such `new` with live
references, not just sunk ones. Both are behind `CRATONVM_JIT_ALLOC_SINK`
(default on, `0` restores the old bytes).

`bottomUpTree` goes from 4,151 to 3,775 bytes. Its first recursive call
publishes nothing and its second one entry, where they published two and
three. Measured on one binary, the switch as the A/B, G1 `-Xmx8g`,
interleaved, pinned, medians (d16/d18: 9 reps at load 1.9; d20: 9 reps at
2.0, rerun because the first pass ran into load 2.6):

| depth | HotSpot | before | after | speedup | ratio after |
|---|---:|---:|---:|---:|---:|
| 16 | 47 ms | 215 ms | 202 ms | 1.06x | 4.30x |
| 18 | 179 ms | 956 ms | **909 ms** | 1.05x | **5.08x** |
| 20 | 834 ms | 4,654 ms | **4,492 ms** | 1.04x | **5.39x** |

The d18 ranges are disjoint (after ≤ 919, before ≥ 950). At d20, eight of nine
"after" runs (4,450–4,507) are below every "before" run (4,643–4,669); the
ninth was a 6,487 ms outlier. At d16 the ranges overlap.

Correctness. `tools/bench/SinkProbe` covers four shapes: bintrees; a sunk node
held on the operand stack across a later allocating call; an argument call that
throws with the allocation pending; and a recursion that overflows the stack
with it pending. It was correct on ZGC, G1 and Generational at 16m and 256m
with the switch on and off, and under precise roots only with forced moving
(Generational, `-Xmx64m`); so was `BinTreesSized 16`. Two sabotaged builds,
same flags:

| binary | SinkProbe | bintrees d16 |
|---|---|---|
| this change | correct 3/3 | correct 3/3 |
| materialised slots not marked as references | **SIGSEGV 3/3** (correct 3/3 with the switch off) | — |
| slow edge drops the shadow push | wrong 1/3 (correct 3/3 with the switch off) | **wrong checksum 3/3** |

Stress 24/24 and the regression suite 104/105 (`RFileTimes`, as before).

Not this change: under those diagnostic flags with four VMs contending for the
CPU, `BinTreesSized 16` crashes ~20–45% of runs with "fault addr is inside a
RECENTLY DECOMMITTED heap span". It does so on a build without either of
today's levers and with both switches off. A build on the older `4414dcd1a`
base did not crash (0/24), so it is collector work that landed in between.

### The 8-byte header (landed 2026-09-24)

Lever 3 below, taken. Every object now starts with ONE 8-byte header word,
class id @0 and a 32-bit mark word @4, and a compact instance (everything
with a registered compact layout, which includes `Node`) has nothing else:
its fields start at 8. Arrays and legacy instances keep a second word (shape
@8, identity hash @12), so their data offset (16) and every array site is
unchanged. `docs/architecture/compact-object-and-field-layout.md` has the
contract. What had to move out of the header to make room:

* **the field count**: a compact instance's comes from its class's current
  layout (`compact_field_count`). That is only sound when the layout cannot
  change under live instances, so compatibility-stub classes and their
  descendants never get a compact layout.
* **the identity hash**: 20 bits in the NEUTRAL mark word (plus 11 class-id
  bits on top). A hashed compact object that is then locked inflates, and the
  hash moves into the monitor.
* **the thin-lock owner**: an 11-bit per-VM lock slot, leased per thread
  (`LockSlots`), and a 3-bit recursion count. Past either, the lock inflates.
* **the inflated monitor pointer**: monitors are found by object address in
  the sharded monitor index, and every moving collector re-keys it.
* **the forwarding pointer**: the object's second word
  (`FORWARDING_TARGET_OFFSET = 8`). Parallel evacuators claim with one CAS to
  `FORWARDED | BUSY` and publish after writing the target. A self-forward
  writes only the mark word. The sliding old-generation compactor uses a side
  map, because it reads live bodies while moves are pending.

Every compact field displacement in `CompactLayout` is now ABSOLUTE
(`field_disps`, `ref_disps`, `total_size`, renamed from the relative
`field_offsets` / `ref_offsets` / `body_size` so the compiler found every
consumer). The inline allocators store the class id and one mark dword, and
the JIT's plain-object guards became `TEST BYTE [r+6],3` so hash bits in byte 6
cannot fail them.

`CRATONVM_DBG_LAYOUT=1` on the same program:

```
before: [layout] BinTreesClassic$Node cid=526 body=16 refs=2 fields=2   (16 + 16 = 32)
after:  [layout] BinTreesClassic$Node cid=526 total=24 refs=2 fields=2
```

Measured on the Windows development host (8 cores, an IDE and other sessions
running, so noisier than the Azure numbers above and not comparable to them),
fat-LTO release against the fat-LTO base (`fc68d6dc1`), G1 `-Xmx8g`,
interleaved:

| depth | reps | base median | 8-byte header median | speedup |
|---|---:|---:|---:|---:|
| 16 | 9 | 643 ms | 625 ms | 1.03x |
| 18 | 7 | 3,145 ms | 2,935 ms | **1.07x** |
| 20 | 4–5 | ~16.2 s | ~16.7 s | noise (15.2–25.9 s spread) |

At d18 five of the seven new runs (2,892–2,943) are below every base run but
one. d20 on this host does not resolve, and one base d20 run died with no
output. Re-measure on the Azure host before quoting a ratio. Correctness: a
header probe (identity hashes before/after locking, nested and contended
locks, wait/notify, `IdentityHashMap` across GCs, exceptions, bintrees) gives
output identical to the base binary on G1, ZGC and Serial at `-Xmx64m`.
Bintrees checksums match. The regression suite is 104/105 (`RFileTimes`, as
before). `RMapGcStress` needs `TIMEOUT=600`, as its harness note says, and then
passes along with `RLockedIdentityHash` and `RMapResizeGc`.

### Both methods in the optimizing tier, recursively inlined (landed 2026-09-24)

> **2026-09-25, merge with JIT round 11: the recursive half is lost for now.**
> Round 11 had turned `ir-recursive-inline` on for `CratonBench fib` with a
> narrower admission (no implied multi-return splicing; a self copy must be
> trap-free), and the merge kept it. On the merged binary `itemCheck` splices
> nothing and `bottomUpTree` one level, and depth 18 runs the same with the
> switch on or off (medians of 5: 1,190 vs 1,177 ms, this Windows host). Work
> item: `docs/internal/fixed-bugs/r11w15-orch-recursive-inline-admission-drops-the-binary-trees-gain-FIXED-20260925.md`.


**The finding that changed the direction:** for these two methods the
optimizing (IR) tier's code was slower than the single-pass tier's. `perf`
plus the `CRATONVM_DBG_JIT_DISASM` listing from the same run, so every sample
lands on an instruction (the method: record the samples' IPs, then bucket them
by the listing's entry addresses):

* `itemCheck` (IR) publishes its **frame block once per activation**, about
  6% of all samples. The block's "already published" word is a frame slot,
  zeroed in every prologue, so a recursion republishes it in every frame. On
  top of that it pays prologue zeroing of every block home and the six-compare
  containment guard on each reference `getfield`. The single-pass `itemCheck`
  has none of these.
* Forced back to the single-pass tier, `itemCheck` alone took d18 from 923 to
  812 ms.
* `bottomUpTree` could not enter the IR tier at all: the builder refused the
  inlined constructor's field store (`splice_store_is_local`, because the
  receiver was allocated outside the splice). Once admitted it was slower
  there, 263 against 208 ms at d16. Every allocation called
  `jit_post_tlab_init`, every reference store called `jit_putfield_object`
  under G1, and the leaf `new` published the frame block.

Per activation the IR tier is expensive, and nothing amortised it. **Bounded
self-recursive inlining** (`CRATONVM_JIT_IR_RECURSIVE_INLINE`, two spliced
copies, HotSpot's `MaxRecursiveInlineLevel` plus one) does: a full subtree of
seven nodes becomes one frame. It existed, default off, and neither method
could take it. What had to change (all in `jit/`, each new behaviour behind a
default-on switch):

| change | what it removes |
|---|---|
| `splice_store_is_ctor_receiver` | the refusal of an inlined constructor's stores into an `Op::New` receiver. A spliced `new`'s taint (from storing a call result into it) no longer blocks the constructor's next store either. |
| `phi_home_droppable` requires a register | the home of a phi with NO register was dropped. The phi then had no location, and `emit_phi_copies` refused the compile: `itemCheck` under recursion |
| dropped self-copy published via `gp_load_value` | the same refusal, for a phi whose home slot is its (carried) source's |
| `CRATONVM_JIT_IR_TLAB_SKIP_POST_INIT` | the post-init CALL after every inline `new` of a class that needs none (the single-pass `TlabPostInit::Skip`) |
| `CRATONVM_JIT_IR_G1_REF_STORE` | the helper CALL per reference store under G1. It is now inline when the receiver is compact, the old value null, and the new one null or in the same region (F-08's filter) |
| `CRATONVM_JIT_IR_INITIAL_ZERO_STORES` | `new Node(null, null)`'s two stores into a fresh compact object |
| `CRATONVM_JIT_IR_ALLOC_MAP_SINK` | the frame-block publication on an inline `new`'s fast path, which cannot collect. It is now on the slow path only. |
| `CRATONVM_JIT_IR_LOAD_CSE_DOMINANCE` | under the still default-off `CRATONVM_JIT_IR_LOAD_CSE`, a second `n.left` read in a dominated block |

Measured on Azure, G1 `-Xmx8g`, interleaved, pinned, at host load around 10
(another session's VMs were running; the ratios hold, the milliseconds are
inflated). Medians of 5, all checksums identical:

| depth | 8-byte-header build | both IR + recursion | + load CSE |
|---|---:|---:|---:|
| 16 | 295 ms | 202 ms (1.46x) | 188 ms (1.57x) |
| 18 | 1,328 ms | 865 ms (1.54x) | 829 ms (1.60x) |

Re-measured on a quiet host (load 0.0–1.1), the merged tip (`365051ac6`, this
work plus origin/dev), same method, 7 reps each, all checksums identical. The
gap is "excess over HotSpot": `(CVM - HotSpot)`, and the cut is against the
pre-header base.

| depth | HotSpot | pre-header base | 8-byte header | this work | + load CSE | gap cut |
|---|---:|---:|---:|---:|---:|---:|
| 16 | 59 ms | 221 ms (3.75x) | 206 ms | 129 ms (2.19x) | 122 ms | 57% (61% with CSE) |
| 18 | 280 ms | 971 ms (3.47x) | 916 ms | 549 ms (1.96x) | 530 ms | 61% (64%) |
| 20 | 920 ms | 4,805 ms (5.22x) | 5,583 ms | 2,949 ms (3.21x) | 2,861 ms | 48% (50%) |

**d20 and the header build.** On the 8-byte-header build alone, d20 was 16%
slower than the base, 5,583 against 4,805 ms. All the builds take ONE young
pause at 6,137 MB, and `-Xlog:gc` showed where the time went:

| build | survivors | pause |
|---|---:|---:|
| pre-header base | 66 MB | 453 ms |
| 8-byte header | 164 MB | 1,350 ms |
| 8-byte header + the IR-tier work, before the dev merge | 160 MB | 1,507 ms |
| this work, merged | 61 MB | 675 ms |

The extra ~100 MB is the **dead stretch tree** (`itemCheck(bottomUpTree(21))`,
about 4M nodes at 24 bytes). Two checks confirm it:
* a copy of the benchmark without the stretch tree survives 66–76 MB on
  every build;
* with the conservative JIT root scan disabled
  (`CRATONVM_DBG_NO_JIT_ROOT_SCAN=1`, diagnostic only), both header builds
  survive 48 MB.

So the conservative JIT scan keeps the stretch tree alive, but not through a
root. A temporary census of the pause followed each root's left chain,
printing its length and the root's provenance. On both the pre-merge build
(164 MB) and the merged one (61 MB) it finds `binaryTrees`' `longLived` local
at depth 20 and bottom-up subtrees at 12–18. No root has depth 21, so no root
names the stretch tree. The root sets are the same except for one thing:
before the merge the JIT pin set held **7 regions**, after it **none**. A
pinned young region is kept whole, dead objects included, and what those
objects reference is copied. The path from those regions to the stretch tree
was not isolated.

The base allocates 32-byte nodes, so its pause falls earlier in the run (at
depth 14 rather than 18). No commit in the merge touches
`vm/src/jit/conservative_roots.rs`. The likeliest candidate is `bcd201585`,
which rewrote OSR admission in `jit_bridge.rs` and so changed which frames are
on the stack at the pause. That makes the fix look incidental: see lever 5
below.

Correctness:
* `BinTreesSized` d14/d16 on G1, ZGC and Generational at 64, 128 and 256 MB:
  18/18.
* `SinkProbe` and `CtorStoreProbe` on the three collectors at 16 and 256 MB.
* The regression suite with recursive inlining on by default: jdk-only
  `SUITE=all` 148/150, `--compatible` core 105/106. The failures are
  `RFileTimes`, which fails the same way without these changes, and
  `RJdkSqlPackage`, a harness fault: the vector passes `--compatible`,
  which clashes with `--jdk-only`.
* On the merged tip: the stress set (the bintrees and probe rows above plus
  the forced-moving diagnostic runs) 39/39, `SinkProbe` G1 `-Xmx16m` 24/24,
  and the `types`, `gc`, `jit` and `vm` unit tests.

`SinkProbe` at G1 `-Xmx16m` crashed in `G1Collector::retire_forwards`: 6/24
on the first 8-byte-header build, 1–2/24 after the merge with dev, and 0/24
on the pre-header base. It was the header, not the JIT. The first suspect,
the self-forward mark restore replaying a collector-wide list without checking
the pause, was a real defect and is fixed (`c41978ef4`), but the crash
survived it. A retire that compares the header's target word with the pointer
map's recorded target then caught the cause:

```
[g1] retire: a forward's target word no longer names its recorded copy (#1):
  holder=0x7bc0a7300570 class_id=542 mark=0x1c000003
  header_target=0x3a7400718 recorded=0x7bc0a7400718
```

The target's upper dword was `3`, which is `MARK_FORWARDED` as a mark written
at `holder + 8 + 4`. Something evacuated `holder + 8`, and that was an
**interior root**: a conservative root from a JIT frame's saved-register
image pointing at a compact `Node`'s first field. With the 16-byte header an
interior address only rarely decoded as an object (a zeroed field could).
With the 8-byte one, the field's
halves read as a class id and a mark, and a heap pointer's upper half is a
clean NEUTRAL plain-object mark with a zero flags byte. It passed
`addr_is_followable_object` and was evacuated as an object.
`pinned_region_set_including_non_object_roots` now refuses a root whose word
carries no `GC_FLAG_HEADER` and pins its region, as it already did for roots
that cannot be rewritten. `retire_forwards` also restores from the recorded
target rather than the header word (`2341fc6f6`). With both, 40 runs gave
38 correct, 0 SIGSEGV and 0 target mismatches. The 2 remaining runs were the
`OutOfMemoryError` that dev's own binary also hits at 16 MB. The new rule
fired in 20 of the 40 runs, every time on a `deopt-saved-gpr-image` root in
`SinkProbe.thrown` / `SinkProbe.held`. The generational, ZGC and
old-generation paths decide object starts from an exact start map, not from
header bytes, so they never had this hole.

Under the diagnostic precise-roots-only, forced-moving flags, `CtorStoreProbe`
crashed in "a RECENTLY DECOMMITTED heap span". That crash predates this
work: a `cvm-ct1` binary from the morning, built before any of it, crashed the
same way. On the merged tip its three forced-moving runs passed, but three
runs cannot tell a fix from a rarer crash.

## Levers still on the table

Since the section above both methods run in the OPTIMIZING tier, so items
1 and 2 (single-pass levers) no longer apply to this benchmark as written; the
same questions now stand for the IR tier:

1. **The IR tier's per-activation cost.** Recursive inlining amortises it; it
   does not remove it. A frame-block body zeroes every block home in the
   prologue and publishes the whole block at its first safepoint. Publication
   that is lazy per home, or a block whose zeroing is folded into
   publication, would help every recursive method, not just this one.
2. **Reference `getfield` guards in the IR tier**: epoch guard, null test,
   the six-compare containment check (kept for ZGC's coloured words) and the
   compact test, on every `n.left` / `n.right`. Under a collector with no
   coloured pointers the containment check is dead weight, but JIT code can be
   shared across VMs in one process, so dropping it needs a process-wide "no
   ZGC heap exists" fact rather than the running VM's collector.
3. **`CRATONVM_JIT_IR_LOAD_CSE`** (default off, "while it is measured"): with
   the new dominance extension it is a further 3–5% on the quiet host (549 ->
   530 ms at d18, 2,949 -> 2,861 at d20).
4. **d20's pause, per object.** The merged build's one pause copies 61 MB
   (about 2.5M 24-byte nodes) in 675 ms, ~265 ns an object, where the base
   copied 66 MB of 32-byte nodes (2M) in 453 ms, ~220 ns. The per-object cost
   rose with the header. Among the costs HotSpot does not have, the pointer-map
   build was 4.6% of all samples and `retire_forwards` 2.4% (the
   compact-field-count lookup, 5%, was fixed in `e01202d60`). HotSpot's whole
   d20 run is 920 ms.
5. **JIT-pinned regions can hold a dead structure.** See "d20 and the header
   build" above. With 7 JIT-pinned regions, a 100 MB dead tree survived the
   pause and d20 lost about 900 ms. With none, it did not. The merge removed
   the pins without changing the scan, so the next stack layout can bring them
   back. First step: say which pinned region's object reaches the stretch
   tree. Build the census with a per-pinned-region walk on `df003056f` plus
   the interior-root fix. It reproduces on every run, at 164 MB.

Superseded, kept for the record: the single-pass items were (a) the rest of
single-pass publication, i.e. the push itself; (b) the rest of the putfield
sequence (epoch guard, compact test, G1 filter). Both still apply to any
recursive allocating method that stays in the single-pass tier.
3. ~~**The header**~~ **DONE 2026-09-24**: see "The 8-byte header" above.
   `Node` is 24 bytes, level with HotSpot. What is left of it is allocation
   traffic for arrays and legacy objects, which keep the 16-byte header.

## History: the 2026-08-06 characterisation (depth 18)

`CratonBench bintrees` (depth 18, ~68M `Node` allocations) was then the worst
row in the README table: 1,700 ms against HotSpot's 176 ms.

## It is not the collector

| `-Xmx` | time | collections |
|---|---:|---|
| 8g | 1,686 ms | 1 minor |
| 16g | 1,275 ms | **0** |
| 24g | 1,272 ms | **0** |

With zero collections it is still **7.2x** HotSpot. GC is a 24% tax at the
README's 8g, not the gap.

## Where the time is (perf, `-F 997`, `-Xmx16g`, mapped to instruction offsets)

Sample IPs were mapped back to method offsets by capturing
`CRATONVM_DBG_JIT_DISASM` in the same run and subtracting each method's entry.

| | share |
|---|---:|
| JIT-generated code | **78.0%** |
| kernel (`clear_page_erms`) | 10.6% |
| the VM binary (helpers) | 6.9% |
| libc (`memset`) | 3.7% |

`bottomUpTree` is 47.9% of all samples, `itemCheck` 27.4%.

## The hottest instructions are moving-young publication

The top offsets in `bottomUpTree` are the shadow-stack push before each
recursive call:

```
b32: mov r11,[r10+278h]    <- shadow top
b39: lea r11,[r11+18h]     <- reserve 3 slots        2.7%  (hottest in the run)
b40: cmp r11,[r10+280h]    <- bounds check
b54: mov [rbp-30h],r11
b5c: mov [r11],rax         <- push oop
b63: lea r11,[r11+8]                                 2.2%
```

Two independent measurements agree on the size of this: `CRATONVM_NO_MOVING_YOUNG=1`
is **11%** faster (1,124 vs 1,267 ms, 3 reps each), and the codegen shrinks from
**692 instructions to 498** — the ~194 difference being 23 shadow-top stores, 24
bounds checks, 10 pushes and 15 thread reloads.

**And the workload does not get what it pays for.** The one collection at 8g
reports:

```
[moving-young] fallback #1: reason=innermost-rbp-belongs-to-unguarded-callee
  — this young collection runs the NON-MOVING sweep (no compaction, free-list allocation)
```

So the 11% buys precise relocatable roots, and then the collector declines to
relocate. `chain_entry_rbp_is_foreign` already special-cases direct self-calls
(`returned_from_direct_self_call` matches `E8 rel32` targeting the entry), so
`bottomUpTree`→`bottomUpTree` is fine; what defeats it is `binaryTrees` calling
**two different** JIT methods, leaving the chain entry's `compiled_method`
unable to describe the innermost frame.

### FIXED 2026-08-06 — the frame says which method built it

`innermost_frame_method` replaces the boolean. "The entry cannot describe this
frame" is not "nothing can": the frame's own return address names the method,
provided the call was the direct form.

```text
[exact_rbp + 8] = ret_addr
[ret_addr - 5]  = E8 rel32
ret_addr + rel32 == callee.entry_ptr()      <- must be the ENTRY, not just inside
```

That callee describes the frame exactly, because the frame was built by the
prologue at `entry_ptr`. Indirect calls (the inline MIC/PIC cascade, the hashed
megamorphic stub) encode no target and stay foreign, so the failure direction is
still a non-moving sweep and never a frame walked with the wrong map. Measured
on the same binary with `CRATONVM_GC_NO_CALLEE_RESOLVE=1` as the control:

| `-Xmx` | collections | off | on |
|---|---:|---|---|
| 8g | 1 | `cycles=0 fallbacks=1` | **`cycles=1 fallbacks=0`** |
| 2g | 6 | `cycles=0 fallbacks=6` | **`cycles=6 fallbacks=0`** |
| 1g | 12 | `cycles=0 fallbacks=12` | **`cycles=12 fallbacks=0`** |
| 700m | 18 | `cycles=0 fallbacks=18` | **`cycles=18 fallbacks=0`** |

`young=MOVING reason=moving-jit-coverage-proven`. Every collection relocates;
none fall back.

**1.089x at 8g, 1.000x at 16g** — exactly as it should be, since 16g collects
zero times and a change to what happens *at* a collection cannot help a run that
never has one. Seven-phase checksums identical, and the bintrees checksum exact
through all 18 relocating collections at 700m.

That last point is worth stating precisely, because the sibling result on this
page is the opposite. A green bintrees run cannot validate the **spill sink**
(the positive control below stays green with the spill removed entirely). It
*can* validate this one: under relocation the innermost frame's oop map is used
to rewrite pointers, so a frame walked with the wrong method's map leaves real
oops stale — and `itemCheck` dereferences every node immediately afterwards.
18 relocating collections producing the exact checksum is evidence.

**One correction to this section's own claim.** "Unlocks the 11% already being
paid" is not what happened, and the 16g row shows it: the shadow push/reload is
still emitted and still executed at every safepoint. The 11% was being paid and
still is. What changed is that it now buys something — the collector uses the
precise roots instead of declining them, so the *collection* gets cheaper (a
Cheney copy of a nursery that is almost entirely garbage, in place of a
~410 ms non-moving sweep). Removing the 11% itself would mean giving up precise
roots, which is a different trade.

## The other structural half: objects are 2x

`Node{Node left, Node right}` allocates **48 bytes** — the JIT's inline TLAB
bump is `lea rax,[r11+30h]`, fields at 0x20/0x28. HotSpot's is 24.
`HEADER_SIZE = 32` (`types/src/heap_types.rs`) against HotSpot's 12–16.

That is 2x the memory traffic for the same program, and it is the direct cause
of the 10.6% `clear_page_erms` + 3.7% `memset`: ~3.3 GB of Node bytes get zeroed
by the kernel on fault and again by the TLAB refill.

### Correction: the pieces already exist, they were never composed

This page first said "`CompactHeader` exists (8-byte header) but pairs with
16-byte field slots, so `Node` would be 40 rather than 24 — it is not the answer
here." That is true of `CompactObjectHeap`'s *own* allocator
(`CompactHeader::SIZE + num_fields * COMPACT_SLOT_SIZE` = 8 + 32 = 40) and
misses the actual situation. There are **two independent compact mechanisms**,
and the live allocation path uses exactly one of them:

| | header | body (2 ref fields) | `Node` |
|---|---:|---:|---:|
| today, JIT inline TLAB | 32 | 16 | **48** |
| `CompactObjectHeap`'s allocator | 8 | 32 | 40 |
| HotSpot | 12 | 8 | **24** |
| **the two composed** | **8** | **16** | **24** |

The body is *already* compact on the live path: `cratonvm_types::class_layout`
packs reference fields at 8 bytes each, which is why `emit_inline_tlab_new`
emits `lea rax,[r11+30h]` (48 = 32 + 16) and not 32 + 2 x `SLOT_SIZE`(16) = 64.
**The header alone carries all 24 bytes of the gap.**

### ...but "just turn it on" is not available either

Checked before claiming it, and the claim did not survive. **The compact-header
path is entirely inert:**

* `CompactAllocator` is instantiated in exactly two places, both `#[test]`
  functions in `vm/src/vm.rs`, and it allocates out of its own `Vec<u8>` — it
  is a demonstration, not a heap. Its own doc says so.
* `config.use_compact_headers` has one non-test reader (`vm/src/vm_init.rs`),
  where it appends the **string** `-XX:+UseCompactObjectHeaders` to a reported
  flag list. It selects no allocator and reaches no allocation path.
* `VmHeap::get_compact_header` says it is "only correct once a backend" writes
  compact headers. None does.

So there is no switch. Composing the two means teaching the real
`GenerationalHeap`, the TLAB, `emit_inline_tlab_new`'s baked immediates, the GC
walker, `object_body_size` and every native that computes a field address about
an 8-byte header — and `HEADER_SIZE` is a `const` the JIT bakes into machine
code. That is the enormous blast radius, and it is why this stays a structural
item rather than a patch.

**The tractable first step is smaller and self-contained.** The 32 bytes are:

```
0..4   class_id        8..12  identity_hash_code   16..24  forwarding_ptr
4..8   kind/elem/age/flags    12..16 shape          24..32  mark_word
```

`forwarding_ptr` is 8 of those bytes — and it is redundant. The mark word
already encodes relocation itself: `MARK_FORWARDED = 0b11` with the target in
the upper 62 bits, complete with `forwarded_mark()`, `decode_forwarded()` and
`is_forwarded()` in `types/src/heap_types.rs`. Two forwarding mechanisms exist;
one is a whole field. Folding the field into the mark word takes `HEADER_SIZE`
32 → 24 and `Node` 48 → 40 with no new encoding to invent — about 148
`forwarding_ptr` references to migrate, all of them in GC-correctness code.

For the record, because the size has moved before and the history is easy to
misremember: `HEADER_SIZE` has never been 16. It was 32 at the open-source
commit, went to **40** when `mark_word` was appended for thin-lock monitors,
and came back to **32** in `d46e70521` (2026-07-24, "gc: compact object headers
and field storage") — the same commit that introduced the 8-byte
`CompactHeader` as a separate, default-off type.

## The JIT half: redundant stack traffic in inlined bodies

The single-pass body emits pairs like this around the inlined `Node.<init>`:

```
ca7: mov [rbp-68h],rax
cab: mov rax,[rbp-68h]     <- reload of the value already in rax
caf: mov [rbp-80h],rax
cb3: mov rax,[rbp-68h]     <- and the identical pair again
cb7: mov [rbp-80h],rax
```

There is already a mechanism that removes exactly this — the `slot_mirror`
reload elision in `x64/operand_stack.rs`, default ON. It did not fire because
`try_emit_inline_site` **blanket-suppressed it for the whole duration of an
inlined callee**, on the grounds that the callee's internal joins are invisible
to the position rule (the main loop only invalidates at OUTER-method branch
targets).

The callee's joins are not actually invisible: `try_emit_inline_body` already
computes `callee_branch_targets` for its own merge-point check. Invalidating
the mirror there — the same rule the main loop applies at an outer branch
target — lets the mechanism stay live across inlined bodies.
`perf/inline-slot-mirror-branchless-20260806` does exactly that, and **it is
not worth merging.** Measured, both arms built from the same base:

| | `bottomUpTree` |
|---|---|
| suppression on (dev) | 692 instructions, `len=3874` |
| suppression off | **690 instructions**, `len=3866` |

**Two instructions.** All seven phase checksums identical, so it is correct — it
just does almost nothing, while changing codegen in the path whose earlier
mirror defect made H2 open every database with a null `ACCESS_MODE_DATA` (see
the incident note on the main loop's second invalidation site). Two instructions
does not justify re-entering that. The branch is left unmerged on purpose.

It recovers so little because of the mirror's own rule: it requires the
**immediately preceding emitted instruction** to have touched the same slot
(exact buffer-position equality).

```
ca7: mov [rbp-68h],rax
cab: mov rax,[rbp-68h]     <- elided: -68h is the live mirror
caf: mov [rbp-80h],rax     <- mirror now describes -80h instead
cb3: mov rax,[rbp-68h]     <- NOT elided, and this is the common shape
cb7: mov [rbp-80h],rax
```

Removing the rest needs a real value tracker — "which slot does this register
currently hold", invalidated on writes to the slot, writes to the register,
calls and joins — not a one-entry position-equality mirror. That is a separate
piece of work in which **every emit site that writes a GPR has to be audited**,
which is precisely how the H2 bug happened.

## Where the 690 instructions actually go

```
register spill stores [rbp-2xx]   99
operand shuffling                 84
shadow push / reload              69
calls                             21
```

That is why a peephole cannot close this gap: the body is dominated by
machinery, not by shuffling.

**The largest inline item is the register spill.** The leaf-allocation path —
taken for every leaf `Node`, half of the 68M — runs a full GPR spill into the
reserved spill region *before* the inline TLAB bump:

```
15b: jg 0x3e8                  <- depth > 0 leaves for the recursive path
161: mov [rbp-8],r12           <- register-homed LOCAL flush
165: mov [rbp-260h],rax        <- 14 blind GPR stores, inline, on the fast path
...
1c0: mov [rbp-2C8h],r15
1c7: mov rax,4                 <- safepoint id (bci 4 = the leaf `new`)
1e8: jne 0x275                 <- TLAB-full guard; its slow path is the helper
1ee..270:                      <- inline TLAB bump
```

### Correction: the emitter

This page first attributed that run to `x64/deopt_stubs.rs:1345` and read its
comment ("the guard `JB` **reaches here** with every GPR still holding its
trapping-instant value") as evidence that the emitted layout contradicted its
own design. **That was wrong, and the arithmetic said so.** The deopt stub
spills 16 GPRs *and* 16 XMMs; this run is 14 stores with no `movq`, and
`-0x260 + 13*8 = -0x2C8` lands exactly on the last of fourteen. The emitter is
`x64/safepoint.rs`'s `safepoint_reg_spill_all` loop over the 14-entry
`ALL_SPILL_GPRS` — the SB-CRASH-04 / Keycloak-Gap-9 blind spill, default-on
since 2026-08-03 via `precise_reg_spill_disabled()`. The deopt stub is not on
this path at all. Count the stores before naming the emitter.

### What it costs: 1.178x

`CRATONVM_NO_PRECISE_REG_SPILL=1` removes the blind spill at *every* safepoint —
98 of `bottomUpTree`'s 692 instructions (692 → 594, `len=3874` → `3188`), which
is 7 safepoints x 14. Measured on `-Xmx16g`, 10 interleaved pairs with the order
flipped on alternate pairs, per-**process** user CPU because the host was at
load 25:

| | user CPU per run | median |
|---|---|---:|
| default | 2.20 2.18 2.18 2.18 2.23 2.08 2.12 2.20 2.13 2.12 | 2.18s |
| `NO_PRECISE_REG_SPILL=1` | 1.79 1.82 1.84 1.74 1.87 1.87 1.86 1.83 1.88 1.86 | **1.85s** |

**1.178x, ranges disjoint.** That is the ceiling for any work on this spill.

### What can be claimed from it, and what cannot

Not all of it. The spill exists so the conservative `[scanner_sp, entry_sp)`
walk can see an oop that lives only in a register when the collector stops the
world, and a collector only stops a thread at a safepoint. So the question at
each safepoint is: *can a collection actually be reached from here?*

* **Self-call safepoints — yes.** `bottomUpTree` and `itemCheck` are directly
  self-recursive, and a call collects. `can_elide_self_call_register_spill`
  exists for exactly this shape and correctly refuses both: `itemCheck(Node n)`
  has a reference local in a register, and `bottomUpTree`'s recursive site has
  the half-built `Node` live on the operand stack, so
  `collect_live_oop_homes()` is non-empty and moving-young needs the precise
  publication. Roughly two thirds of the executed spills are these, and they
  stay. (This also retires the "`CRATONVM_JIT_MY_SELFCALL_PROOF=0` is inert"
  observation below: the proof was never firing, and it should not.)
* **`new` safepoints — no.** Under `skip_post_init_helper` the inline-TLAB arm
  emits **no call at all** between the safepoint and the merge point: layout
  guard, cached-thread load, cursor bump, header stores, cursor commit, jump.
  Nothing there can collect, so all 14 stores are dead on the path that
  actually allocates. They are live only on the three slow-path edges, which
  converge on `new_object`.

`perf/alloc-spill-sink-20260806` sinks them: the three registers the fast path
clobbers (RAX, R10, R11) stay at the safepoint, the other eleven move to the
slow-path label, where their value is still their safepoint value precisely
because the fast path does not touch them. Same fourteen slots, same layout.
A partition test pins `ALLOC_FAST_PATH_CLOBBERS` and its complement against
`ALL_SPILL_GPRS`, because a gap there is not a slow benchmark but a live oop
the root scan never sees.

It also drops the inline `get_current_thread` fallback — a CALL clobbers the
whole caller-saved file, which would invalidate the eleven registers spilled
later, and `emit_prologue` writes that slot on every entry anyway, so a null
read means a genuinely non-Java thread and the fallback would have diverted
too.

### Measured (both arms built from the same base, `4cd9af346` vs `e089f1ec3`)

The codegen check is the one that can falsify the design, so it comes first.
Runs of consecutive blind-spill stores in `bottomUpTree`:

| safepoint | base | fix |
|---|---|---|
| 0xdf | 14 | 14 |
| **0x165** (`new`) | **14** | **3**, + 11 at 0x20f (slow path) |
| **0x3d3** (`new`) | **14** | **3**, + 11 at 0x47d (slow path) |
| 0x514 0x6d2 0x8ab 0xa86 | 14 each | 14 each |

Exactly the two allocation sites, exactly 3 + 11, exactly at the slow-path
labels. 692 → 682 instructions overall (the 22 reappear on the slow paths; the
net 10 is the dropped fallback at both sites).

| | fix | base | |
|---|---:|---:|---|
| `-Xmx16g` | 1.96s | 2.07s | **1.056x** |
| `-Xmx8g` | 2.82s | 3.02s | **1.071x** |

10 interleaved pairs each, order flipped on alternate pairs, per-process user
CPU at host load ~20. All seven phase checksums identical; `CRATONVM_GC_STATS`
identical on both arms (`minor=1`, same fallback reason, same counts); zero
`alloc-spill-sink-unconsumed` compile bails across the whole suite.

### What is NOT evidence here — the benchmark cannot see this class of bug

The obvious oracle is "squeeze the heap until it collects continuously and diff
the checksum". Done: `-Xmx2g/1g/700m` (6/12/18 minor collections), both young
lanes, 3 reps each — 18/18 correct on both arms.

**That result is worthless, and the positive control says so.** Running the
*base* binary with `CRATONVM_NO_PRECISE_REG_SPILL=1` — which removes the blind
spill at **every** safepoint, a far larger violation than this sink — is also
green, 12/12, both lanes, at 18 collections. The full bench at squeezed heaps
agrees: base, fix and no-spill-at-all produce identical checksums.

So no CratonBench phase holds a live oop only in a register at a safepoint; the
operand-stack and local flushes already cover everything, and the blind spill is
pure belt-over-braces *on this workload*. It exists for the ones where the oop
tracker misses something (SB-CRASH-04, Keycloak Gap 9), and only those can
validate a change to it.

The sink therefore rests on the structural argument plus the codegen check
above, not on a green benchmark — and on `CRATONVM_JIT_NO_ALLOC_SPILL_SINK=1`
being a one-variable rollback. **If you extend this to the call safepoints, find
a workload where the spill is load-bearing first, and prove it goes red without
it.** Anything else is measuring nothing.

## Things that were tried and are NOT the answer

- **Tiering.** `bottomUpTree` never reaches the optimizing tier at the default
  threshold. Giving it one (`CRATONVM_TIER_C2_THRESHOLD=1000`) changes the time
  by 1 ms (1,266 vs 1,267). `CRATONVM_JIT_FORCE_C2=1` is **25x slower**
  (31,630 ms). More C2 is not a direction here.
- **`CRATONVM_JIT_MY_SELFCALL_PROOF=0`** — inert. Same 43 spill stores, same
  `len=3874`. *Now explained, and it is a non-finding:* the self-call spill
  elision was never firing in either arm, because
  `can_elide_self_call_register_spill` correctly refuses both methods (see the
  section above). Turning off a proof that never succeeds changes nothing.
- **`CRATONVM_JIT_MY_SHADOW_EMISSION=0`** — 1%, and it was separately proven
  inert (byte-identical codegen) during the PERF-02 work. Do not read that 1%
  as a measurement of anything.
- **The duplicated TLAB memset in `g1.rs`** (fixed in `2386e8bd8`): real
  duplication, but **G1 is not the default backend** — `CRATONVM_GC_STATS` says
  `backend=generational`. The memset share did not move (1.80% → 2.21%).

## A third JIT item, not yet taken

The prologue fetches the thread pointer **twice** on an external entry and
still calls the helper once on a self-entry:

```
40: call rax    <- get_current_thread, for the SHADOW thread slot
...
84: mov r10,[rbp]              <- self-entry proven: inherit from caller frame
8b: mov rax,[r10-28h]          <- jit_thread_slot
92: mov [rbp-28h],rax
96: mov rax,[r10-18h]          <- stack_floor_slot
```

`emit_prologue`'s self-cache-inherit block copies `jit_thread_slot_off` and
`stack_floor_slot_off` out of the caller's same-layout frame when a direct
self-call is proven — but the shadow-stack fetch above it is unconditional and
wants the *same* `*mut JvmThread`. On a self-recursive method that publishes
(so the fetch is not NOP'd out), that CALL runs on every invocation —
68M times in `bottomUpTree`.

Not done here because the fetch's byte range is what
`maybe_nop_out_shadow_fetch` erases, so splitting it into inherited and fetched
paths means teaching the erase about both. Worth doing; it is prologue surgery,
not a peephole.

## If you pick this up

The two structural levers are large:

1. ~~**Make moving-young stop falling back** on multi-method JIT stacks.~~
   **DONE 2026-08-06** — see the section above. 1.089x at 8g; every collection
   now relocates. It does not remove the 11%, it makes the 11% buy something.
2. ~~**Shrink the header**~~ **DONE 2026-09-24** (see "The 8-byte header"). The end state is the
   8-byte `CompactHeader` on the path that already packs reference fields at 8
   bytes, which takes `Node` from 48 to 24 — level with HotSpot. That path is
   inert today and the migration is large. The bounded first step is folding
   `forwarding_ptr` into the mark word's existing `MARK_FORWARDED` encoding:
   32 → 24, `Node` 48 → 40, no new encoding required.

The JIT lead is now spent, and the arithmetic says so. The blind spill is the
whole of what the single-pass backend has to give here — 1.178x if it vanished
entirely — and only the allocation sites' share of it was reachable by a local
argument. That share has been taken (1.056–1.071x). The self-call two thirds
would need to prove that a callee's own blind spill covers its caller's
registers, which is true for callee-saved registers by ABI but false the moment
the callee reaches a Rust helper before its first safepoint — a whole-callee
property a single-pass compiler does not have.

So `9.66x → ~9.1x`, and the rest of it is the two structural items above. That
is the finding: a 9x row does not have a JIT-shaped fix.

Neither is a small change, and the per-call shadow-push cost is not removable
without giving up precise roots. A 9.66x row does not have a cheap fix; that is
the finding.
