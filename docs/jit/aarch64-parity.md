# AArch64 JIT backend — parity census against x86-64

**Scope:** `jit/src/aarch64.rs`, `jit/src/aarch64_backend.rs`, `jit/src/platform.rs`,
compared against `jit/src/x64.rs` + `jit/src/x64/*`.

> **Nothing described here has been executed on AArch64 hardware.** Every claim
> below was established by reading code, and every fix was validated only by
> host-side unit tests on x86-64 Windows — which for this backend means
> *instruction-encoding* tests (fixed-width AArch64 words checked against the
> ARM ARM) and *compiler-policy* tests (does this method compile or bail).
> No AArch64 binary has been built, booted, or run. Treat the "Fixed" column as
> "the code now says the right thing", not as "verified working".

---

## 0. The premise correction that governs everything below

The task this audit was commissioned under assumed the AArch64 backend has a GC
write barrier, a SATB pre-barrier, inline caches, deopt metadata and implicit
null checks that might diverge from x86-64's. **It has none of them, because it
has no object model at all.**

`jit/src/aarch64_backend.rs` (module header, and `tests::object_model_opcodes_are_all_unsupported`)
refuses every one of: `getfield`/`putfield`/`getstatic`/`putstatic`, all array
load/store opcodes, `new`/`newarray`/`anewarray`/`multianewarray`/`arraylength`,
`checkcast`/`instanceof`, `monitorenter`/`monitorexit`, `athrow`, and every
`invoke*` form (`Arm64Backend::emit_invoke` sets `self.failed` unconditionally —
there is no call-target resolution here).

So the honest census answer for most rows is not "aarch64 does it wrong" but
"aarch64 cannot reach the situation, because the opcode that creates it bails
the whole method". That is a *sound* posture — the VM permanently bail-lists the
method and interprets it — and it is why this backend, despite being wildly
incomplete, is not actively unsafe in those areas.

The rows that *did* turn out to be actively unsafe are the ones that have
nothing to do with the object model: the safepoint gap, the AAPCS64
callee-saved-FP violation, the FP frame-slot addressing bug, the missing stack
bang, and the missing Windows-on-ARM i-cache flush.

---

## 1. Census table

| Mechanism | x86-64 | AArch64 | Status |
|---|---|---|---|
| **Safepoint poll (entry)** | `Backend::emit_safepoint_poll_prologue`, reads `helpers.safepoint_flag_addr`, on by default (`jit_safepoint_polls_enabled`) | none, and none possible (no helper address plumbed in, and a poll needs a CALL which `emit_invoke` refuses) | **Accepted gap.** A straight-line compiled body runs for a bounded, tiny time and the thread reaches a poll again as soon as it returns to the interpreter. |
| **Safepoint poll (loop back-edge)** | `Backend::emit_safepoint_poll` at every back-edge | none | **FIXED — now bails.** `Arm64Backend::label_for_pc` refuses any method with a backward branch target. Previously a compiled loop was a region with *no* safepoint of any kind; a thread inside one never observes a stop-the-world request and hangs the collector, and the compilable population is "leaf pure arithmetic", i.e. mostly loops. |
| **Oop-map publication** | precise per-safepoint `OopMapEntry` maps, keyed by native PC | `oop_maps` is unconditionally empty; GC walker takes its conservative stack scan | **Sound (superset), now fail-closed.** The dead writer `emit_oop_map_for_safepoint` keyed maps off `instruction_count * 4`, which is wrong for this pseudo-op stream (`Label`/`Comment` emit 0 bytes, `ConstantPoolEntry` emits 8, `MovImm`/`AddImm`/`CmpImm`/far `Ldr`/`Str` expand to 1–4 words). It now sets `failed`, so the first caller cannot silently inherit a mis-keyed map. |
| **GC write barrier (card / remembered set)** | inline reference store + `helpers.write_barrier`; the `GC_FLAG_OLD_GEN` young/old test is inline | **unreachable** — no `putfield`, no `putstatic`, no `aastore`, no `new`. There is no reference store to guard. | N/A by construction. The `GC_FLAG_OLD_GEN`-on-promotion hazard this branch found in the x86 inline store has **no AArch64 analogue**: the analogous store does not exist. |
| **SATB pre-write barrier** | `helpers.satb_pre_write_barrier` called before overwriting a non-null old ref | **unreachable** (same reason) | N/A by construction. |
| **Implicit null check / signal contract** | **none — this row used to claim one.** Every field and array access carries an EXPLICIT check (`emit_null_check_*`, `emit_trusted_oop_receiver_check`), elided only where `null_check_elim` proves the receiver non-null. `crash_handler.rs` is a *reporter*: it reads the faulting PC to attribute a crash to a JIT region, and never rewrites `RIP` to resume. There is no registry of recognised faulting PCs on either backend. | **unreachable** — no dereference of a Java object ever occurs | N/A by construction, and the backends **agree** here rather than diverging. The corollary holds on both: a SIGSEGV inside a compiled body is a genuine backend bug, never an implicit exception. Adding a real implicit-null-check mechanism to x86-64 is blocked on code-buffer lifetime (a freed buffer's address is reusable, so a PC-keyed recovery table needs the code-cache lifecycle to unregister with it), not on the signal handler. |
| **Explicit exception throw** | full throw/handler machinery | none; `athrow` bails, and `idiv`/`ldiv`/`irem`/`lrem` bail because their only "exception path" was `BRK #1` (SIGTRAP, which nothing converts) | Sound (refuses). |
| **Deopt metadata / frame reconstruction** | `jit/src/deopt.rs`, uncommon-trap stubs, frame state interning | absent; neither "deopt" nor "OSR" appears in the backend | Sound *today* — this is the only tier, so there is nothing to tier down from and no speculative optimisation to invalidate. It is a hard prerequisite for ever adding one. |
| **Deopt trap patching (self-modifying code)** | x86-64 patches traps into live code | **no patch sites exist** | N/A — see §3. |
| **Inline caches (MIC / PIC)** | `JitMICSlot` / `JitPICSlot`, allocated per call site | absent (no calls at all) | N/A. See §2 for the memory-model requirement any future AArch64 IC inherits. |
| **Inline TLAB bump allocation** | present | absent (no `new`) | N/A. |
| **Stack-overflow bang** | `emit_stack_bang_before_frame_alloc` probes every page the frame crosses, before moving RSP | none — the prologue is a bare `SUB SP, SP, #frame` | **FIXED — now bails** for `frame_size >= 4096`. `max_locals`/`max_stack` are class-file `u16`s, so `num_spills = gpr_spills + max_stack` could produce ~512 KiB frames that step clean past the guard page. |
| **Callee-saved register discipline (GPR)** | correct | correct — `used_callee_saved` saved/restored in prologue/epilogue | OK. |
| **Callee-saved register discipline (FP)** | correct | **was broken** — float locals were homed in `D8`–`D15`, which AAPCS64 makes callee-saved, while the prologue saved only GPRs and `Arm64FrameLayout` reserves no FP save area | **FIXED.** `compile_pass` now ignores `alloc.xmm_assignments`; float locals live in frame slots or GPRs. See §4. |
| **Frame-slot addressing (GPR)** | correct | correct since "ARM64 BUG #1" (`ldur`/`stur`, no writeback) | OK. |
| **Frame-slot addressing (FP)** | correct | **was broken** — `offset as u16` into the *scaled unsigned* form, which cannot express a negative displacement | **FIXED.** See §4. |
| **Branch-displacement overflow** | `ExecutableBuffer::overflowed` → bail | `Aarch64Emitter::overflowed()` sticky flag, read by `emit_machine_code` | OK (fixed). |
| **Unbound label / unresolved branch** | n/a | `emit_machine_code` returns `None` | OK (fixed). |
| **Wide-immediate truncation** | n/a | `emit_addsub_imm_safe` / `CmpImm` materialise into IP0 | OK, and the one unencodable shape now **bails** instead of emitting `BRK` and reporting success. |
| **32-bit int semantics** | W-form / correct wrapping | **`iadd`/`isub`/`imul`/`ineg`/`ishl`/`ishr`/`iushr` lower to 64-bit X-form** | **OPEN — silent miscompile.** See §5. Deliberately not fixed here. |
| **I-cache maintenance, Linux/FreeBSD aarch64** | n/a (coherent caches) | `__clear_cache` in `platform_make_executable`, before the RW→RX flip | OK. |
| **I-cache maintenance, macOS aarch64** | n/a | `sys_icache_invalidate` in `platform_make_executable` | OK, with an unvalidated caveat (§3). |
| **I-cache maintenance, Windows aarch64** | n/a | **was absent** — the Windows arm of `platform.rs` had no arch conditional at all | **FIXED.** `flush_icache_range_windows` (`FlushInstructionCache`) now runs before the RW→RX flip on every non-x86 Windows target. |
| **Cross-thread code publication (reader-side ISB)** | n/a (x86 is coherent + TSO) | not present anywhere | **OPEN — cross-file.** See §2. |

---

## 2. Memory model

AArch64 is weakly ordered; x86-64 is TSO. Every place the x86 backend gets an
ordering for free is a place AArch64 needs an explicit barrier. Because this
backend emits no shared-memory Java operations at all, most of these are
*prospective* — they are the bill that comes due the moment anyone extends it.

### 2.1 Code publication to other threads — the one live gap

The publication sequence today is:

1. compiler thread writes instruction bytes through the data path
   (`ExecutableBuffer::emit`, an ordinary `copy_nonoverlapping`);
2. `ExecutableBuffer::finalize()` → `platform::make_executable`, which on
   aarch64 runs `__clear_cache` (Linux/FreeBSD) or `sys_icache_invalidate`
   (macOS) or now `FlushInstructionCache` (Windows), then `mprotect`/`VirtualProtect`;
3. the `CompiledMethod` is published into the JIT cache;
4. **another** thread picks up the entry pointer and branches to it.

Steps 1–2 are the writer half of the ARM ARM's *Concurrent modification and
execution of instructions* sequence (B2.4.4) and are correct. Step 4 is the
reader half, and the architecture requires the *executing* PE to perform a
context-synchronisation event (`ISB`) after observing the publication, before
fetching the new instructions. Nothing in this codebase does that explicitly.

In practice `mprotect`/`VirtualProtect` on a multi-threaded process issues a
TLB shootdown IPI to every core, and taking an interrupt is a context
synchronisation event — which is why this shape works on real systems and why
most JITs get away with it. It is nevertheless not architecturally guaranteed,
and the guarantee evaporates if the W^X flip is ever elided (e.g. an RWX
fast path, or a dual-mapping scheme).

**This cannot be fixed inside `platform.rs`.** The reader is
`jit/src/lib.rs` / the VM dispatch path, which is outside this lane. Recorded
here as a cross-file finding; see §6.

### 2.2 Barriers that a future AArch64 backend will need, none of which exist

| Situation | x86-64 relies on | AArch64 needs |
|---|---|---|
| Publishing an inline-cache slot (`JitMICSlot::cached_class_id` is stored `Release` *after* `cached_entry_ptr`) | TSO makes the compiled code's two plain loads un-reorderable | the *reader* — generated code — must acquire: `LDAR` on `cached_class_id`, or a `DMB ISHLD` between the guard load and the entry load. A plain `LDR`/`LDR` pair may read a stale `cached_entry_ptr` behind a fresh `cached_class_id`. |
| `volatile` field read | plain `MOV` | `LDAR` (or `LDR` + `DMB ISHLD`) |
| `volatile` field write | plain `MOV` + `MFENCE`/`XCHG` for the StoreLoad edge | `STLR`, plus `DMB ISH` where a StoreLoad edge is required |
| `monitorenter` / `monitorexit` | `LOCK CMPXCHG` (implicitly full-barrier) | `LDAXR`/`STLXR` pair, and the unlock needs release semantics |
| Object publication (constructor `final` freeze) | TSO ordering of the stores | `DMB ISHST` before publishing the reference |
| Patched branch target in live code | store + any serialising instruction on the executing core | full DC CVAU / DSB ISH / IC IVAU / DSB ISH / ISB, plus the reader-side ISB of §2.1 |

The barrier *encoders* exist and are now pinned by exact-byte tests
(`aarch64::tests::test_barrier_encodings`): `DSB SY` `0xD5033F9F`, `DSB ISH`
`0xD5033B9F`, `DMB SY` `0xD5033FBF`, `DMB ISH` `0xD5033BBF`, `DMB ISHST`
`0xD5033ABF`, `ISB SY` `0xD5033FDF`. **Nothing in the backend emits any of
them.** That is correct today (nothing needs one) and is a trap tomorrow.

The `DMB ISH` and `ISHST` values above read `0xD50333BF`/`0xD50332BF` when
this doc was written, and the test carried the same two wrong literals until
the suite was first run against it. The encoding is `base | CRm << 8`, so
`CRm` lands in the third hex digit from the right. Both were written from one
mistaken reading, which is precisely why this doc is the wrong thing to check
the test against — check both against the ARM ARM.

---

## 3. Instruction-cache maintenance — every patch site

The task asked for a sweep of "every patch site, including inline-cache updates
and deopt trap patching". The finding is that **there are none**.

* Within-compilation patching (`Aarch64Emitter::patch_branch`, `patch_bcond`,
  `patch_adr`, `patch_ldr_literal`) mutates a plain `Vec<u8>` *before* the bytes
  are ever copied into executable memory. No cache maintenance is required or
  possible at that point; the single `__clear_cache` / `sys_icache_invalidate` /
  `FlushInstructionCache` at `make_executable` covers the whole buffer.
* After finalisation there is no code patching at all. A workspace grep for
  `ExecutableBuffer::make_writable()` finds **zero production callers** — only
  `jit/src/lib.rs`'s own round-trip unit test. The three x86-64 mechanisms that
  *sound* like self-modifying code are not:
  - **Inline caches** are heap data cells (`JitMICSlot` / `JitPICSlot`) that
    compiled code *loads from*. Updating one is a data store, not a code
    write, so it needs the release/acquire pair of §2.2, not i-cache
    maintenance.
  - **Deopt traps** do not exist on aarch64.
  - **Direct-call baking** does not exist on aarch64 (no calls).
* Therefore `platform_make_executable` is the *only* place that needs the
  sequence, and after this audit all three OS families do it. The remaining
  hole is the reader-side `ISB` of §2.1.

**Unvalidated caveats on the macOS path.** `platform_alloc` maps with `MAP_JIT`
but nothing ever calls `pthread_jit_write_protect_np`. On Apple Silicon,
`MAP_JIT` pages under the hardened runtime are governed by the per-thread
W^X toggle, and `mmap(MAP_JIT)` itself requires the
`com.apple.security.cs.allow-jit` entitlement. Whether the current
allocate-RW → write → `mprotect`-RX shape works depends on entitlement and
hardening configuration. Not changed, because it cannot be tested from here and
a wrong guess would break the one aarch64 path that may currently work.

---

## 4. What was fixed

All four fixes are in the three files owned by this lane.

### 4.1 `platform.rs` — Windows-on-ARM i-cache flush

`platform_make_executable`'s Windows arm was `#[cfg(target_os = "windows")]`
with no architecture conditional: it called `VirtualProtect` and nothing else.
The Unix arm had acquired an arch-conditional `__clear_cache` when the
Linux/FreeBSD aarch64 port landed; the Windows arm never did. On
`aarch64-pc-windows-msvc` that means every JIT body was published without ever
invalidating the I-cache — the exact failure the Unix comment block describes,
on the one platform where nobody looked.

Added `flush_icache_range_windows`, calling
`FlushInstructionCache(GetCurrentProcess(), base, len)` before the RW→RX flip.
The *call* is gated to non-x86 targets (on x86/x86-64 it is a documented no-op
and would be a pure syscall on the JIT hot path), but the *function* is compiled
on every Windows target, so a broken `extern` declaration is caught by an
ordinary x86-64 Windows build rather than by a cross-build nobody runs.

Test: `platform::tests::windows_icache_flush_is_callable` calls it directly on
any Windows host (including sub-page and zero-length ranges).

### 4.2 FP frame-slot addressing — the FP twin of "ARM64 BUG #1"

`Arm64FrameLayout::spill_offset` is always **negative**. The `FpLdr`/`FpStr`
lowering in `emit_machine_code` did:

```rust
emitter.str_fp_d(fp(*vt), r(*rn), *offset as u16);
```

`str_fp_d` is the *scaled unsigned-offset* form. `-24i32 as u16` is `65512`,
which the encoder then scales by 8 — a store roughly **64 KiB above FP**, i.e.
into the caller's frame. Every `fstore`/`fload` of a spilled float or double was
silent memory corruption.

This is precisely the bug that was found and fixed for the GPR `Ldr`/`Str` arms
("ARM64 BUG #1", which introduced `ldur`/`stur`); the FP arms were left behind.
The lowering now mirrors the GPR routing exactly: scaled-unsigned for
non-negative correctly-aligned offsets, unscaled `LDUR`/`STUR` for the signed
imm9 range, address-materialisation into IP0 otherwise. Four new emitters in
`aarch64.rs` (`ldur_fp_d`, `stur_fp_d`, `ldur_fp_s`, `stur_fp_s`).

Tests: `aarch64::tests::test_ldur_stur_fp_unscaled` (exact ARM ARM encodings,
each equal to the corresponding GPR word with the V bit set),
`test_fp_unscaled_has_no_writeback`,
`aarch64_backend::tests::fp_frame_slot_access_uses_unscaled_form_for_negative_offsets`
(asserts the exact words `STUR D0,[X29,#-24]` = `0xFC1E83A0` and
`LDUR D1,[X29,#-24]` = `0xFC5E83A1`), plus positive-offset and far-offset
negative controls.

### 4.3 AAPCS64 violation — float locals in callee-saved `D8`–`D15`

`regalloc::ARM64_LOCAL_FPS` is `D8..D15`. On AAPCS64 those are the callee-saved
FP registers. `compile_pass` homed float locals there, but the prologue and
epilogue save/restore only `alloc.used_callee_saved` (GPRs) —
`alloc.used_xmm_regs` is never read and `Arm64FrameLayout` reserves no FP save
area. Any compiled method with a float or double local therefore destroyed the
**caller's** `D8`–`D15`. The caller is the interpreter (Rust, compiled by LLVM,
which very much does keep values in `D8`–`D15` across calls) or another compiled
frame.

Fixed by not allocating them: `compile_pass` ignores `alloc.xmm_assignments`.
Float locals live in a frame slot (via the now-correct `FpLdr`/`FpStr`) or in a
GPR via `FmovToFp`/`FmovFromFp`. The alternative — saving and restoring them —
would need an FP save area in `Arm64FrameLayout::compute` plus prologue and
epilogue emission; it is the right long-term fix and is documented at the
change site, but it is a change that can itself be wrong, and this backend has
no performance worth that risk.

Side effect: `to_fpreg`'s `debug_assert!(r.0 >= 32 && r.0 <= 39)` was a latent
debug-build panic, because `D8`–`D15` were encoded as `Arm64Register(40..47)`.
Removing the producer makes the assertion's stated invariant true.

Test: `float_locals_never_use_callee_saved_fp_regs` (replaces
`p95_backend_float_local_uses_fp_reg`, which asserted the buggy behaviour).

### 4.4 Fail-closed conversions

| Was | Now |
|---|---|
| A loop compiled with no safepoint anywhere in it | `label_for_pc` refuses any backward branch target — including switch case/default targets and the degenerate `goto .` |
| A frame of any size, with no stack bang | frames `>= 4096` bytes refuse |
| `emit_addsub_imm_safe`'s unencodable-SP case emitted `BRK #0` **and reported success** — the SP adjustment simply never happened and the first thread to arrive died on SIGTRAP, which nothing in the VM converts | returns `false`; `emit_machine_code` returns `None` |
| `emit_oop_map_for_safepoint` computed a wrong native-PC key and would have published a mis-keyed map to its first caller | sets `failed` |

Tests: `loop_method_bails_no_safepoint_poll`,
`backward_goto_and_self_loop_both_bail`, `backward_switch_case_target_bails`,
`oversized_frame_bails_no_stack_bang`,
`addsub_imm_safe_refuses_unencodable_sp_adjustment`,
`emit_machine_code_bails_on_unencodable_sp_immediate`,
`oop_map_writer_is_fail_closed`. Each has a negative control (a forward branch,
a small frame, an encodable immediate) so none of them can be satisfied by a
blanket refusal.

---

## 5. Deliberately NOT fixed: 32-bit integer width

`iadd`, `isub`, `imul`, `ineg`, `ishl`, `ishr`, `iushr` all lower to 64-bit
X-form instructions (`emit_int_add` → `Arm64Instruction::Add` → `Aarch64Emitter::add`,
`sf = true`). JVM 32-bit wrapping therefore does not happen, and `ishl`/`ishr`
do not mask the shift amount to 5 bits.

Observability is narrower than it first looks. Values enter as sign-extended
`i32` (`emit_iconst` → `MovImm { imm: i64::from(value) }`), and `ireturn` hands
back a full X register whose low 32 bits are correct — so the *result* of a
single overflowing `iadd` is right. It goes wrong when something reads the upper
bits: a signed `if_icmp*` (64-bit `CMP` on a non-canonical value gets the
comparison backwards), `i2l`, `iushr` (needs 32-bit zero-extension, and this
backend masks the value but not the shift), and mixing with `l*` ops.

**Why it is left open.** The correct fix is small and known — canonicalise with
`SXTW Xd, Wd` (`SBFM Xd, Xn, #0, #31` = `0x93407C00 | Rn<<5 | Rd`) after each
32-bit-producing op, and mask shift amounts with `AND Xd, Xn, #31`
(`0x92401000 | Rn<<5 | Rd`) — but it touches the arithmetic lowering of a
backend that cannot be built or run from this host, on a branch where eight other
agents are editing concurrently. The module header documents this as a
known, deliberate non-fix. It is recorded here as the single largest remaining
silent-miscompile risk, with the fix shape written down so the next pass is
mechanical.

Note that the safepoint fix in §4.4 substantially shrinks its blast radius:
without loops, the surviving compilable population is straight-line arithmetic,
where an overflow that is subsequently *compared* is rarer.

---

## 6. Cross-file changes needed but not made

These are outside this lane's three files. None were touched.

1. **Reader-side `ISB` on code publication** (`jit/src/lib.rs`, VM dispatch).
   See §2.1. Architecturally required; works in practice via the `mprotect` IPI.
   The right shape is an `ISB` (or a documented reliance on the syscall) on the
   path that first branches into a newly published `CompiledMethod` on a thread
   other than the compiler.
2. **`vm/src/runtime/interpreter.rs` and `vm/src/vm.rs` call `x64::compile*`
   directly** from the eager first-call, OSR and probe paths, with no
   `target_arch` guard. An aarch64 build would emit x86-64 bytes from those
   sites. Already noted in the module header ("Reachability"); still true, still
   outside this lane.
3. **`regalloc::ARM64_LOCAL_FPS`** advertises `D8`–`D15` as available for locals.
   With §4.3 the backend simply ignores it, so nothing is broken, but the
   allocator is now computing an assignment nobody consumes. If someone later
   builds the FP save area, this becomes live again; if not, the pool should be
   emptied there.

---

## 7. What remains unvalidated

* Every fix in §4, on real hardware. The encoding tests run on x86-64 and check
  bytes; they cannot check that those bytes do what the ARM ARM says.
* The macOS `MAP_JIT` / `pthread_jit_write_protect_np` question in §3.
* Whether an AArch64 build even links and boots. This lane ran no build (by
  instruction), and `jit/src/lib.rs`'s `#[cfg(target_arch = "aarch64")]` block
  has, as far as this audit can tell from the code, never been exercised in CI —
  only the unit tests inside these modules run, and those are `cfg`-free.
* The back-edge refusal's effect on the AArch64 compile rate. It is expected to
  be large (loops are most of what was compilable), and that is the intended
  trade: interpreting a loop is correct, compiling one without a safepoint is
  not.
