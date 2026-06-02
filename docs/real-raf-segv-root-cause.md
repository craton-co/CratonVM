# Real-bytecode RAF SEGV — precise root cause (2026-06-01)

> **UPDATE 2026-06-02 — the documented bug is FIXED; a *second*, distinct bug is
> now the blocker.** The "JIT branch/merge spill-slot codegen bug" this whole
> document root-causes (the `visit(CPI)` putfield writing the boolean into the
> receiver's slot → `obj_ptr=0x1`) was a single-line defect in `reset_spills`
> (`jit/src/x64.rs`): after a conditional branch it reset `next_spill_offset`
> all the way to `base_spill_offset`, ignoring the operand stack still holding
> the `putfield`/`putstatic` receiver (`this`) at `base_spill+0`. The next
> `push_stack` then handed that slot back and the computed boolean overwrote
> `this`. Fix: reset only to just past the highest *live* stack slot. After the
> fix the `JIT-PFI-BAD` diagnostic never fires, synthetic avrora still passes,
> and all 686 jit unit tests pass.
>
> Real-RAF avrora then advanced past `visit(CPI)` and SEGV'd **elsewhere** — a
> *second*, distinct, pre-existing bug the experiments below never reached because
> the deterministic `obj_ptr=0x1` fault always fired first. That one is now also
> **FIXED**: it was a **JIT code use-after-free** — deoptimization frees a
> method's code buffer (`jit_cache.remove` → `ExecutableBuffer::drop` →
> `VirtualFree`) while baked-in direct `CALL rel32` sites in other methods still
> target it, so a later call faults (execute) at the freed 64KB buffer. Fix: JIT
> code is now retained for the process lifetime (no free-on-drop). With both fixes
> real-RAF avrora runs to completion (only a pre-existing, JIT-independent digest
> mismatch remains). Full analysis: "Part 3 — ROOT CAUSE + FIX: JIT code
> use-after-free on deopt" at the bottom.

This supersedes the earlier hypothesis in
`system-out-redirect-and-fos-buffering-fix.md` (§"why real-bytecode RAF SEGVs
avrora — the JIT re-entrancy UB"). **That hypothesis was wrong** and is
disproven below. The real cause is now pinned to a specific faulting
instruction, faulting function, faulting value, and triggering method.

## TL;DR

- The `jit_thread_mut` "aliasing &mut JvmThread borrow" debug assert is a
  **false positive**. The nested-dispatch re-entry is a sound *child* reborrow
  (the inner `&mut *ptr` descends from the outer `&mut` via the
  `set_jit_thread(thread)` cast). Proven: with the assert neutered, avrora
  drives 1115 nested borrows and **PASSES** with no SEGV. Fixed by making the
  borrow flag level-aware (commit "fix(jit): make jit_thread_mut borrow check
  level-aware").
- The actual real-RAF SEGV is a **JIT × GC root-corruption** bug, NOT the
  re-entrancy and NOT in the RAF/Cleaner code itself.

## How it was localized

Tooling added this session (Windows had ZERO hardware-fault diagnostics — a
SEGV died with empty stderr + bare `STATUS_ACCESS_VIOLATION`):

1. `install_hardware_fault_handler()` — a Windows vectored exception handler
   (`crash_handler.rs`) that on a fatal fault prints the faulting PC, access
   type + data address, thread, exe module base + faulting RVA, and a raw
   `RtlCaptureStackBackTrace` return-address list (robust on corrupted/JIT
   stacks where `Backtrace::force_capture` re-faults), written single-shot to
   `hs_err_pid<pid>.log` so it survives multi-threaded teardown races.
2. `CRATONVM_SYMBOLIZE=<rva,...>` startup hook — resolves exe-relative RVAs
   against the SAME binary's PDB via dbghelp `SymFromAddr` (set the symbol
   search path to the exe dir; `SymLoadModuleExW` the main module).
3. `CRATONVM_REAL_RAF=1` — gates OFF the synthetic `RandomAccessFile` natives in
   `native-io::register_io_extras_natives` and
   `phases_late::register_phase57_random_access_file`, so RAF runs real JDK
   bytecode (the reproduction switch).
4. `CRATONVM_DBG_JIT_PUTFIELD=1` — `jit_putfield_int` reports a non-canonical
   receiver (obj_ptr not 8-aligned / out of canonical range) with the
   containing JIT method via a save/restore `CURRENT_JIT_CALLEE` thread-local.

## The fault, exactly

```
EXCEPTION_ACCESS_VIOLATION (write) at core::ptr::write::<Value>+0x3
  called from  cratonvm_vm::jit::helpers::jit_putfield_int+0xFE
  called from  JIT-compiled code
on a worker thread ("Thread-2"/"Thread-3")
[JIT-PFI-BAD] obj_ptr=0x1 field_index=11 val=0x1  (write target 0xD9)
current_jit_callee = avrora/arch/legacy/LegacyInstrVisitor.visit(Lavrora/arch/legacy/LegacyInstr$CPI;)V
```

`jit_putfield_int` guards only `obj_ptr == 0`; here `obj_ptr == 0x1` (the
int/boolean `1`, NOT a heap pointer), so it computes `1 + HEADER_SIZE +
11*SLOT_SIZE = 0xD9` and `ptr::write`s a `Value::Int` there → SIGSEGV.

## Why it is GC × JIT, not a static miscompile and not RAF/Cleaner code

Decisive experiments (all on `avrora -s small`):

| config | JIT | RAF | result |
|---|---|---|---|
| default | on | synthetic | **PASS** |
| default, `-s default` | on | synthetic | **PASS** |
| aggressive JIT (threshold 20) | on | synthetic | **PASS** |
| `CRATONVM_DISABLE_JIT=1` | off | real | no SEGV (runs; avrora digest differs) |
| default | on | **real** | **SEGV** in `visit(CPI)`, obj_ptr=0x1 |

- JIT off ⇒ no crash ⇒ the fault requires JIT-compiled code.
- Synthetic RAF never crashes, even with aggressive JIT forcing `visit(CPI)` to
  compile ⇒ NOT a static codegen bug in `visit(CPI)`.
- avrora's simulated instruction stream is deterministic regardless of RAF, so
  the ONLY thing real-RAF changes is **host GC behavior**: the real RAF ctor
  allocates `FileDescriptor` + `Cleaner`/`FileCleanable`/`PhantomReference`,
  driving extra (STW, compacting) GCs.
- The crash is on a worker thread *running JIT-compiled `visit(CPI)`*, whose
  receiver (the interpreter object) is held in a JIT frame slot. The JIT
  compiler **does not emit precise oop maps** (see
  `conservative_roots::JitEntryGuard::enter_with_compiled` — falls back to
  conservative scanning). Under the extra real-RAF GC pressure, a relocation
  during `visit(CPI)` corrupts that receiver slot to `0x1`.

## The fix — part 1 DONE: cross-thread JIT roots in the snapshot

Commit "fix(gc): scan active JIT frames in the cross-thread root snapshot".

The STW collector reads each thread's roots via `collect_all_root_snapshots()`
(the only cross-thread path; `collect_roots`, which *did* scan JIT frames, runs
only on the current thread / in tests). But `update_root_snapshot`
(interpreter.rs) scanned only `thread.frames` + native pins — it NEVER scanned
the thread's active JIT spill region. So a worker thread's JIT-held receiver was
entirely absent from the snapshot the collector reads. Fix:
`update_root_snapshot` now also calls `scan_active_jit_frames` (it always runs on
the thread it snapshots, so the thread-local scan captures that worker's live
JIT frame).

**Verified:** with this fix, `CRATONVM_REAL_RAF=1` avrora **no longer SEGVs in a
debug build** (it advances all the way to real-RAF I/O); synthetic avrora still
PASSES in both debug and release (no regression).

Part 1 closes a real latent cross-thread reclamation gap, **but it does NOT fix
the avrora real-RAF SEGV** (see below). Keep it on its own correctness merits.

## Part 2 — avrora real-RAF SEGV is NOT a GC reclamation/relocation bug

The earlier "GC×JIT roots" hypothesis is **disproven** by experiment (all on
`release-with-debug`, which reproduces the release crash WITH symbols):

| experiment | result |
|---|---|
| Part 1 applied (JIT roots in snapshot) | still SEGVs in `visit(CPI)`, obj_ptr=0x1 |
| instrumented `update_root_snapshot` | scan captures 20–37 JIT roots on ~74% of safepoints — the fix IS exercised |
| young-gen GC during the run | runs NON-MOVING while any thread is in JIT (no relocation); only ~1 young GC total |
| `CRATONVM_DBG_NO_CONC_GC=1` (disable old-gen concurrent GC) | still SEGVs |
| JIT disabled | no SEGV |
| synthetic RAF (incl. aggressive JIT forcing visit(CPI) to compile) | no SEGV |

So: relocation is prevented (non-moving while in JIT), reclamation now has the
JIT roots (part 1) AND the old-gen collector can be off — yet it still crashes.
GC is **ruled out**. The earlier "debug build is fixed by part 1" conclusion was
a **timing artifact**: part 1 adds a conservative JIT-frame scan to every
safepoint, which slows the (already byte-by-byte) real-RAF I/O enough that the
debug build times out in the *file-load* phase before ever reaching the
simulation phase where the crash lives. release is fast enough to reach it and
crashes identically.

### Real cause (refined): JIT clobbers local-0 (`this`) across a call in visit(CPI)

`LegacyInterpreter.visit(CPI)` (the real bytecode, javap-confirmed) is:

```
0:   aload_0; aload_0; getfield pc; iconst_2; iadd; putfield nextPC  // this.nextPC = this.pc+2
15:  ... invokevirtual getRegisterByte  // -> low(); a CALL
...  // flag computation into locals 6..11
160: aload_0; <nested ifeq/ifne/goto -> 0|1>; putfield H:Z          // this.H = bool
199: putfield C:Z ; 205: putfield N:Z ; ...                          // more flag stores
```

Decisive evidence from `CRATONVM_DBG_JIT_PUTFIELD`: the FIRST non-canonical
receiver is `field_index=11` (a flag field, stored at offset ~160, AFTER the
call), NOT the `nextPC` putfield (field #6, offset 7, BEFORE the call, same
`aload_0 this`). Since the one-shot did not fire on `nextPC`, `this` was VALID at
offset 7 and CORRUPTED to `0x1` by offset 160 — i.e. **`this` (local 0) is
clobbered across the `getRegisterByte`→`low()` call**. The clobber value `0x1`
is a boolean/small-int. This is a JIT **register/spill preservation** bug around
a method call, NOT GC and NOT the putfield codegen itself.

Suspected mechanism (Windows x64): the JIT maps locals to callee-saved GPRs and
spills register-resident locals to frame slots before a safepoint call
(`emit_pre_safepoint_spill`). A frame-layout overlap — e.g. local-0's spill slot
falling in the 32-byte shadow space the callee may write, or a `used_callee_saved`
gap so a callee clobbers a callee-saved reg holding `this` — would corrupt
local 0 across the call. Why only real-RAF: `visit(CPI)` is JIT-compiled only
when the real ELF program contains CPI instructions; synthetic RAF feeds
different bytes (different simulated program), so `visit(CPI)` never compiles —
which is exactly why aggressive-JIT + synthetic does NOT reproduce.

Reproduction status: NOT minimally reproducible yet — four faithful Java
reconstructions (`scratch/cpi/Cpi*.java`: the putfield pattern, the
getRegisterByte/low call, the visitor invokeinterface double-dispatch, and a
register-pressure-heavy callee) all produce CORRECT results on CratonVM. The bug
needs the exact register allocation / frame offsets the real (large) method
produces. Next steps: (1) instrument the JIT to dump, for `visit(CPI)`, which
storage holds local 0 and whether it is in `used_callee_saved` / its spill-slot
offset vs the shadow-space range; (2) or pin local 0 to a non-clobbered slot and
confirm the crash disappears, then fix the frame-layout/preservation gap.

**NOT acceptable** (no-mask rule): a non-canonical-receiver guard in
`jit_putfield_int` to skip/deopt the write.

Reproduce: `CRATONVM_REAL_RAF=1 [CRATONVM_DBG_JIT_PUTFIELD=1] cratonvm --jar
dacapo.jar avrora -s small`; symbolize `hs_err` RVAs with `CRATONVM_SYMBOLIZE=...`
on the same binary (use `--profile release-with-debug` for symbols).

## Part 2 — DEEP DIVE RESULT: a runtime JIT spill-slot corruption (not yet fixed)

A second, exhaustive pass (3 parallel x64.rs audits + bytecode extraction + JIT
instrumentation) localized the mechanism precisely. Each step ruled a layer out
(all on `release-with-debug`, which reproduces the release crash with symbols):

- **Ground truth (frame dump):** `LegacyInterpreter.visit(CPI)` has
  num_locals=12, max_stack=8; local 0 (`this`) is graph-colored to RDI (reg 7),
  and ALL 7 Windows LOCAL_REGS are consumed by locals (extreme register
  pressure).
- **Not GC** (young GC is non-moving while in JIT; disabling concurrent old-gen
  GC doesn't help; JIT-off doesn't crash).
- **Not register-clobber of `this`:** forcing local 0 to SPILL
  (`assignments[0]=None`) does NOT fix it — the crash just moves to the
  structurally identical `visit(CPC)`.
- **Not inlining** (disabling method inlining doesn't help).
- **Not call-boundary preservation:** a post-call reload of register-mapped
  locals + hoisting the inline-IC-cascade spill (so all dispatch paths spill
  before the CALL) does NOT fix it.
- **Not the compile-time operand-stack ORDER:** instrumenting the putfield
  emission (`CRATONVM_DBG_JIT_PF11`) shows the codegen MODEL is CORRECT — at the
  faulting field-11 store the receiver is `obj_slot=Frame(base_spill+0)` (=
  `this`) and `val_slot=Frame(base_spill+8)`. The JIT loads the right slot as the
  receiver.

**The actual bug:** at RUNTIME, `Frame(base_spill+0)` holds `0x1` (a boolean),
not `this`, by the time the field-11 putfield executes. So the receiver's
canonical operand-stack slot is being **overwritten with a computed boolean
during the nested-branch flag computation** in these AVR compare-instruction
visitors (`this.<flag> = <0|1 via nested ifeq/ifne/goto>`), while the
compile-time model still believes `this` lives at `base_spill+0`. The model is
right; the emitted code writes the boolean into the receiver's slot on some
control-flow path. This is a JIT **branch/merge spill-slot codegen** bug
(`branch_target_stack_depth` + `canonicalize_stack` + the dead-code stack
reconstruction at x64.rs ~9954, and the live-merge guard at ~9974 which skips
`canonicalize_stack` when `stack.len() != expected_depth`). A static trace of
visit(CPI)'s H/C/N/V/S stores looked self-consistent, so the divergence is in
the actual emitted slot WRITES, not the model.

**Next step (clear):** disassemble the generated machine code for visit(CPI)
around bytecode pc 229–285 (the V/S flag stores) and find the instruction that
writes the boolean to `[rbp-(base_spill+0)]`; or emit a runtime write-guard on
`base_spill+0`. Reproduction methodology (re-add these gated diagnostics; all
were removed to keep the tree clean):
- in `Compiler::new` (x64.rs ~3958): dump frame layout + `local_assignments`
  for methods whose `field_info` contains `field_index==11`.
- in the top-level putfield handler (x64.rs ~12825): print `obj_slot`/`val_slot`
  for `field_index==11`.
- `regalloc.rs` after `color_graph`: optional `assignments[0]=None` to spill
  `this`.
A real secondary defect was also found and should be fixed: the dead-code stack
reconstruction (x64.rs ~9955) rebuilds `self.stack` but NOT
`self.stack_oop_marks`, desyncing the oop map at every dead-merge (a GC/oop-map
correctness gap, distinct from this SEGV).

## Part 2 — FIXED (2026-06-02): the `reset_spills` receiver-slot clobber

The "actual emitted slot WRITES" the deep dive predicted were wrong turned out
to be exactly `reset_spills` in `jit/src/x64.rs`:

```rust
// BEFORE (buggy)
fn reset_spills(&mut self) {
    self.next_spill_offset = self.base_spill_offset;   // ignores live stack depth
}
```

The flag-store pattern `aload_0; <bool via ifeq/iconst/goto>; putfield flag:Z`
keeps the receiver (`this`) live on the operand stack at `base_spill+0` while the
`ifeq`/`if_icmp`/`goto` runs. Those branch handlers call `reset_spills()`, which
reset `next_spill_offset` back to `base_spill_offset`. The next `push_stack`
(the `iconst_0/1`) was then handed `Frame(base_spill+0)` — the receiver's own
slot — and stored the boolean there. At the `putfield` the receiver read back as
`0x1` and the store targeted `0x1 + HEADER + 11*SLOT = 0xD9` → SIGSEGV. This
matches the captured fault exactly (`obj_ptr=0x1`, `val=0x1`, `field_index=11`,
write target `0xD9`).

```rust
// AFTER (fixed) — reset only to just past the highest *live* stack slot
fn reset_spills(&mut self) {
    let mut next = self.base_spill_offset;
    for &slot in &self.stack {
        if let StackSlot::Frame(off) = slot {
            next = next.max(off + 8);
        }
    }
    self.next_spill_offset = next;
}
```

This is the same invariant `canonicalize_stack` (x64.rs ~4364) and the dead-merge
reconstruction (~9904) already use: live stack entry `i` owns `base_spill + i*8`.
Verified: `JIT-PFI-BAD` no longer fires under
`CRATONVM_REAL_RAF=1 CRATONVM_DBG_JIT_PUTFIELD=1`; synthetic avrora still exits
0; `cargo test -p cratonvm-jit --lib` = 686 passed.

## Part 3 — second blocker: corrupted MIC/PIC slot pointer in commit()→advance

With Part 2 applied, real-RAF avrora runs past `visit(CPI)` and then SEGVs with a
**non-deterministic** `EXCEPTION_ACCESS_VIOLATION (execute)` — a distinct, older
bug the prior experiments never reached.

Captured via a new VEH register dump (`crash_handler.rs`) + the
`current_jit_callee` thread-local (read with `CRATONVM_DBG_JIT_PUTFIELD=1`):

```
#  EXCEPTION_ACCESS_VIOLATION (execute) at pc=0x000000004F060000
#  thread: "Thread-4"
Registers:
  rax=0x00007FF736710188 (exe+0xE60188)   rcx=rbx=rsi=0x40E67810 (heap obj)
  rsp=0x000000004EE5DE08  rbp=0x000000004EE5DE90
  r10=0x000000004F060000  rip=0x000000004F060000   ← r10 == faulting target
current_jit_callee = avrora/arch/legacy/LegacyInterpreter.commit()V
```

Facts established:

- **Not the documented bug, not GC, not a float.** Reproduces with `--Xmx 8000m`
  and `CRATONVM_DBG_NO_CONC_GC=1`. The target `0x4F0X0000` is **64KB-aligned**
  (varies by `0x10000`, the Windows allocation granularity) and is ~2 MB **above
  `rsp`** — i.e. an address inside the *thread stack* region, NOT heap/code/float.
- **The faulting site is `commit()`'s `invokevirtual MainClock.advance:(J)V`**
  (bytecode pc 17; receiver `this.clock`, arg `(long)this.cyclesConsumed`).
  `commit()` has no branches, so the Part-2 fix does not touch its codegen — this
  crash is independent and pre-existing.
- **R10 holds a stack address where a MIC/PIC slot-box pointer belongs.** The
  inline monomorphic/polymorphic cache dispatch (x64.rs ~16187 / ~15912) emits
  `MOV R10, <imm64 = &JitMICSlot/JitPICSlot>` then `CALL qword [R10 + entry_off]`.
  There is **no** direct `call r10`/`jmp r10` anywhere in the codegen, yet the
  fault has `r10 == rip == 0x4F060000`. So R10 — which should be a heap `Box`
  address (~`0x40E…`, like rcx/rbx/rsi) — has been mis-loaded or clobbered to a
  stack address before the indirect call. R10 is also the JIT's dedicated
  bounds-check / SIMD scratch (x64.rs ~3492), and the IC imm64 is rewritten by
  the unroll duplicator via `ic_patches` (x64.rs ~3678) — both are prime suspects.

### Part 3 — ROOT CAUSE + FIX (2026-06-02): JIT code use-after-free on deopt

The R10 "stack address" was a red herring — the decisive signal was that the
faulting target (`0x4F0X0000`) is **64KB-aligned, unmapped, and shifts with the
address-space layout** (with `RUST_MIN_STACK=64MB` it moved to `0x60E60000`).
That is the signature of a **freed JIT code buffer**: code is allocated with
`VirtualAlloc`/`mmap` at 64KB granularity (`jit/src/platform.rs`), and an
`execute` fault at such a base means a call/branch landed on a buffer that was
already returned to the OS.

Confirmed by experiment: leaking every `ExecutableBuffer` instead of freeing it
makes the SEGV **disappear** (avrora then runs to completion — only the
pre-existing, JIT-independent digest mismatch remains, exactly as with JIT off).

**Mechanism.** `ExecutableBuffer::drop` (`jit/src/lib.rs`) `VirtualFree`s the
code. A `CompiledMethod` is dropped — and thus its code freed — when it is
removed from the JIT cache, notably by
`DeoptimizationController::deoptimize` → `jit_cache.remove(...)`
(`vm/src/jit/helpers.rs` ~2823) on `ReceiverTypeChanged` / `ClassCheck` /
`ClassLoading` / speculation failure. But other compiled methods contain
**baked-in direct `CALL rel32`** instructions (and cached MIC/PIC entry pointers)
that target that code — emitted by `direct_calls` / `try_jit_compile_callee`
(interpreter.rs). There is **no back-reference mechanism** to find and patch those
inbound call sites, so after the callee's code is freed they dangle, and the next
call through one faults (execute) at the now-unmapped 64KB buffer base. avrora is
hot and deoptimises in the simulation dispatch path
(`commit → MainClock.advance → DeltaQueue.advance/advanceSlow → Link.fire`), so
this fires there. Not GC, not a float, not the spill bug.

**Fix (`jit/src/lib.rs`):** retain JIT code for the process lifetime —
`ExecutableBuffer::drop` no longer frees the mapping (it accounts the bytes in
`RETAINED_JIT_CODE_BYTES` and leaves the region mapped + registered). Freeing is
unsafe until the JIT tracks inbound call sites and patches/invalidates them at a
safepoint before reclamation (a real code-cache sweeper — the proper long-term
fix). `CRATONVM_JIT_FREE_CODE=1` restores the old free-on-drop behaviour for A/B
testing only (it reintroduces the UAF).

**Verified:** with the fix, `CRATONVM_REAL_RAF=1` avrora no longer hits the
execute-fault SEGV (was 5/5 crashes; now runs to completion). Synthetic avrora
still exits 0; `cargo test -p cratonvm-jit --lib` = 686 passed.

**Diagnostics added this session (kept):** the Windows VEH (`crash_handler.rs`)
now dumps the x64 GPRs, the `current_jit_callee` (when
`CRATONVM_DBG_JIT_PUTFIELD=1`), and VirtualQuery-guarded code bytes preceding each
JIT return address + memory around R10 — this is what localized the bug.

### Part 4 — FIXED (2026-06-02): avrora digest mismatch = `LinkedList.iterator().remove()`

The real-RAF avrora digest mismatch (EXIT 127) was NOT a simulation-correctness
bug — avrora's `stdout` is correct. DaCapo digests the benchmark's captured
`System.err` and expects it EMPTY (`0xda39a3ee…` = SHA-1 of ""); ours was
non-empty (and non-deterministic across runs). Capturing the tee'd `System.err`
(it also mirrors to the real process stderr) showed a Java exception trace:

```
java.lang.UnsupportedOperationException: remove
  at avrora.sim.radio.Medium$Receiver.earliestNewTransmission(Medium.java:496)
  ...
```

`Medium$Receiver.earliestNewTransmission` does `transmissions.iterator().remove()`
on a `java.util.LinkedList` (javap-confirmed: `transmissions = new LinkedList()`,
bytecode `invokeinterface Iterator.remove ()V`). The message `remove` is the JDK
**default `Iterator.remove()`** (`throw new UnsupportedOperationException("remove")`).

Root cause: `invokeinterface Iterator.remove()V` is force-routed (via
`force_native_over_real_jdk_bytecode` + `intercept_force_registered_native`) to
the receiver-dispatching native `native_itr_remove_noop`
(`native-collections/src/lib.rs`). That dispatcher matches `HashMap$KeyItr`,
`ArrayList$Itr`/`$ListItr`, `TreeSet$Itr` and routes them to their working
removes, but **had no arm for `LinkedList$Itr`** (the class our native
`LinkedList.iterator()` returns), so it fell to the `_ => UOE("remove")` arm —
even though `native_ll_itr_remove` exists and works. (`hasNext`/`next` worked
because they are *abstract* in `Iterator` and dispatch straight to the
receiver-class natives; only `remove`, which has an interface default, was
force-routed through the dispatcher.)

Fix (one line): add `"java/util/LinkedList$Itr" => return native_ll_itr_remove(...)`
to `native_itr_remove_noop`. Verified: `LinkedList.iterator().remove()` now works
in isolation (`[x,z]` after removing the middle element), the avrora UOE is gone,
and `Digest validation failed` no longer appears.

### Part 5 — FIXED (2026-06-02): JIT *metadata* use-after-free (completes Part 3)

The "rare worker-thread memory corruption" (a `core::fmt` slice panic on a
corrupted class/method string `avrora/sim/clock/MainClock.<NUL bytes>`, plus
`read at 0x4`/`0x3D`) was the SAME use-after-free as Part 3, but for the JIT
*metadata*, not the code. Backtrace (isolated clean build):
`jit_invoke_virtual_mic → invoke_or_native → invoke_on_class_shared_inner →
tracing log → str slice panic`. The MIC dispatch read a **freed `JitInvokeInfo`**
(garbage class/method name with embedded NUL).

Part 3 retained the executable code (`ExecutableBuffer::drop`) but the emitted
code also holds RAW pointers into the owning `CompiledMethod`'s
`_jit_strings` / `_jit_invoke_infos` / `_jit_mic_slots` / `_jit_pic_slots`
(jit/src/lib.rs). Those boxes were still freed when a deoptimised/evicted
`CompiledMethod` dropped, so the retained code dangled into them.

Fix: `Drop for CompiledMethod` now leaks (`mem::forget`) that metadata for the
process lifetime too, exactly mirroring the code retention — the two MUST go
together. `CRATONVM_JIT_FREE_CODE=1` restores full freeing.

**Verified (isolated clean worktree at the pre-audit checkpoint):** real-RAF
avrora went from 5/5 corruption crashes → **0/8 crashes**, no digest failure, no
UOE. Our `LinkedList.iterator().remove()` was also stress-tested vs real JDK
(remove first/last/consecutive/all/then-add) — byte-identical, so the remaining
issue below is NOT a remove() bug.

### Still open — avrora simulation does not terminate within the watchdog

With all crashes + the digest pollution gone, real-RAF avrora now runs the radio
medium *correctly* — nodes exchange real packets (`<====`/`---->` with data),
which they never did before (the broken `it.remove()` had left the medium's
transmission list unmanaged). But the simulation advances far past the old
UOE-truncated stop (`~2.07M` cycles) to tens of millions of cycles and the 120s
stack-dump watchdog aborts it (main parked in `RippleSynchronizer.join` →
`Thread.join`; worker threads still progressing, events distinct = not a tight
loop). Open question: genuinely non-terminating (a correctness divergence vs the
reference simulation) or just slower than HotSpot past the 120s default watchdog.
Next: run with `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` + a long timeout to see if it
completes; if not, diff our event stream against a real-JVM avrora run to find the
divergence point.

**Findings (2026-06-02), reframing what "pass" means here:**
- **Real JDK 25 also FAILS this benchmark** (`avrora -s small`): it fails at
  startup (4 lines, no banner, no simulation — the 2009 DaCapo avrora uses
  JDK APIs changed/removed since). Its digest fails too. So the benchmark's
  built-in empty-`stderr` digest (`0xda39a3ee…`) is a JDK-6-era artifact not
  achievable on any modern runtime, and there is **no working reference run** to
  diff against on this box.
- **Synthetic avrora (`CRATONVM_REAL_RAF` unset) is a false pass:** it completes
  EXIT 0 in seconds but prints **zero** simulation events — the synthetic
  `RandomAccessFile` shim short-circuits ELF loading so avrora simulates nothing.
  This is exactly the kind of stub the project forbids; the green check is empty.
- **With real RAF + all the fixes above, our VM runs the real simulation further
  than JDK 25 does** — it loads the TinyOS ELFs and the nodes exchange real radio
  packets. The watchdog-off run (>7 min) keeps advancing (tens of millions of
  cycles, distinct events) without terminating. Whether that is a genuine
  non-termination/divergence or just our interpreter being far slower than HotSpot
  over a long simulation is the open question — and it needs a *working* reference
  avrora (an older JDK, or a standalone avrora jar run outside DaCapo) to settle.
