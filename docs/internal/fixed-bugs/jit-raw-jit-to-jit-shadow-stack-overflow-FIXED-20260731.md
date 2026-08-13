# Raw JIT-to-JIT direct calls overflowed the shadow stack — FIXED, gate reopened

**Status: RESOLVED (2026-07-31).** `direct_jit_callee_calls_enabled()` no longer
consults the moving-young flag; the raw JIT-to-JIT edge is back on by default,
with `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` as the only off-switch. The
`=force` experiment lever is gone with it.

## The bug, in one paragraph

The inline PIC cascade's **inter-slot `JNE` was a `rel8`**, sized by a comment
claiming "a single slot body is ~30 bytes for n<=5". That stopped being true: a
slot body had since grown the post-call innermost-RBP republish and the
callee-deopt service check, putting it well past 127 bytes. The patch loop
computed the displacement as `i64` and then wrote `rel as u8` — a silent
truncation guarded only by `debug_assert!`, i.e. guarded only in the build where
it cannot happen. In release the branch became `JNE -128`, landing **inside the
pre-call shadow-stack push**. Executed from that offset the push ran as a loop
with nothing reloading `top`: it marched off the end of the thread's 2 MiB shadow
buffer, kept storing through the mimalloc arena behind it (including the
`JvmThread`), and faulted ~170 MiB later at the arena end. Only reachable with
raw JIT-to-JIT calls enabled, which is why closing that gate "fixed" it.

## What the previous rounds got wrong

* **Round 1** blamed the frame shape: "a raw JIT-to-JIT call leaves an unguarded
  callee frame, so a GC at that boundary can select an incompatible oop map and
  RECLAIM A LIVE ROOT". Measured, no. `chain_entry_rbp_is_foreign` detects the
  unguarded frame (`FOREIGN_INNERMOST_RBP` 4115×/run), the moving-young coverage
  proof comes back incomplete, `moving_young_precise_only` refuses it, and the
  cycle diverts to the non-moving sweep. Nothing is reclaimed. The cost of an
  unguarded callee frame is **precision, not correctness** — which is why gating
  the edge on the collector's mode bought no safety and only hid a machine-code
  bug behind a flag.
* **Round 2** correctly identified the *symptom* — a shadow-stack push running
  off the end of the buffer — but concluded "at least one more unbalanced push
  lives in the single-pass backend" and went looking for a push/reload pairing
  defect. There was none. The compiler's pairing is correct; the *branch* was
  wrong, and it jumped into the middle of a correct push.

## How it was actually found

1. Emit the `end` overflow guard (below). The gate-open run stopped SIGSEGVing
   and started **hanging** instead, with one thread pegged at 100%.
2. `kill -SEGV <hot tid>` with `CRATONVM_DBG_JIT_NAMES=1`. The crash handler
   named the spinning body: `ConcurrentReferenceHashMap$Segment.getReference`.
   (`gdb` cannot attach on the Azure host — `ptrace_scope=1`.)
3. `CRATONVM_JIT_SP_INLINE_IC=0` (new lever) made the class pass 26/26 with the
   gate still open ⇒ the defect is in the inline MIC/PIC cascade, not in the
   raw static/special direct call and not in the sibling tail-call.
4. `CRATONVM_DBG_DUMP_JIT='Segment.getReference'` + `objdump -D -b binary`.
   The faulting PC was `entry+0x14ad`, inside the push, and 0x71 bytes further
   on sat `75 80` — `jne -128` — pointing straight back into it.

## Fixed by this change

1. **`jit/src/x64.rs` — the PIC inter-slot branch is `rel32`** (`0F 85 cd`),
   patched with `try_patch_i32`. No distance cliff.
2. **`Compiler::patch_rel8_or_bail`** replaces every remaining `rel as u8`
   displacement patch in the single-pass backend. A `rel8` that does not fit now
   marks the buffer overflowed — the driver discards the half-emitted method and
   falls back — instead of retargeting the branch. Pinned by
   `rel8_patch_out_of_range_bails_instead_of_truncating`. (The IR tier already
   uses `rel32` throughout, after two earlier bugs of exactly this shape.)
3. **Both backends now emit the `end` overflow guard.** `ShadowStack::END_OFFSET`
   had always been documented as *"read by the JIT-emitted overflow guard"* and
   `ShadowStack::push` had always returned `false` on overflow — but neither
   `x64::emit_shadow_push` nor `ir_lower::emit_shadow_push` emitted a bounds
   check, so an overflow was silent heap corruption rather than a bail. The
   guard is `LEA/CMP/LEA/JA` (LEA does not touch flags, so the bump is undone
   between the compare and the branch and R11 stays the only scratch).
   * **Reload-skip protocol.** On a bail nothing is stored and `top` is left
     alone; the push marks its saved-base slot with **bit 0** (slot addresses
     are 8-aligned, so the bit is free) and the paired reload branches on that
     tag to a tail that only restores `top`. Without it the reload would copy
     slots the push never wrote back into live home registers, and the
     savebase-out-of-range recovery path — which pops unconditionally — would
     under-pop.
   * Bails are counted in `cratonvm_jit::SHADOW_OVERFLOW_COUNT`, reported once
     at the next JIT→Rust boundary and in every crash report
     (`shadow_overflow_bails=`). `CRATONVM_SHADOW_OVERFLOW_DIAG=1` additionally
     records the bailing method's name.
4. **`emit_epilogue_without_ret` restores the shadow `top` watermark.** The
   sibling tail-call is a method exit — the callee returns straight to *our*
   caller — so nothing downstream would ever put `top` back where this
   activation found it, and a caller that tail-calls from inside a loop
   accumulated one leak per iteration. The real epilogue had always restored it.
5. **The two stale hazard comments are corrected**, in
   `jit/src/lib.rs::direct_jit_callee_calls_enabled` and at the inline MIC/PIC
   emission site in `x64.rs`.

### New levers

| variable | effect |
|---|---|
| `CRATONVM_SHADOW_NO_END_GUARD=1` | suppress the `end` guard (restores the pre-fix corrupting behaviour; only useful to confirm a failure *is* the overflow) |
| `CRATONVM_SHADOW_OVERFLOW_DIAG=1` | record the bailing method's name |
| `CRATONVM_JIT_SP_INLINE_IC=0` | single-pass inline MIC/PIC cascade off, raw static/special direct calls still on |
| `CRATONVM_JIT_SP_TAILCALL=0` | demote the single-pass sibling tail-call to an ordinary CALL |

## Validation

`org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
(`module/spring-boot-webmvc`), Azure Linux host, JDK 25.

| arm | result |
|---|---|
| before, gate closed (the shipping default) | 26/26 |
| before, gate forced open | SIGSEGV ~1 s, 4/4 |
| `end` guard only, gate forced open | no SIGSEGV — hangs instead, one thread pegged inside `Segment.getReference`. The wild `JNE` is still there; the guard only stops it corrupting the heap. |
| `end` guard + `CRATONVM_SHADOW_NO_END_GUARD=1`, gate forced open | SIGSEGV — same binary, differs only by the env var |
| `end` guard + `CRATONVM_JIT_SP_INLINE_IC=0`, gate forced open | 26/26 — pins the defect to the inline MIC/PIC cascade |
| **all fixes, gate open (the new shipping default)** | **26/26 × 14 consecutive runs** |
| all fixes, `CRATONVM_SHADOW_NO_END_GUARD=1` | 26/26 — with the branch fixed the guard has nothing left to catch on this workload |
| all fixes, `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | 26/26 — the off-switch still works |

Also `regression-suite/run.sh` (19 classes, CratonVM vs HotSpot differential):
19 passed, 0 failed. `cargo test -p cratonvm-jit --release --lib`: 1064 passed,
0 failed.

**Harness note.** The host was carrying load averages of 60–620 from other
sessions. Running seven VMs at once produced one OOM-kill and several
`HttpClient request timed out` single-test failures — in the gate-*closed*
control as much as in the gate-open arm, so they are contention, not this edge.
The 14-run acceptance above was run two at a time and was clean throughout. If
you re-run this and see a lone timed-out request, check the load average before
reading anything into it.

## Residuals

* An unguarded callee frame still costs the moving-young coverage proof: with
  the edge open, cycles that see one fall back to the non-moving sweep. That is
  a *precision* cost and it predates this work — the hashed megamorphic stub
  (`runtime_lowering::emit_hashed_vtable_stub`) has never been gated and already
  produces the same frames on the default path. Making an unguarded callee frame
  describable to the root scan remains worthwhile, but it is not a correctness
  blocker and it is not this document's bug.
* `jit/src/lib.rs`'s IR buffer estimate (`nodes*32 + calls*448 + 1024`) was
  previously blamed for a `JIT try_patch_i32: offset out of bounds` flood under
  the open gate. That flood is at 0 — `runtime_lowering::emit_post_call_frame_republish`
  now prefers the 9-byte inline TLS store over a PUSH/SUB/CALL/ADD/POP sequence,
  which shrank IR bodies enough on its own.
