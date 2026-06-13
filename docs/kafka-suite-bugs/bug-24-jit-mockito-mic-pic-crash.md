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

## Fix direction (candidate, untested)
In the non-publish branches of `jit_invoke_virtual_mic` (and the
`callee_compiler` gate), **invalidate** the MIC + PIC slot for that site
(`cached_class_id = 0` / `cached_entry_ptr = 0` and clear the matching PIC slot)
instead of merely not publishing — so the inline cascade can never call a stale
entry. Strictly safer (clearing a cache is always sound); needs a
`JitMICSlot::invalidate()` / `JitPICSlot::invalidate(class_id)` and a build+repro
cycle to confirm it kills the crash without regressing Tomcat bugs H/I
(`bench/BugH.java`, `bench/BugI.java`) or bintrees18.

## Status
Open. Tracked separately from bug-09 (the Mockito fix itself is correct — proven
under `--nojit`). Discovered while syncing `dev` into the kafka worktree for a
post-Mockito suite re-run; affects `dev` generally (dev carries both `82b9bdf6`
and the Mockito fix), not just this suite.
