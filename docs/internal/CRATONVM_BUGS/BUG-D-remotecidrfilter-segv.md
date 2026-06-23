# Bug D — `TestRemoteCIDRFilter` SIGSEGV (EXCEPTION_ACCESS_VIOLATION)

**Severity:** Medium (hard crash of one test class; distinct from the hang
clusters).
**Status on CratonVM:** CRASH (process aborts). **HotSpot:** PASS (2.4 s).
**Run date:** 2026-06-11
**Binary:** dev `c8f3bb3a` (+ Bug A); reproduced under the Tomcat suite run.

## Symptom

`org.apache.catalina.filters.TestRemoteCIDRFilter` aborts the VM mid-run:

```
INFO [...TestRemoteCIDRFilter] Starting test case [testAllowDenySetAsNull]
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF778F6D429
Registers:
  rip=0x00007FF778F6D429
```

HotSpot runs the same class to a clean PASS.

## Notes

- The crash fires while running `testAllowDenySetAsNull` — a `RemoteCIDRFilter`
  configured with null allow/deny sets. The filter parses CIDR/`NetMask`
  patterns, so the fault is likely in the request-filter / address-parsing path
  rather than the embedded server (this class does not need HTTP serving).
- The default crash dump carries no symbolized native frame or Java stack
  (`pc`/`rip` only). Pinning the faulting Rust frame needs a
  `strip="none"`/`debug="line-tables-only"` release-with-debug build plus
  `CRATONVM_SYMBOLIZE` on the captured RVA (see the crash-debug tooling).
- A SIGSEGV (not a timeout) is not a parallelism artifact — contention in this
  harness manifests as hangs, never as an access violation — so this is a real
  CratonVM memory-safety defect.

## Reproduction

```
cratonvm.exe -cp <tomcat-test-cp> org.junit.runner.JUnitCore \
  org.apache.catalina.filters.TestRemoteCIDRFilter
# EXCEPTION_ACCESS_VIOLATION during testAllowDenySetAsNull; HotSpot: OK (passes)
```

Minimal standalone repro (no Tomcat server, no Bug A/C dependency — reproduces on
**plain dev** as well as the tomcat-fixes binary): `apps/tomcat/.tooling/drv/`
holds `org/apache/catalina/filters/CidrDrv.java`, which constructs a
`RemoteCIDRFilter` with null allow/deny and runs the same 6144-iteration
`doFilter` loop the test does (each iteration builds a `MockHttpServletRequest`,
i.e. `new Connector()`):

```
cratonvm.exe -cp ".tooling/drv;<cp>" org.apache.catalina.filters.CidrDrv 256
# SIGSEGV after ~n=4000, preceded by gen_heap "inconsistent header" corruption.
```

## Root cause (2026-06-12) — JIT conservative-root young-GC heap corruption

This is **not** a CIDR/NetMask parsing fault. It is the
conservative-JIT-root / young-GC class (see `docs/precise-jit-stack-maps-*`,
the bintrees saga). Evidence, all on the default 8 GiB heap, `CidrDrv 256`:

| Configuration | Result |
|---|---|
| JIT on (baseline) | **SIGSEGV** + `gen_heap` header corruption — every run |
| `CRATONVM_DISABLE_JIT=1` | **clean** (`DRV DONE`, 0 corruption) — deterministic |
| `CRATONVM_JIT_NO_BCE=1` (bounds-check elim off) | still crashes |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | still crashes |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` | still crashes |
| `CRATONVM_DISABLE_UNROLL=1` | probabilistic (1 clean / 1 crash) — lowers odds, not the cause |
| `CRATONVM_PRECISE_JIT_MAPS=1` | corruption → `OutOfMemoryError` (precise maps incomplete) |
| `--Xmx 512m` (frequent GC) | does **not** reproduce |

Mechanism: the hot loop tiers up to JIT. A live object reference held only in a
JIT **register** (or a register-homed slot the spill path doesn't cover) at a
GC-capable safepoint is invisible to `conservative_roots::scan_active_jit_frames`,
which scans only the JIT frame's stack words (its load-bearing invariant — "every
live reference is in an 8-byte-aligned stack slot at a safepoint"). The missed
oop is reclaimed by the non-moving young sweep (the collector used while JIT
frames are live), its slot is reused by a later allocation, and a subsequent
write through the now-stale register pointer scribbles into the reused object —
producing the `"kind=Object but array_length set"` / desync corruption the
`gen_heap` walker reports, which a later access turns into
`EXCEPTION_ACCESS_VIOLATION`. The bug is **probabilistic** (depends on GC timing
vs. which oop is register-resident at the collection); register pressure (loop
unrolling) raises the odds; the large default heap is *required* because it lets
the loop reach steady-state JIT before the first young GC.

Note this also means the original "crash vs HotSpot-pass" framing is only half
the story: even with the SIGSEGV fixed, the test cannot *pass* on CratonVM — each
`new Connector()` costs ~28 ms in the interpreter (~500× HotSpot), so the
6144-iteration method needs ~170 s and blows any per-class timeout. Fixing the
SIGSEGV converts CRASH → (slow) clean run, not CRASH → PASS.

### Fix attempt that did NOT work (recorded so it isn't retried)

Hypothesis: operand-stack oops homed in a **callee-saved** register
(`StackSlot::CalleeSaved`) are left unspilled by `flush_scratch_registers`
(which handles only `Scratch`/`Xmm`). Patch: in `emit_pre_safepoint_spill`
(`jit/src/x64.rs`), additionally spill every register-homed live operand oop to
the frame. **Result: still crashes** (2/2 runs). The CalleeSaved operand homes
turn out to already alias spilled locals, so they were covered; the genuinely
missed root is elsewhere (an untracked codegen temporary, or a moving cycle in a
quiescence gap relocating a conservative root). Reverted.

### Symbolized fault (2026-06-12, `release-with-debug` build)

Faulting access: **read at `0x0000000C`** — i.e. `ObjectHeader.array_length`
(offset 12) read from a **near-null / clobbered header base**. Offline
`CRATONVM_SYMBOLIZE` of the captured RVAs against the line-tables binary:

```
exe+0x1DCD29  cratonvm_gc::gen_heap::gen_object_total_size            gen_heap.rs:5179
exe+0x1D2DF2  cratonvm_gc::gen_heap::collect_garbage_inner   +0x25E2  gen_heap.rs:2668
exe+0x87CBFF  cratonvm_vm::runtime::interpreter::maybe_gc             interpreter.rs:291
exe+0x867392  cratonvm_vm::runtime::interpreter::execute_instruction  interpreter.rs:7835
exe+0x83061B  ...execute_frame / execute / Vm::invoke / cratonvm::run
```

The `external/jit` frames in the raw backtrace are **stale stack residue** from
a just-returned JIT call, not the live chain. The live chain is: the
**interpreter** hits an allocation, calls `maybe_gc`, runs a **moving (Cheney)**
young collection, and faults in its post-copy promotion-stats loop
(`collect_garbage_inner:2659-2668`, `for new_addr in pointer_map.values() { …
gen_object_total_size(header) … }`) when it reads the header of a forwarded
object whose header is corrupt.

So the moving GC is the **victim**, not the culprit: the `gen_heap`
"inconsistent header / kind=Object but array_length set" warnings *precede* the
fatal fault. The heap was already corrupted by an earlier JIT-active **non-moving
sweep** cycle. `CRATONVM_DBG_CORRUPT_FRAMES` shows the mutator at first-corruption
detection deep in object construction:
`CidrDrv.main → MockHttpServletRequest.<init> → Request.<init> →
MappingData.<init> → MessageBytes.<init>` — all JIT-banned `<init>`s, so the
corruptor is a JIT-compiled helper they call on the hot allocate path
(`MessageBytes.newInstance` / `MessageBytes$MessageBytesFactory.newInstance` /
`Integer.valueOf` were all `upgrade-OK` in the `CRATONVM_DBG_JITC` log).

The exact corrupting JIT method could not be pinned to a single name: the bug is
probabilistic and the tiny-young-gen precision tool (`CRATONVM_DBG_GC_STRESS`)
diverts to the moving path and fails differently (startup `ServiceLoader`
break / rc=1) before it can dump frames adjacent to the corruptor.

### CORRECTION (2026-06-12): `SHADOW_STACK` only *masks* it — the root is a JIT-miscompiled WRITE

An earlier revision of this file claimed `CRATONVM_SHADOW_STACK` *fixed* Bug D
(CidrDrv ×5 clean). That conclusion was **wrong** — it masks, it doesn't fix.
The decisive experiment (`CRATONVM_SHADOW_MARK`, a one-off gen_heap patch that
feeds the *same* shadow precise roots into the **non-moving** sweep's marking
instead of switching to the moving collector):

| Configuration | roots | collector | CidrDrv | bt18 |
|---|---|---|---|---|
| `SHADOW_STACK=1` | precise (shadow) | **moving** | **clean** ×5 | **SIGSEGV** |
| `SHADOW_MARK=1` (+NORELOAD) | precise (shadow) | **non-moving** | **SIGSEGV** ×4 | 68332206 ✓ |
| `DBG_FORCE_MOVING=1` | conservative | moving | 2 clean / 1 crash | (n/a) |
| default | conservative | non-moving | SIGSEGV | 68332206 ✓ |

Precise roots + non-moving sweep **still crashes** (4/4). So the shadow roots do
**not** prevent the corruption — only the **moving collector's evacuation** does
(it copies live objects to to-space and discards the corrupted from-space each
cycle). That also explains the heap-size dependence: a small heap GCs often
enough that the (interpreter-triggered) moving cycles evacuate the damage before
it accumulates to a fatal walk. The corruption is therefore a **JIT-miscompiled
heap write that clobbers an object header**, not a missed GC root — consistent
with the failed `emit_pre_safepoint_spill` spill patch.

`SHADOW_STACK` is therefore neither a real fix (it hides a live bug) nor
default-safe (it routes JIT-active GC to the moving collector, whose shadow root
coverage is incomplete for OSR-heavy loops → it **crashes bt18**). Do not ship it
as the Bug-D fix.

Narrowing of the miscompiled write so far: it is JIT-emitted (`DISABLE_JIT` →
deterministically clean); it is **not** bounds-check elimination (`NO_BCE` still
crashes), **not** the inline-TLAB `new` path specifically (`DISABLE_INLINE_NEW`
still crashes — but `new_info.num_fields` feeds both the inline and slow `new`
paths, so a wrong field count remains a live suspect), and **not** scalar
replacement. Loop unrolling *raises* the odds (`DISABLE_UNROLL` lowers but does
not eliminate them). `CORRUPT_FRAMES` puts it on the
`Request.<init> → MappingData.<init> → MessageBytes.<init>` allocation path,
whose JIT-compiled helpers (`MessageBytes.newInstance` / factory,
`Integer.valueOf`) are the prime candidates.

### Bisection (2026-06-12, `CRATONVM_JIT_BISECT_SKIP`, default 8 GiB heap, CidrDrv 256)

The 18 methods this workload JIT-compiles were bisected. The corruptor is **not a
single method** — multiple "allocate-an-object-then-run-its-`<init>`" methods
each independently corrupt (every proper subset still crashes; skipping all 9
writer methods → 3/3 clean). The inverse "keep only X compiled" test isolates
individual corruptors:

| Test | Result | Conclusion |
|---|---|---|
| skip all 9 writers | 3/3 **clean** | the corruptor(s) are within these 9 |
| skip MessageBytes / BOX / ByteBuffer / lambda+fmt (subsets) | all crash | ≥2 corruptors, spread across subsets |
| **keep ONLY `java/nio/ByteBuffer.allocate`** | 2/3 **crash** | **ByteBuffer.allocate is a confirmed corruptor** |
| **keep ONLY `java/lang/Integer.valueOf`** | 3/3 **clean** | Integer.valueOf is NOT a corruptor |

Discriminator: `ByteBuffer.allocate` does `new HeapByteBuffer(cap,cap)` — a class
with a **deep hierarchy** (`Buffer`→`ByteBuffer`→`HeapByteBuffer`) whose `<init>`
itself allocates a 16 KiB `byte[]` (a GC-triggering allocation) **while the new
HeapByteBuffer is still live in the JIT frame** (dup'd for the `areturn`).
`Integer.valueOf` does `new Integer` — a trivial `<init>` with no nested
allocation — and never corrupts. The corrupted region in the crash dumps sat
adjacent to a 16424-byte (= 40 + 16384) array, i.e. exactly that `byte[]`.

Ruled out as the mechanism: **undersized allocation** — the JIT new-resolver
(`interpreter.rs:2662`) uses `class.num_total_fields`, the *same* count the clean
interpreter uses, so the object is correctly sized. Also not BCE
(`NO_BCE` crashes), not inline-TLAB-`new` (`DISABLE_INLINE_NEW` crashes), not
scalar replacement. And not a missed GC *mark* — `SHADOW_MARK` (precise shadow
roots fed to the non-moving sweep) still crashes 4/4, while the moving collector's
*evacuation* masks it. The signature points at a miscompiled **store** in the
`new X; dup; …; invokespecial <init>` sequence when `<init>` is a GC-capable
safepoint — most likely the dup'd return-value reference going stale across the
nested-allocation GC and a subsequent write landing in reused memory.

### Static step-debugger pass (2026-06-12, `CRATONVM_DBG_JIT_DISASM`)

Dumped `ByteBuffer.allocate`'s emitted x86-64 (`CRATONVM_DBG_JIT_DISASM=ByteBuffer.allocate`,
driver `.tooling/drv/BBAlloc.java`) and audited the allocation helpers. The
per-method codegen is **structurally correct**, which *rules out* a per-method
miscompile:

- The dup'd return reference is **spilled to a frame slot `[rbp-18h]`** across the
  `invokespecial <init>` call and reloaded after — so it is a visible conservative
  root at that safepoint, not register-only. (`Integer.valueOf` being clean is
  therefore NOT about a deeper-ctor register-spill gap.)
- `num_fields = 0x10 = 16` for `HeapByteBuffer` (class_id 0x18B) comes from
  `class.num_total_fields` (`interpreter.rs:2662`) — the *same* count the clean
  interpreter uses. **Verified not undersized**: a 60k-iteration `BBAlloc` run
  produced **zero** `set_field out-of-bounds … dropped` warnings, so `<init>`'s
  field writes fit in 16 slots. (HotSpot reports 11 instance fields;
  CratonVM's layout is 16 — internally consistent.)
- Every field/array store is **bounds-guarded** — JIT `jit_putfield_slot_in_bounds`
  (`helpers.rs:1512`), interpreter `gen_heap::set_field` (drops + logs OOB),
  `jit_init_primitive_fields` (routes through guarded `set_field`). An OOB store is
  dropped, never a header clobber.

So the clobber is an **unguarded write / allocation overlap**, not a field store —
consistent with the 8-byte heap-walk desync (`RE-SYNCED … skipped 8 bytes`) in the
crash dumps (an object computed the wrong *size*, so the next one overlaps). It is
in the shared `new X` + GC-capable-`<init>` + forced-GC interaction
(`jit_new_object` probes young and may run `maybe_gc_forced_pub` with the caller's
JIT frame live), not in `ByteBuffer.allocate`'s own bytes. Static disasm cannot go
further; the genuine next tool is a **runtime heap-write watchpoint** — trap writes
to the first clobbered header's address to catch the offending store in the act.

## Status

Confirmed a **multi-method JIT GC-interaction defect** (not a per-method codegen
error — the emitted code is correct and the allocation is correctly sized) in the
`new X; invokespecial <init>` path under a GC-capable constructor;
**`ByteBuffer.allocate` is a confirmed corruptor**, `Integer.valueOf` is not.
**Not fixed.** Static disasm ruled out the easy explanations; the remaining root
needs a runtime heap-write-watchpoint build (trap on the first clobbered header).
Verified **workaround**: skip-list the affected allocate-and-`<init>` methods
(`CRATONVM_JIT_BISECT_SKIP` / `vm/src/jit/skip_list.rs`; skipping all 9 → 3/3
clean). Deterministic mitigation: `CRATONVM_DISABLE_JIT=1`.
`CRATONVM_SHADOW_STACK=1` only masks it (and regresses bt18) — do not rely on it.

## RESOLVED (2026-06-12) — un-filled TLAB tails, NOT a JIT-miscompiled write

Branch `fix/bug-d-cidr-jit-gc` (worktree `C:/craton/CratonVM-cidr`). The whole
"JIT-miscompiled heap write" framing was a **misdiagnosis**. There is no bad
write: the corruptor is **un-walkable TLAB-tail memory** that the non-moving
young sweep's linear walk strides off-grid. Three holes, all in the
allocator/sweep, none in the JIT:

1. **Refill drops the old TLAB tail un-retired.** `tlab_alloc_object`
   (`vm/src/runtime/interpreter.rs`) replaced `thread.tlab` on a refill (the fast
   path returned `None` because the request didn't fit the *remaining tail*, not
   because the TLAB was empty) **without calling `retire()`**. That leftover tail
   — zeroed arena bytes inside young's live `[base, used)` — is neither a walkable
   object nor a free hole. The moving collector never notices (it traces live
   roots), but the **non-moving sweep** (used while JIT frames are live) walks
   young linearly, decodes the zeroed tail as a run of 40-byte all-zero "objects",
   and desyncs when the tail isn't a multiple of 40 → "implausible object size";
   a later moving GC then SIGSEGVs walking the wrecked heap. **Fix:** retire the
   outgoing TLAB before replacing it. This is why `ByteBuffer.allocate` (16424-byte
   `byte[]` + 16-field `HeapByteBuffer` rarely fit a partly-used tail → forced
   refill, big tail) corrupts but `Integer.valueOf` (tiny, fits any tail) doesn't.

2. **Sub-`HEADER_SIZE` tails can't hold a filler and were zeroed.**
   `install_tail_filler` (`gc/src/tlab.rs`) writes a walkable `int[]` only for
   tails >= 40 bytes; an 8/16/24/32-byte tail it just **zeroed** — but a zeroed
   sub-40 region is byte-identical to a live `new Object()` (class_id 0,
   num_slots 0; its only non-zero header word, identity_hash at offset 8, lies
   past an 8-byte gap), so the walk can't distinguish them and desyncs anyway.
   **Fix:** stamp a distinct `GAP_FILLER_CLASS_ID` sentinel (class_id at offset 0,
   exact gap length at offset 4) into the first 8 bytes.

3. **The sweep walkers must skip the sentinel.** All six young-from linear
   walkers in `gc/src/gen_heap.rs` (non-moving sweep, `clear_all_mark_bits`, the
   two young->old mark/fixup passes, and the two selective-promotion passes) now
   recognise `GAP_FILLER_CLASS_ID`, read the length, and stride over it *before*
   `gen_object_total_size` (whose offset-16 `num_slots` read would fall outside an
   8-byte gap). The span is left in place (reclaimed wholesale at the next Cheney
   reset) rather than free-listed — a sub-40 block can never satisfy an allocation,
   so free-listing it would only bloat the linear free-list scan.

Why every prior signal fit: JIT-off -> no JIT frames -> always the *moving*
collector -> masked (it never linear-walks from-space). `SHADOW_MARK` (precise
roots, non-moving) still crashed because the bug is not a missed root — it's
un-walkable memory in the linear walk, independent of root precision. Static
disasm found the codegen correct because the codegen *is* correct.

**Verification (release-with-debug):** CidrDrv 256 CRASH->clean, 3+ full
completions (`DRV DONE n=6144`), **0** corruption warnings (was 43+ then SIGSEGV);
`bintrees10/14/16/18` all = HotSpot checksums incl. **bt18 = 68332206**, 0
warnings; gc unit tests 663/666 (3 failures pre-existing in `reference.rs`,
untouched); regression pool **23/23 PASS, 0 regressions**. Per the note above the
test still can't beat a per-class *timeout* (interpreter `new Connector()` ~28 ms
x 6144 ~= 136 s) — CRASH->(slow) clean run, as expected.

Files: `vm/src/runtime/interpreter.rs` (retire-on-refill), `gc/src/tlab.rs`
(`GAP_FILLER_CLASS_ID` + sentinel install), `gc/src/gen_heap.rs` (sentinel skip in
6 walkers). The `CRATONVM_DISABLE_JIT` / skip-list mitigations are no longer needed.
