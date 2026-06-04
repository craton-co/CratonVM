# BC-JIT miscompile + deopt-flag follow-ups (branch `fix/bc-jit-miscompile`)

Continuation of the `org/bouncycastle/` JIT-ban throughput effort. The cached-JIT
deopt double-pop crash is already FIXED on `dev` (commit `ab50db9`). This branch holds
the next two items + the build infrastructure that makes them iterable.

## ★ Working build loop (the key unblocker)

This checkout cannot build `libffi-sys` from scratch (it needs a full MSVC+autotools env;
the MSYS `link.exe` also shadows MSVC's). Recipe that WORKS, isolated in this worktree:

1. **Seed** the already-built libffi from main's target (built by the concurrent
   `--workspace` build):
   `cp -rp <main>/target/release/build/libffi-sys-{0f3d1660ec01e00a,1da30f180c9c4015}  <wt>/target/release/build/`
   `cp -rp <main>/target/release/.fingerprint/libffi-sys-{...}  <wt>/target/release/.fingerprint/`
2. **Build via `build-wt.bat`** (in this worktree): runs `vcvars64.bat` (for LIB/INCLUDE/PATH
   so the MSVC linker is used) then **unsets `VCINSTALLDIR` + `VSCMD_ARG_TGT_ARCH`** — those
   are on libffi-sys's `rerun-if-env-changed` list, and the cached libffi was built with them
   unset, so unsetting them keeps the seeded `libffi.lib` valid and cargo skips the failing
   build script. `set PATH=%PATH%;C:\Program Files\Git\usr\bin;C:\ProgramData\chocolatey\bin`.
3. `cmd /c build-wt.bat` via the PowerShell tool. First full build ≈ 5–6 min; an
   `interpreter.rs` edit re-builds `cratonvm-vm`+`cli`+link ≈ 5–6 min.

Result: `target/release/cratonvm.exe` from clean-dev + this branch's edits. Validated working.

## A — wrong-result miscompile: OSR loop overrun in `Horst.horst_sign`  (ROOT-CAUSED)

With the ban lifted (`CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/`), `Sphincs256` throws a
real `ArrayIndexOutOfBoundsException`. Precise root cause:

- `Horst.horst_sign`'s `for (i = 0; i < HORST_T; i++)` loop (HORST_T = 1<<16 = 65536) calls
  `hs.hash_n_n(tree, (HORST_T-1+i)*HASH_BYTES, sk, i*HORST_SKBYTES)` (HORST_SKBYTES = 32,
  `sk.length = 65536*32 = 2097152`).
- The new `CRATONVM_DBG_AIOOBE2=1` gate (added in this branch, in interpreter.rs at the
  central `pending_runtime_error` throw) reports:
  `[AIOOBE2] index=2097152 class=…/HashFunctions method=hash_n_n([BI[BI)I pc=26`.
  `2097152 = 65536*32` = `i*HORST_SKBYTES` for **i = HORST_T = 65536** — i.e. the loop ran
  ONE iteration too many (valid max i is 65535).
- `CRATONVM_DBG_OSR=1` shows `horst_sign` is **OSR-compiled** (`[cratonvm-osr] enter …
  Horst.horst_sign … entry_pc=25 num_locals=18 locals=[…, i=1000, …]`) — OSR entered at
  i=1000 and the OSR-compiled loop overruns the `i < 65536` bound by one. It's the only
  OSR-compiled method in the run.

⇒ **FIXED — root cause was a 1-byte bytecode-length bug in `bytecode_len_at` (jit/src/x64.rs).**
`ldc` (opcode 0x12) was absent from the 2-byte arm and fell through to `_ => 1`, so the
PC-stepping consumers (the inline `branch_targets` precompute, the DCE walk, OSR/unroll
duplication) under-counted `ldc` by one byte. Disassembling `horst_sign`'s OSR code showed
the loop-exit `if_icmpge` emitted as `0f 8d 00000000` (rel32=0, a dead branch). Tracing the
precompute's visited PCs proved it stepped `…25, 27, 28, 30…` — at `ldc 65536` (pc 27) it
advanced by 1 to pc 28, then read the `ldc` operand byte (0x19) as an `aload` and jumped to
pc 30, **skipping the `if_icmpge` at pc 29**. So `branch_targets[60]` was never set ⇒ pc 60
(the loop exit) was DCE-marked dead (`pc_to_native[60] = -1`) ⇒ the forward branch to it was
left unpatched (rel32=0) ⇒ the bound check was dead ⇒ the loop ran to i=65536 and AIOOBE'd.
Fix = add `0x12` to the 2-byte arm (and the previously-also-missing `0x13`/`0x14` ldc_w/ldc2_w
to the 3-byte arm, and `0xa9` ret / `0xa8` jsr for completeness). VERIFIED: the AIOOBE is gone
(`CRATONVM_DBG_AIOOBE2` count 0) and the loop-heavy JIT benches `sieve250k`/`matrix600`/`fib44`
still match HotSpot checksums (no regression). NOTE: this only manifests when an `ldc` sits
immediately before a branch AND the operand byte's opcode-length realignment skips that branch
— rare, which is why most `for(i;i<CONST;…)` loops are unaffected.

REMAINING (separate, NOT this bug): with the overrun fixed, `Sphincs256` now runs the full
(very heavy) signing and still ends in a deeper failure (rc=1, output buffered/lost) around
~80s — a further BC-JIT issue and/or throughput limit, to be chased next with the build loop.
Repro (≈45s): `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/ CRATONVM_DBG_AIOOBE2=1 cratonvm …
org.bouncycastle.pqc.crypto.test.Sphincs256Test`.

## B — deopt-thrash / out-of-band deopt flag  (DESIGNED, not implemented)

The committed `ab50db9` fix restores popped args on the cached-JIT deopt path; correct, but
an `i64::MIN`-returning hot method (e.g. `Pack.bigEndianToLong`) deopts+re-executes on every
such return (the in-band `i64::MIN` sentinel collides with a real `long` value). Proper fix =
a thread-local deopt-pending flag set by `jit_uncommon_trap` (the npe/aioobe/exception deopts
already set flags) so `execute_jit_call`, on `result == i64::MIN` with NO flag set and a
`b'J'`/`b'D'` return type, pushes the value directly (no re-exec). **RISK:** must cover every
`i64::MIN`-returning deopt path (audit `jit/src/x64.rs` deopt emission — uncommon_trap stub,
npe/aioobe/exception helpers, nested-call propagation); a missed path → a genuine deopt read
as a value → silent corruption. Only observable once A is fixed (ban stays until then), so
sequence after A.
