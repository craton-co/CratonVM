# ReflRepro GC corruption — ✅ FIXED 2026-06-23 (it was GC-side free-list accounting, NOT a register-resident JIT root)

> **✅ FIXED (commit `6e3ddb05`, branch `fix/jit-register-roots`, NOT pushed).** The corruption
> the title blamed on a "register-resident missed JIT root" was **never that**. ROOT CAUSE: the
> non-moving young sweep's free-block coalescer merged only **adjacent** free blocks; **overlapping**
> ones survived, and `Arena::alloc` (no overlap check) then **double-served** the same young region
> → two live objects at overlapping addresses → linear-walk desync → corruption. FIX: coalesce
> overlapping blocks too (`off <= last_end`, extend to max end). **`ReflRepro 8000 @
> GC_STRESS=65536` → `ok=8000 bad=0` (the verification bar, met); `bt16=14985902`, `bt18=68332206`
> golden.** Paired with a robust free-block skip (`f9138bf0`) that keeps any residual desync
> recoverable. The full investigation (register-root refutation → mechanism → breadcrumb → fix)
> is below, kept as the evidence trail.
>
> **TWO fixes land the result:** (1) coalesce **overlapping** free blocks (`6e3ddb05`) — fixes
> the data corruption (`bad=0`); (2) **clamp over-sized objects** that overstep a pre-existing
> free hole (`9d…`/next commit) — a corrupt over-sized header (the `et=Int` byte-array read)
> can't span a free hole, so the sweep retains-not-frees it and re-syncs at the hole instead of
> over-freeing into a neighbour. This cut the recoverable RE-SYNC warns **685792 → 29930 (23×)**
> with `bad=0` + bt golden preserved. **Residual (non-corrupting, deeper follow-up):** ~30k warns
> remain from over-sized headers that overstep into a *live* neighbour (not a free hole), so the
> clamp can't catch them; the true source is a `byte[]`'s `element_type` byte reading `Int` —
> a double-serve residue / stray write under the non-moving sweep that the two fixes decay but
> don't fully eliminate. Benign (`bad=0`); perf/cleanliness only.

---
## RE-DIAGNOSIS 2026-06-22 (worktree `CratonVM-regroots`, branch `fix/jit-register-roots`, binary `cvmregroots.exe`, off dev `6e1c13a8`; precise-maps default-on)

A fresh, systematic investigation (~20 controlled experiments + a 6-agent forensic
workflow with adversarial verification) on the **current dev** binary **REFUTES the
central premise of this whole document** ("register-resident missed JIT root") and
substantially re-localizes the bug. **Read this before touching the code or re-running
any of the levers below — most of the older lever table is now misleading.**

### ⛔ REFUTED: it is NOT a missed root in *scan's JIT registers*
`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` blind-spills the **FULL 14-GPR file (incl. RAX
and every caller-saved/argument register)** to scanned frame slots at every GC-capable
call safepoint (the `=all` mode in `jit/src/x64.rs::safepoint_reg_spill_all`, frame
reservation at ~7153, store loop at ~8013). **This document's old table only ever tested
`=1` (callee-saved only).** Result on `ReflRepro 8000 @ CRATONVM_DBG_GC_STRESS=65536`:
`=all` is **BYTE-IDENTICAL to default** — same corruption, same RE-SYNC offsets (2624 /
3304 / 66744), same exit. A live oop sitting in *any* of scan's JIT registers at a *call*
safepoint would have been made visible by `=all`. It was not. **So the missed root, if
one exists, is NOT in scan's JIT-compiled register/operand state at a call safepoint.**
(`=all` does NOT spill a *native's* Rust registers, so it does not rule out a native-side
root — see below.)

### ✅ ESTABLISHED: the bug is a miscompile/codegen-context bug of the single method `ReflRepro.scan`
- `CRATONVM_JIT_BISECT_SKIP=ReflRepro.scan` → **fully clean** (`ok=8000 bad=0 rc=0`).
  Skipping `describeField` alone does **nothing**: `describeField` *bails* on `op=0xba`
  (`invokedynamic`, from the `+` string-concat `makeConcatWithConstants`) and is **never
  JIT-compiled** — it always runs interpreted. So the corruption is driven entirely by the
  JIT codegen of **`scan`** (single-pass, invocation-tier-up, `len=5880`).
- A *generic* missed-root would not care which method is compiled. This one does ⇒ it is in
  scan's compiled code path, i.e. the **regalloc/codegen family**
  ([`jit-regalloc-callee-saved-clobber-family.md`](../../jit-regalloc-callee-saved-clobber-family.md)),
  NOT the GC-root-coverage family.
- `--nojit` clean. `-Xmx 4g` (suppress young GC) does not crash. Needs GC.

### What ruled-OUT (current binary, each rebuilt-free env A/B)
| lever | result | reading |
|---|---|---|
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` | byte-identical | NOT scan's JIT registers (see above) |
| `CRATONVM_NO_PRECISE_JIT_MAPS=1` | **WORSE** (rc=132 SIGILL, ~3116 warns vs ~217) | precise-maps' oop-local spill+reload **MASKS most** of it; underlying bug is in base regalloc |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | still corrupts | scan's only alloc (the StringBuilder) inline-new is header-before-commit & correct |
| `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH=1` | still corrupts | not the operand-stack callee-saved oop flush |
| `CRATONVM_NO_CTOR_DIRECT_CALL=1` | still corrupts | not ctor-direct-call |
| OSR disable (scan's loops < OSR threshold anyway) | still corrupts | invocation-tier-up, not OSR |
| `CRATONVM_DBG_FORCE_MOVING=1` | **no crash, `ok=1917 bad=83`, `MISMATCH: java.lang.Object`** | see below |

### Decisive `FORCE_MOVING` finding — reconciles the collector story
Under `FORCE_MOVING` (moving collector even with scan's JIT frame live) there is **no
crash but wrong results** (a returned object decayed to bare `java.lang.Object`). So:
- The corruption happens under **both** collectors **whenever scan's JIT frame is live**;
  it is tied to the **JIT frame**, not the sweep's linear walk. The crash/hang (rc=127/139)
  is just the **non-moving sweep's** downstream reaction (linear-walk desync on the
  resulting garbage header); the moving collector instead silently returns wrong results.
- `sb` (the StringBuilder) **IS** found as a conservative root at `[rbp-0x10]` (FORCE_MOVING
  *relocates* it → stale slot → `java.lang.Object`), so under the default **non-moving**
  sweep `sb` is **pinned, not reclaimed**. Therefore the non-moving corruption is **not a
  simple reclaim of `sb`** — the primary corrupted object (`CRATONVM_DBG_SWEEP_EDGES`:
  `root=1 ... mark filter rejected a live root`, header **already garbage at mark time**)
  is a **different** live object whose only reference, when scan is JIT-compiled, is not a
  GC root — and a **write/use-after-free corrupts a header**, which the GC then merely
  *detects*. It is **not** the clean "object reclaimed by the sweep" story the old text tells.

### Reproducer isolation (confirms context-sensitivity — matches the family doc)
Two minimal Java reproducers of scan's *shape* — `scratch-min/Min.java` (StringBuilder held
across a `String[]` for-each calling a `+`-concat-bailing helper) and `scratch-min/Min2.java`
(two loops over **freshly-allocated-inside-scan** arrays) — **do NOT reproduce** (`bad=0`).
**Only the real reflection natives** (`getDeclaredFields`/`getDeclaredMethods`/`isSynthetic`/
`getName`/`getParameterCount`) inside scan trigger it. This matches
the archived [`jit-regalloc-callee-saved-clobber-family.md`](../../jit-regalloc-callee-saved-clobber-family.md)
key finding: *"NOT reproducible by bytecode shape … context-sensitive register allocation …
per-method skip bisection, not a small synthetic repro, is what localizes each instance."*

### Forensic workflow verdicts (6 agents, all 3 root-cause hypotheses REFUTED)
1. "JIT spills r13/r14 before every call but never reloads them" — **REFUTED**: r13/r14 are
   Win64 callee-saved (ABI-preserved); `[rbp-0x20]/[rbp-0x28]` are **GC-root spill slots
   written-but-never-read by design** (`emit_pre_safepoint_spill` ~7999: "no post-call reload
   needed under the non-moving sweep"). scan's loop counters are maintained in-register
   correctly (`cmp r13d,r14d` etc.).
2. "Commit-before-header marking window in the slow heap-alloc path" — **REFUTED**: the
   collector is STW (`collect_garbage` takes a `StopTheWorldToken`), GC runs only *after*
   `try_alloc_young`/`tlab_alloc_object` returns (header written), and the sweep and the
   allocator are mutually exclusive on `young_from.lock()`.
3. "RAX native-return-value root gap before push_from_rax" — **REFUTED**: `emit_pre_safepoint_spill`
   precedes every GC-capable call, the post-return sequence has no intervening safepoint, and
   the young GC fires *inside* the interpreted callee where the in-flight oops live in that
   callee's own (rooted, band-covered) interpreter frame.

### ✅✅ MECHANISM CRACKED (2026-06-22, live `CRATONVM_DBG_A2` probe in the non-moving sweep)
**A2 is a NON-MOVING-YOUNG-SWEEP LINEAR-WALK SIZE DESYNC (a header-size/coherence mismatch),
NOT a GC-root gap and NOT a register-resident root.** The decisive evidence is a byte-level
dump of the heap at the first corruption (`gen_heap.rs` sweep walk, gated `CRATONVM_DBG_A2`):

- The walk desyncs at an offset where the bytes are **object field data** — 16-byte `Value`
  cells `{disc=4=Object, <heap-ptr>}` (`SLOT_SIZE=16`), or **UTF-16 char data** (e.g.
  `0x3a006500750072` = `"rue:"` from describeField's `"true:"`), sitting **where a 40-byte
  object/array header should be**. I.e. the linear walk has stepped off the object grid: some
  preceding object's computed size ≠ its real size, so the walk lands mid-object and reads
  field/char data as a header (→ the long-seen `"kind=Object but array_length=N (num_slots=4,
  class_id=4)"` / `"implausible object size"` messages — those are the **symptom of the
  understep/overstep**, the walker reading a `Value`-cell discriminant `4` as a `class_id`).
- **Reference arrays are NOT the bug** (ruled out): `element_byte_size(Reference)=REF_ELEMENT_SIZE=8`
  and a live `Reference[6]` (`class_id=49`) walked cleanly at `size=88 = 40 + 6*8`. The
  allocator, the element reader (`read_prim_element`, 8-byte stride), and the walker all agree
  on 8-byte reference elements. The 16-byte cells at the desync are **object fields**, not
  array elements.
- **This is why it is JIT-only.** The non-moving sweep is the ONLY collector that **linear-walks**
  the young gen (it is forced whenever a JIT frame is live, via `gc_quiescence`). The moving
  (Cheney) collector traces live roots and never linear-walks, so it never consults the
  mis-computed size. Hence `--nojit` (no JIT frame → moving) is **clean**, and `FORCE_MOVING`
  (moving even with a JIT frame) gives **wrong-results-without-crash** (a different failure:
  relocating conservative JIT roots). The bug needs scan JIT-compiled ONLY to force the
  non-moving linear-walk collector — scan's *codegen* is incidental, **not** miscompiled.
  (This SUPERSEDES the "regalloc miscompile of scan" framing in the section header above: scan
  is the trigger for the non-moving sweep, not the source of a wrong store.)

**⇒ The fault is a header-coherence / size-computation mismatch for some object or array kind
in the reflection-allocation workload** (`describeField`'s `String`/`char[]`/`byte[]` building
+ `Field[]`/`Method[]`/reflection objects), where `gen_object_total_size`'s computed size
diverges from the allocator's actual cursor advance. bintrees (uniform `Node` objects, no such
arrays/objects) never trips it. The exact culprit allocation varies per GC (the dead-object
zeroing — `ArrayElementType::Reference == 0`, so a zeroed header reads as a `Reference` array —
also obscures post-walk re-reads), so the precise next step is an **allocation breadcrumb**:
record `(addr → class_id, kind, element_type, array_length, num_slots, real_size)` at every
young header-write (`init_object_header` in vm + `try_alloc_array`/`try_alloc_object` in gc),
and at the sweep desync look up the corrupt address **and** the preceding object — that names
the exact mis-sized object + its allocation site in one run, isolating whether it is (a) an
allocator that writes a wrong/partial header (the documented "inline-alloc forgot to set
kind=Array" class, `gen_heap.rs:5918`), or (b) a `gen_object_total_size` size-computation bug
for a specific object/array kind. The fix is then GC-side (header coherence / walker sizing) —
**NOT** the precise-JIT-maps/regalloc project, and **NOT** a `skip_list` ban (those address the
wrong layer). Gated probe `CRATONVM_DBG_A2` (first 4 corruptions) is committed in `gen_heap.rs`.

### ✅ Allocation breadcrumb (2026-06-22, CRATONVM_DBG_A2) — drilled the mechanism further + a REAL fix
A young-allocation breadcrumb (`gc/src/a2dbg.rs`: records `addr → class_id/kind/element_type/
array_length/num_slots/real_size` at every young header-write — `try_alloc_*`/`alloc_*` in
gen_heap + `init_object_header` in vm; also `record_free` in the sweep's dead/forwarded branches)
+ a walk-time raw-header capture in the sweep walk produced these load-bearing facts:

1. **The walk over-sizes a `byte[]`.** At the first detected desync the walker computes e.g.
   `byte[37]` (real `40 + 37*1 = 80`) as **192** = `40 + 37*4` (a 4-byte/Int element). The
   walk-time header raw word is `0x00000a0100000000` → `class_id=0, kind=Array(1),
   element_type=Int(0x0a=10)` — the **`element_type` byte at offset 5 reads Int, not Byte(8)**.
   So the walk reads a corrupt/garbage header for what was a byte array and over-strides into
   the next (live, correctly-tracked) object's field region → the `class_id=4`/`array_length=N`
   "implausible size" symptom.
2. **Reference arrays remain clean** (a live `Reference[6]` walks at `40+6*8=88`, matches alloc).
3. **A REAL robustness bug FOUND + FIXED (committed):** the sweep's free-block skip
   (`gen_heap.rs` walk loop) only matched `cursor == off` EXACTLY. On any overshoot
   (`cursor > off`) `free_iter` wedged forever and every later freed+zeroed region was walked as
   a run of 40-byte phantom `Object`s → desync cascade. Replaced with a **robust skip** (advance
   past wholly-passed blocks; resync to the block end when the cursor lands at/inside one).
   **Validated: byte-identical in the normal in-sync case; `bt16=14985902`, `bt18=68332206`
   golden, no regression.** This is a genuine fix for a *cascade-amplifier* class — but it does
   **NOT** make A2 `bad=0`, because the over-sized `byte[]` (#1) is not a free-list block.
4. **The residual root** is the over-sized byte-array header: a freed byte-array slot whose
   header word0 is overwritten to `class_id=0/kind=Array/et=Int` by an apparently-UNTRACKED reuse
   (all `kind=Array` *allocations* are breadcrumb-hooked, so this is either a write the breadcrumb
   can't see or a header corruption), making the walked object OVERLAP a live tracked object →
   overstep. Only under the non-moving sweep (moving collector never linear-walks → `--nojit`
   clean). **Breadcrumb caveat:** between `scan` calls (no JIT frame) the *moving* collector swaps
   the young arena, so breadcrumb absolute addresses go stale across GC epochs — the cross-epoch
   "FIRST-MISMATCH @0" is an artifact; the breadcrumb is only reliable within one non-moving epoch.
   **Next step:** clear the breadcrumb on each arena from/to swap (so addresses stay valid), then
   the FIRST-MISMATCH within one epoch names the exact mis-sized allocation; and audit
   `Arena::add_free_block`/the free-block split-on-alloc for a region handed out twice (overlap),
   which `Arena::add_free_block` does not currently check. The fix is GC-side (arena/header
   coherence), NOT precise-maps/regalloc, NOT a skip_list ban.

### ✅ Clear-on-swap done (2026-06-23, `cvma2arena.exe`) — root = a FREED-slot garbage header missing from the free list
`a2dbg::clear()` now fires at the young from/to swap (`gen_heap.rs` ~3295) so the breadcrumb is
reliable within one non-moving epoch. Decisive: the sweep's **FIRST-MISMATCH finds NO mismatch
among LIVE tracked objects** — every live object sizes correctly. The desync is ENTIRELY a
**freed slot** (`@2384`, freed THIS epoch) whose header reads garbage
`kind=Array/et=Int(10)/array_length=37 → 192` (the original byte[37]'s `kind`+`array_length` with
the `element_type` byte at offset 5 flipped Byte(8)→Int(10)), whose freed region is **NOT on the
sweep's free list** (the robust skip can't skip what isn't listed), overstepping the live
`class_id=398` object at 2520. ⇒ A2 is a **free-list/alloc accounting bug**: a freed byte-array
region (a) goes missing from the free list between sweeps (consumed by a TLAB refill /
`Arena::alloc` free-list path but left with a non-zero header and NOT re-tracked), and (b) its
`element_type` byte is corrupted Byte→Int by a stray/leftover write (no young array-header write
path is unhooked — JIT has no inline array alloc; native arrays use the hooked `alloc_array`).
**Next instrument (precise pin):** log every `Arena` free-list add/remove + every alloc served
from the free list (offset+size+caller) to capture the moment `@2384`'s region leaves the free
list with a non-zero header. Committed: clear-on-swap `d8d85f62`; robust free-block skip FIX
`f9138bf0` (validated bt16=14985902/bt18=68332206 golden).

Repro (unchanged): `CRATONVM_DBG_GC_STRESS=65536 cvmregroots.exe --java-home <jdk25> -cp
docs/internal/repros/A2-reflrepro ReflRepro 8000`. `javac` the class first.

---
## RE-DIAGNOSIS 2026-06-18 (worktree `CratonVM-shadowdbg`, branch `dbg/shadow-reload-probe` @ `c9b56f7d`; precise-maps default-on + shadow reload fix) — SUPERSEDED in part by 2026-06-22 above (the `=all` test was never run in this section)

A full re-investigation on the **current** binary corrects two load-bearing claims in
the 2026-06-17 section below and re-confirms the rest. **Net: A2 is a register-resident
missed-JIT-root use-after-free. It is NOT a sweep sizing bug and NOT closeable by the
"8-byte stride" thread.** Read this before touching the code.

### ⛔ REFUTED: the "8-byte allocator↔walker stride mismatch" is a RED HERRING — do NOT chase it
The 2026-06-17 "refined next step" hypothesises a specific allocation kind whose
cursor-advance is 8 bytes larger than `gen_object_total_size`. **An exhaustive static
audit of every young-allocation path proves no such mismatch exists:**

| path | size it bumps the cursor by | matches walker? |
|---|---|---|
| `gen_heap::alloc_array` / `try_alloc_array` / `try_alloc_array_full` | `HEADER_SIZE + array_data_size(len,elem)` (round-**8**) | ✅ identical to `gen_object_total_size` |
| `gen_heap::try_alloc_object` | `HEADER_SIZE + num_fields*SLOT_SIZE` | ✅ |
| interpreter `gc_alloc_array` → `try_alloc_array` | same as `alloc_array` | ✅ |
| interpreter `tlab_alloc_object` (`thread.tlab.alloc(total,8)`) | `HEADER_SIZE + num_fields*SLOT_SIZE` | ✅ |
| JIT inline `new` (`emit_inline_tlab_new`, x64.rs ~10011) | `HEADER_SIZE + num_fields*SLOT_SIZE`, cursor 8-aligned | ✅ |
| JIT `jit_newarray` / `jit_anewarray_object` (helpers.rs 963/1327) | `HEADER_SIZE + array_data_size` (round-8) via `try_alloc_array` | ✅ |

Every `.alloc(size, align)` site uses **align 8** (61×) or 1 (10×, byte buffers); **none
use 16**. There is **no post-allocation write to the `array_length` (off 12) or `num_slots`
(off 16) header fields** outside `ObjectHeader::new`. `40 mod 16 == 8` makes every *object*
size `≡8 (mod 16)`, which is what seduced the prior author into the "round-16" theory — but
nothing rounds to 16. **Conclusion: the 8-byte hole at the sweep desync is a downstream
*consequence* of the wrong reclaim + free-block reuse + UAF writes, not an independent
fixable sizing bug.**

### ✅ CORRECTED: the SIGSEGV is a downstream use-after-free, NOT the sweep linear walk
On the current binary the non-moving sweep **re-syncs successfully** (`RE-SYNCED at offset
3072 (skipped 8 bytes)`) and does NOT itself crash. The `rc=139` is a later
`EXCEPTION_ACCESS_VIOLATION read at <obj>+8` from **JIT/mutator code dereferencing a
stale/reused slot** (the wrongly-reclaimed reflection oop's address, now reused as a
different object → wild pointer). `FORCE_MOVING` avoids the crash because a semispace
never re-uses the freed slot as a real object (→ wrong result, not a wild pointer), AND
never linear-walks. So the prior section's "the crash IS the non-moving linear walk" is
imprecise: the linear walk is robust now; **the crash is the UAF the reclaim creates.**
Therefore the "crash-robustness / make the sweep stride the hole" fix (#1 below) is
*insufficient* — it cannot stop a mutator-side UAF. Only root coverage (#2) fixes both.

### Current-binary lever status (re-measured; the 2026-06-17 table is STALE — precise-maps default-on changed the landscape)
`CRATONVM_DBG_GC_STRESS=65536 … ReflRepro 8000`:

| config | result | note |
|---|---|---|
| default | `rc=139` | A2 (UAF SIGSEGV) |
| `--nojit` (`CRATONVM_DISABLE_JIT=1`) | clean | JIT-only |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | `rc=127` | inline-new is NOT the culprit (8-byte desync persists) |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL=1` (blind-spill all callee-saved GPRs) | `rc=139` | missed oop is NOT in a callee-saved reg |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | `rc=132` | **now ALSO crashes** (diverged from old handoff's "no crash, bad=1") — whole-stack scan marks garbage |
| `CRATONVM_DBG_FORCE_MOVING=1` | no SIGSEGV, wrong results | reclaim still happens; no linear walk |
| `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1` | `rc=1`, **no SIGSEGV but still corrupts** (throws at `describeField:28`) | shadow publishes operand-stack *Reg* oops; the lost oop is NOT one of those |

**No lever fixes it.** Notably `SAFEPOINT_REG_SPILL` (callee-saved blind-spill) and
`SHADOW_PIN` (operand-stack Reg publish) both fail → the lost oop is neither a
callee-saved-register local nor an operand-stack register entry.

### Root-scan architecture (verified — so the next session doesn't re-derive it)
- Collector while any JIT frame is live = the **non-moving sweep** (`gc_quiescence::is_active()`), default + selective-promotion. Over-retention is always SAFE for it (never relocates).
- **Current (allocating) thread** roots come from a **fresh `collect_roots`** (`vm/src/memory/roots.rs:48`) at GC time — NOT from `thread.root_snapshot` (that is used only for *parked* threads via `collect_all_root_snapshots`). `collect_roots` = interpreter `thread.frames` (locals+operand stack, `is_object_address`-filtered; +`scan_locals_conservative` when `conservative_locals_enabled`) + statics/mirrors/interns/etc + `native_pin_roots` + `native_pending_return` + `scan_active_jit_frames` (after `invalidate_scan_cache_for_gc`).
- `scan_active_jit_frames` per chain entry → **precise path** `scan_one_frame_precise` = (a) read all of the method's oop-map slots, **plus (b) a conservative backstop `scan_one_frame(scanner_sp, info.frame_base)`**. `frame_base` is the **entry-time SP (≈ outermost `entry_sp`)**, deliberately kept separate from `exact_rbp` (innermost) — see `conservative_roots.rs:144-154` (they already fixed an "innermost-only shrink" regression). So the backstop covers `[scanner_sp, entry_sp)` = the **entire** JIT stack region (all nested JIT frames + the native/Rust frames between them). ⇒ **any reflection oop spilled to ANY JIT frame slot, or held in any native Rust frame below entry_sp, IS conservatively rooted.**
- `push_from_rax` (x64.rs:10217) ALWAYS spills an invoke/native object return to a `Frame` slot `[rbp-off]` (within the backstop) — so the return value is covered the instant it is consumed into the operand model.

### What this leaves as the ONLY possibility
`SWEEP_EDGES` = `root=0 young-survivor=0 old-gen=0` (no heap/root/card edge) + the whole
JIT stack region is conservatively scanned + interpreter frames are scanned ⇒ at the GC
safepoint the reflection result's **only** reference is in a **live machine register that
is not spilled anywhere on the stack** (classically `rax` holding a native call's return
before any spill, or a value the operand model holds in a caller-saved/scratch reg across a
GC-capable call without spilling). A conservative *stack* scan — cached, fresh, per-entry,
or whole-stack — fundamentally cannot see a live register. This is exactly the class the
precise-JIT-stack-maps / shadow-stack work targets, and why none of the stack levers close it.

### Recommended next steps (priority order; all require build+test, ~minutes each)
1. **Decisive instrument (do FIRST):** audit `emit_oop_map_for_safepoint` / `emit_pre_safepoint_spill` for whether the operand model EVER leaves an oop in a **caller-saved / scratch GPR live across a GC-capable CALL** without spilling — and whether the just-returned `rax` of an object-returning invoke is spilled BEFORE the *next* GC-capable call (it is spilled by `push_from_rax`, but verify there is no intervening safepoint). If a live-oop reg survives a call unspilled, that reg is the lost root: spill *every* live-oop GPR (not just callee-saved `alloc_used_regs`) to a scanned frame slot before each safepoint. This is the handoff's option-3 "narrow interim" and the most contained real fix. Suspect sites: the MIC/PIC direct-`call r11` fast path, and any invoke whose result is consumed by a *following* call.
2. **Shadow-stack-PIN completion:** `collect_live_oop_homes` only publishes operand-stack `Reg` homes. It does NOT publish (i) the `rax` native-return before `push_from_rax`, nor (ii) oop *locals* the register allocator kept in a caller-saved reg. Extend the published-home set to those, keep `SHADOW_PIN` (non-moving-safe), verify `bad=0`.
3. Precise per-PC register maps (largest; deferred).

**Verification bar (unchanged): `bad=0` on `ReflRepro 8000` under `GC_STRESS=65536` (not just
"no crash"), no bintrees regression (`bench/BenchSuite bintrees18` must stay `68332206`), no
WildFly/Spring regression.**

Build: `build-cpu.bat` (PowerShell). JDK: `C:\Program Files\Java\jdk-25`. Repro needs the
class compiled first: `javac wildfly-suite/repro/ReflRepro.java`.

---
## DEFINITIVE DIAGNOSIS 2026-06-17 (4-agent workflow + verification on dev `82cf85e9`, binary with the SHADOW reload fix + precise-maps default-on)

A2 is a **two-part bug** and is **NOT closed by any root-coverage mechanism** currently:

**Part 1 — the reclaim (root cause).** `ReflRepro.scan` is JIT-compiled; it dispatches
`Class.getDeclaredFields()/getDeclaredMethods()` (allocating natives) whose object result
(`Method[]` / `getName` String / StringBuilder `char[]`) is live only via a JIT **register
(rax)** or a native-return slot **above** the per-JIT-entry `[scanner_sp, entry_sp]`
conservative band at the GC. Under `GC_STRESS=65536` the JIT-active non-moving sweep marks
from a root set that misses it → it is reclaimed while live. `CRATONVM_DBG_SWEEP_EDGES`:
`root=0 young-survivor=0 old-gen=0` (no inbound edge).

**Part 2 — the CRASH (the SIGSEGV) is a SEPARATE non-moving-sweep robustness bug.** After the
reclaim, the non-moving sweep's *linear* walk cannot safely re-walk the resulting hole: the
freed/zeroed span drifts the cursor off the object grid (a zeroed 40-byte chunk decodes as a
phantom empty Object; an accumulated free list overlaps live objects — `Arena::add_free_block`
does no overlap check, and the walk skips free blocks only on exact `cursor==off` match), so
the walk lands mid-object and reads leftover field bytes as a giant `num_slots`
(`class_id=0, kind=Object, num_slots=16423 → size 40+16423*16 = 262808` — the reported
"implausible object size 262808" at off=3064). The walker itself is SOUND — off=3064 is a real
boundary it correctly reached; the bad size is how it *detects* the pre-existing corruption.

**Decisive verification (current binary):**
| config | result | reading |
|---|---|---|
| default (precise maps on) | rc=139, stop @3064 | A2 reproduces |
| `--nojit` | **ok=8000 bad=0** | JIT-on only |
| `CRATONVM_DBG_FORCE_MOVING=1` | **ok=7584 bad=416, NO crash** | the reclaim still happens (bad=416 wrong results), but the MOVING collector never linear-walks → no crash. **Proves the crash is the non-moving linear walk, the reclaim is a separate wrong-result bug.** |
| `CRATONVM_SHADOW_STACK=1` (+ the reload fix) | rc=139 (stop @2944) | does NOT fix A2 (the old "shadow fixes ReflRepro" claim is STALE) |
| `CRATONVM_PRECISE_JIT_MAPS` (default) | rc=139 | precise maps fix A3 but NOT A2's register-only/native-return root |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | rc=139 (stop @67536) | full native-stack scan does NOT fix it either (truly register-only residual) |

So **no stack/register-coverage approach (precise, shadow, fullstack) prevents A2** on the
current binary — the missed root is genuinely register-only (rax, a native-call return value).

**Two fixes are needed for `bad=0` + no crash (both are deep GC/JIT work):**
1. **Eliminate the crash (more contained, GC-side):** make the non-moving sweep robust to a
   reclaimed hole — rebuild the from-space free list from THIS cycle's dead regions instead of
   carrying a stale accumulated list; harden `Arena::add_free_block` (gc/src/arena.rs:149) against
   overlaps; and/or stamp every reclaimed dead span with a walkable filler (like the TLAB
   `install_tail_filler` GAP_FILLER) so the linear walk strides it cleanly regardless of cursor
   position. This stops the SIGSEGV even when a reclaim happens (degrades crash → wrong-result).
2. **Eliminate the reclaim (root coverage):** cover the register-only / native-call-return oop —
   the narrow interim is to have JIT codegen spill every invoke/native-call **object return
   value** to a conservatively-scanned stack slot before the next GC-capable call (investigate the
   MIC/PIC direct-call `call r11` fast path in jit/src/x64.rs). precise maps don't cover this
   because the oop is a register/native-return value, not a JIT frame slot.

### Fix attempt #1 (reclaimed-region filler) — TRIED, did NOT fix A2, REVERTED

Hypothesis: the non-moving sweep ZEROES dead objects (`gen_heap.rs` `sweep_young_non_moving`,
`write_bytes(obj_ptr, 0, total_size)`), and a zeroed `>= HEADER_SIZE` hole decodes as a run of
phantom 40-byte `Object`s (class_id=0, num_slots=0 → size 40), so a re-walk whose cursor missed
the exact free-block start strides them and drifts off-grid. Fix tried: stamp a walkable `int[]`
filler over each reclaimed span instead of zeroing (same as `Tlab::install_tail_filler`), so any
hole is self-describing and the linear walk re-syncs from any on-grid position.

**Result: did NOT fix A2** — the crash just MOVED (off=3064 → off=2904) and the corrupt header
changed from a zeroed gap to *random payload* (`kind=0x3a`, huge garbage), i.e. the walk still
drifts, from a DIFFERENT source. **So the drift is NOT the reclaimed-zeroing** (refuted). The
original off=3064 byte-dump confirms this: the 8 bytes at 3064–3071 are a zero **inter-object
gap** (the walk stops at 3064; the real next object re-syncs at **3072**, 8 bytes later) — i.e.
an **8-byte stride mismatch between the allocator's cursor advance and the walker's computed
size for the *preceding* object**, NOT a zeroed reclaimed dead object (which is `>= 40` bytes).
The filler also **regressed sweep perf** badly (`MinRegexProbe` @`GC_STRESS=4MB`: ~2 s → 71 s) —
the fillers accumulate (re-stamped / re-added every sweep on long-lived workloads). Reverted;
not on dev.

**Refined next step:** instrument the sweep walk to log the FIRST object whose
`gen_object_total_size` stride diverges from the real allocation grid (the object BEFORE the
first desync), and cross-check its size against what the allocator advanced the cursor by for
that exact object — the 8-byte mismatch is an allocator↔walker size disagreement for some
specific object/array kind (candidate: a JIT inline array alloc that 16-aligns or over-rounds
the cursor by 8 vs the walker's 8-rounded `array_data_size`; or a `char[]`/odd-element-size
array). `bt16/bt18` are unaffected (checksums stay golden) so it's a kind the bintrees workload
never allocates — reflection/`char[]`/String-specific.

**Status: OPEN — fully diagnosed mechanism, root of the 8-byte stride mismatch still unpinned;
fix is multi-session GC/JIT core work** (distinct from the now-fixed A3 register-invisibility,
which precise maps default-on closed).

---

**Status: OPEN.** JIT-on-only heap corruption under GC stress. A live
reflection-result object is reclaimed by the non-moving young sweep because its
only reference, at sweep time, is invisible to the conservative root scan — it
sits in a JIT **register** (and/or on the native stack *above* the per-JIT-entry
scan band). The slot is reused as a bare `java/lang/Object` → crash
(`implausible object size` sweep abort, rc=132/139) or a silent wrong result
(`MISMATCH: java.lang.Object@…`).

This is the long-standing residual of
[`jit-junit-discovery-reflection-corruption.md`](jit-junit-discovery-reflection-corruption.md)
(bug-06; the reflection mirror-array *pinning* part is fixed). It is the same
class as the precise-JIT-stack-maps work — see
[`fork6-fjp-multithread-jit-root-reclamation-FIXED.md`](../fork6-fjp-multithread-jit-root-reclamation-FIXED.md).
Detailed writeup: `docs/wildfly-suite-bugs/bug-06b-jit-scan-cache-unsound.md`.

## Reproduce

```
# wildfly-suite/repro/ReflRepro.java (committed). JDK 25 boot.
CRATONVM_DBG_GC_STRESS=65536 target/release/cratonvm.exe \
  --java-home "C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot" \
  -cp wildfly-suite/repro  ReflRepro 8000        # rc=132/139, deterministic
```

`ReflRepro.scan` is JIT-compiled; it iterates `Class.getDeclaredFields()` /
`getDeclaredMethods()` (allocating natives, dispatched from a JIT frame) and
builds strings with `StringBuilder`. `GC_STRESS=65536` forces a young GC every
64 KB so the corruption is deterministic. Clean with `CRATONVM_DISABLE_JIT=1`.

## Decisive evidence (run on current dev, df304353+)

| Toggle | Result | Reading |
|---|---|---|
| default | crash (rc=132/139) | the bug |
| `CRATONVM_DISABLE_JIT=1` | clean | JIT-specific |
| `CRATONVM_NO_JIT_SCAN_CACHE=1` (cache off) | **crash** | NOT the JIT-scan cache |
| `CRATONVM_JIT_SCAN_CACHE=1` (cache on) | crash | NOT the cache |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` (scan whole native stack as roots) | **no crash, but `bad=1`** (`java.lang.Object` mismatch persists ~1/8000) | root is mostly *above the per-entry band*, occasionally *not on the stack at all* |
| `CRATONVM_DBG_FORCE_MOVING=1` (moving collector) | no crash | the non-moving sweep is the manifestation |
| `CRATONVM_SHADOW_STACK=1` (publish register oops) | no crash (slow) | the missed root is a register oop |
| `CRATONVM_DBG_SWEEP_EDGES=1` | `root=0 young-survivor=0 old-gen=0` every sweep | reclaimed node has NO heap/root/card edge ⇒ register/native-stack root |

The `FULLSTACK_SCAN` result is the key: scanning the **whole** native stack
removes the crash but still leaves `bad=1`. So:
1. Usually the root has spilled to the stack but lives **above the JIT entry's
   `[scanner_sp, entry_sp]` band** (in the interpreter/native/Rust frame that
   invoked the JIT) — the per-entry conservative scan stops at `entry_sp` and
   misses it; a whole-stack scan finds it.
2. Occasionally the root is **truly register-resident** (`rax` holding a native
   call's return value before it is spilled) — no stack scan can see it; this is
   the residual `bad=1`.

## Root cause

`getDeclaredFields()` / `getDeclaredMethods()` are dispatched **from a JIT
frame**; their object result returns in a register. The non-moving young sweep
is forced whenever a JIT frame is live (`gc_quiescence`), because conservatively
discovered roots cannot be relocated. That sweep marks from the conservative
root set, which scans only the **stack** (and only the per-JIT-entry band). A
register-resident oop — or one on the stack above `entry_sp` — is therefore not a
root, gets reclaimed, and its slot is reused (→ bare `java/lang/Object`).

## What does NOT fix it (verified dead ends — don't repeat)

- **Disabling the JIT-scan cache.** A prior pass mis-attributed the crash to the
  cache being unsound and shipped `jit_scan_cache_enabled()` default-off. On the
  base it was developed against (`0e3f0398`) that *appeared* deterministic, but
  it was only a **GC-timing perturbation**: on current dev the crash is identical
  cache-on and cache-off. **Reverted** in `b41c0484`. (Kept the genuine, separate
  `collection_count` cache key — stops the cache republishing a freed address
  across a GC.) **Lesson: verify a timing-sensitive GC fix on the actual target
  branch HEAD, not an old worktree base; a "deterministic" env toggle can be a
  timing mask.**
- **A whole-native-stack conservative scan** (`scan_full_native_stack` added to
  `collect_roots`/`safepoint_check`, or `DBG_FULLSTACK_SCAN`). Removes the crash
  but not the corruption (`bad=1`) — the residual root is in a register.
  Subtlety found while testing: a full-stack scan in `collect_roots` *alone* did
  NOT remove the crash; only the full-stack scan inside `scan_active_jit_frames`
  (which also runs on the `update_root_snapshot` per-native-call path) did. So
  the above-`entry_sp` root is captured at native-return time and persisted in
  the published snapshot, not re-found at GC time — worth understanding before
  picking an insertion point.

## The real fix (required)

The root set the non-moving sweep marks from must include **register-resident
oops** at the safepoint. Options, in rough order of cleanliness:

1. **Complete the shadow stack** (`gc/src/shadow_stack.rs`,
   `jit/src/x64.rs::shadow_stack_maps_enabled` and the push/reload codegen). It
   already pushes "every live oop (operand-stack entries AND oop locals)" before
   GC-capable calls — exactly what's needed — but is gated off and **globally
   incomplete**: per the memory it currently routes to the *moving* collector
   (which under-counts bintrees, 68199090) and "its push/reload codegen is
   incompatible with the non-moving sweep." Making the shadow push/reload work
   *with* the non-moving sweep (pin shadow oops instead of relocate — see the
   `CRATONVM_SHADOW_PIN` experiment) and enabling it by default is the principled
   fix. See `precise-jit-stack-maps-fork6-findings.md`.
2. **Precise oop maps** for JIT frames (Stage B/C, deferred) — describe exactly
   which slots/registers hold oops at each safepoint.
3. **Narrow interim:** have JIT codegen spill the live-oop set (at minimum every
   invoke/native-call object return value) to a stack spill slot the conservative
   scan covers, before the *next* GC-capable call. Candidate gap to investigate:
   the **MIC/PIC direct-call fast path** (`call r11` in `jit/src/x64.rs`, the
   inline-cache hit that bypasses `jit_invoke_*_mic`) — confirm whether it spills
   the caller's live oops before the call the way the helper path does. If it
   doesn't, that is a plausible localized source of the register-resident window.

Whichever path: the verification bar is **`bad=0`** on `ReflRepro 8000` under
`CRATONVM_DBG_GC_STRESS=65536` (not just "no crash"), plus no regression on
bintrees (`bench/BenchSuite bintrees18`) and the WildFly/Spring suites.

## State on dev

- `b41c0484 fix(gc): revert misdiagnosed JIT-scan-cache default-off; correct
  ReflRepro residual diagnosis` — cache re-enabled, `collection_count` keying
  kept, `bug-06b` doc corrected. No code change attempts the real fix.

## Diagnostic env reference

`CRATONVM_DBG_GC_STRESS=<bytes>` (force young GC), `CRATONVM_DISABLE_JIT`,
`CRATONVM_JIT_BISECT_ONLY=<class-prefix>` / `CRATONVM_JIT_BISECT_SKIP=<Class.method>`
(narrow which methods JIT), `CRATONVM_DBG_SWEEP_EDGES` (classify the reclaimed
node's inbound edge), `CRATONVM_DBG_SWEEP_ZERO` (record swept objects' original
class), `CRATONVM_DBG_FULLSTACK_SCAN`, `CRATONVM_DBG_FORCE_MOVING`,
`CRATONVM_SHADOW_STACK` (+ `_NOPUSH`/`_NORELOAD`/`_PIN` bisect toggles),
`CRATONVM_DBG_CORRUPT_FRAMES` (mutator Java stack at first sweep corruption).
