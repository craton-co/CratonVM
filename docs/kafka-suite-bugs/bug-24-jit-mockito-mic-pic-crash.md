# Bug 24 — JIT MIC/PIC crash on Mockito-generated virtual dispatch (SIGSEGV)

**Severity:** High — once bug-09 (Mockito) is fixed, **every** Mockito-creating
test class crashes under the default (JIT-on) config with an
`EXCEPTION_ACCESS_VIOLATION`. `--nojit` is clean.

## Symptom
```
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x0000033E0000033B
#  Faulting access: execute at address 0x0000033E0000033B
#  thread: "main-vm"
Native frames (most recent call first) [raw]:
   0: exe+0x99D1A9
   1..3: (external/jit)
   4: 0x0000033E0000033B  (external/jit)   <- jumped/called to garbage
```
The faulting `rip == r11 == 0x0000033E0000033B` — a low, heap-looking value, NOT
a compiled code address (real JIT entries are in the `0x00007FFB…` range). So
JIT code called/jumped through a **corrupt or stale code-pointer** (a MIC/PIC
`cached_entry_ptr`). Deterministic: same address across runs.

Minimal repro: `apps/kafka/tests/repro/MockProbe.java` (just
`Mockito.mock(List.class)`). Crashes JIT-on, prints
`Mockito.mock(List) OK` under `--nojit`.

## Cause (narrowed, not yet fully root-caused)
- Clean A/B: the pre-merge commit `3ac1b5c7` (bug-09 fixes, **without** the
  Tomcat-H/I JIT commit) runs `MockProbe`/`MockFull` JIT-on with no crash —
  verified dozens of times. After merging `dev`, the binary crashes. The **only**
  JIT commit in the merge range (`3ac1b5c7..b5c709c7`) is
  **`82b9bdf6` "fix(jit): Tomcat bugs H & I"**. `--nojit` avoids it. ⇒ the
  regression is the interaction of `82b9bdf6` with Mockito's heavily-polymorphic
  ByteBuddy-generated virtual dispatch.
- `82b9bdf6` made MIC/PIC entry publishing **conditional** on
  `!mic_callee_has_exception_table(...)` in `jit_invoke_virtual_mic`
  (`vm/src/jit/helpers.rs`) and gated the statically-bound sibling in
  `interpreter.rs`'s `callee_compiler`. The gating *skips publishing* a fresh
  entry for an exception-table callee but does **not invalidate** a slot that was
  already populated — and the inline x64 MIC/PIC cascade
  (`jit/src/x64.rs`, reads `JitMICSlot.cached_class_id`@0 /
  `cached_entry_ptr`@8) calls the cached entry directly in machine code,
  bypassing the helper's bug-H re-route. Leading hypothesis: a stale/recompiled
  `cached_entry_ptr` is called for a matching `cached_class_id`. (The garbage
  value looks like packed heap data, consistent with a slot whose entry field is
  out of sync with its class-id tag.)

## ROOT CAUSE (found) — use-after-free of loop-unroll-cloned inline-cache slots

Not a stale-entry / bug-H logic issue. The leading hypothesis (offset/tag
confusion, torn writes) was wrong; the offsets are correct and every *logical*
write publishes a valid entry (instrumentation showed all `entry_ptr`s in the
JIT code arena ≈ `0x4dxx0000`). The crash value (`0x0000033E0000033B`,
`0x0000033800000335` — two packed 32-bit *class ids*) is what you get when the
inline cascade reads a PIC `entry_ptrs[i]` slot whose backing `Box` has been
**freed and the memory reused for a Java object** (whose fields are class ids).

The lifetime bug: `jit/src/x64.rs::compile` moves the loop-unroll **cloned**
MIC/PIC slot boxes into `cm._jit_{mic,pic}_slots` via `extend`, and its contract
(comment at the `extend`) says the caller must attach its *owned* slots
**additively** — "without keeping them alive on the CompiledMethod … the next
invokevirtual on an unrolled copy would dereference freed memory." But
`jit/src/lib.rs::try_compile` did:
```rust
compiled._jit_mic_slots = owned_mic_slots;   // ASSIGN — drops the cloned boxes!
compiled._jit_pic_slots = owned_pic_slots;
```
overwriting (and dropping) the cloned boxes that the unrolled machine code's
baked `MOV R10, <slot>` pointers still reference. Reused memory → class-id
garbage in `entry_ptrs[i]` → `CALL R11` through it.

Why it surfaced only after the `82b9bdf6` (Tomcat H/I) merge: bug-H changed which
sites get MIC/PIC-published and promoted, which (together with Mockito's heavy
polymorphic ByteBuddy dispatch + loop unrolling) reliably hit a dropped unrolled
slot. Latent before; deterministic after.

Isolated by: `CRATONVM_JIT_NO_INLINE_VCACHE` gate (disabling the inline cascade →
`Mockito.mock OK`, proving the helper/MIC path is clean and the inline PIC read
is the fault) + `CRATONVM_DBG_BUG24` write instrumentation (proved no logical
write produces the garbage → external corruption) + reading the slot-ownership
handoff.

## FIX
`jit/src/lib.rs::try_compile`: `extend` instead of assign, so both the
loop-unroll cloned slots and the owned slots stay alive for the compiled code's
lifetime:
```rust
compiled._jit_mic_slots.extend(owned_mic_slots);
compiled._jit_pic_slots.extend(owned_pic_slots);
```
Also hardened (correct regardless): `JitMICSlot::update` and
`JitPICSlot::seed_from_mic` now publish `entry_ptr` BEFORE `class_id` (matching
`write_entry`'s documented invariant) so a concurrent inline reader can't pair a
new class id with a stale entry.

## Status
FIXED (branch `fix/bug-24-jit-mockito-mic`). `MockProbe` runs JIT-on without
crashing; verify with the full Mockito behaviour + a JIT regression spot-check
(bench fib/sieve, bintrees18 checksum) before merge. Affected `dev` generally
(dev carries both `82b9bdf6` and the Mockito fix), not just this suite.
