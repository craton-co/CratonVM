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

REMAINING (separate, NOT this bug): with the overrun fixed, `Sphincs256` runs the full (very
heavy) signing and still does not complete. **RESOLVED — see section C: this is a throughput
wall, not a further miscompile.** `chachaCore` (and the hash primitives) are byte-correct; the
JIT is just far too slow on BC crypto, so SPHINCS-256 signing cannot finish in a practical
timeout and the `org/bouncycastle/` ban stays.
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

### B — UPDATED audit (branch `fix/bc-jit-sphincs`): the original design is UNSAFE as written

The flag set covers more than the handoff implies, and one path the design overlooked makes
the naive "push the value at the top level when no flag is set" rule **corrupting**:

- `jit_uncommon_trap` (`vm/src/jit/helpers.rs:3085`) sets **no** flag today — it records the
  deopt and returns an action code the stub ignores. So the generic deopt stub
  (`emit_deopt_stubs`, `x64.rs:10302`, incl. div-by-zero reason 3) returns `i64::MIN` with no
  flag. ⇒ B step 1 (set a `JIT_PENDING_DEOPT` in `jit_uncommon_trap`) is correct and necessary.
- NPE / AIOOBE / exception stubs DO set their flags and are drained ABOVE / INSIDE the `i64::MIN`
  arm in `execute_jit_call`, so those never reach the "no flag" branch.
- **The overlooked path:** `emit_post_invoke_exception_check` (`x64.rs:10255`) emits an inline
  `CMP RAX, i64::MIN; JE` after EVERY nested dispatch call, and the shared
  `emit_exception_check_stub` (`x64.rs:10272`) re-loads `i64::MIN` and returns it up the call
  chain **without setting any flag** — it relies entirely on the top-level "`i64::MIN` ⇒ deopt
  ⇒ re-execute" contract. When a *nested* callee legitimately returns `Long.MIN_VALUE` (or
  `double -0.0`, same bit pattern `0x8000_0000_0000_0000`), this guard fires as a FALSE POSITIVE
  and the caller bails early returning `i64::MIN`. Under the current code that surfaces at the
  top level as a deopt → the caller is re-executed in the interpreter → **correct result**
  (just slow + double side effects). Under the naive B rule, the top level would instead PUSH
  `i64::MIN` as the caller's return value WITHOUT running the caller's post-call bytecode →
  **silently wrong result**. This is strictly worse than the bug B set out to fix.

⇒ A safe B must ALSO make `emit_exception_check_stub` set `JIT_PENDING_DEOPT` (cheap: it is a
single shared out-of-line stub, so add a `CALL set_jit_pending_deopt` before the `MOV RAX,
i64::MIN`; the exception case already sets `JIT_PENDING_EXCEPTION`, which the top level checks
first, so over-setting deopt there is harmless). Only then is "`i64::MIN` + no flag + `b'J'`/`b'D'`
return ⇒ push the value" sound. Note the genuine-value producers that legitimately yield
`i64::MIN` and must keep flowing through as values: `d2l`/`f2l` overflow (`x64.rs:5436` /
`9993`, which push `0x8000_0000_0000_0000` as the saturated long) and any method returning
`Long.MIN_VALUE` / `-0.0`.

**Decision (this branch): B left unimplemented.** Its original BC motivation (`Pack.bigEndianToLong`)
is moot because the ban stays (see C). Its general value — a JIT leaf returning `Long.MIN_VALUE`
or `double -0.0` with side effects double-executing — is real but rare, and the blast radius of a
missed path is the whole JIT. Not worth shipping without the full audit + targeted tests
(leaf returning `Long.MIN_VALUE`/`-0.0` with a side effect must NOT double-execute; nested call
returning `Long.MIN_VALUE` must still produce the right caller result; div-by-zero / BCE deopts
must still re-execute).

## C — "A-remaining deeper failure" ROOT-CAUSED: it is a THROUGHPUT wall, not a miscompile

Reproduced on `fix/bc-jit-sphincs` (binary built via `build-wt.bat`). Hard data:

- **HotSpot runs the whole `Sphincs256Test` in ~1s** (`Sphincs256: Okay`).
- **CratonVM cannot finish even one subtest** of `performTest()` in 400s, JIT-on or JIT-off.
  The 40s watchdog (`--stack-dump-on-timeout 40`) catches it grinding in
  `ChaChaEngine.chachaCore` (via `Seed.prg → Salsa20Engine.processBytes →
  ChaChaEngine.generateKeyStream`) inside `Horst.horst_sign` — i.e. the 2^16-iteration HORST
  signing hashing loop. The earlier "rc=1 ~80s" was just the buffered-stdout loss on a slow
  run; both interpreter (`rc=1` at <600s) and JIT (`rc=124` at 400s) fail to complete.
- **`chachaCore` is byte-correct.** `ChaProbe` (calls `ChaChaEngine.chachaCore` directly,
  `public static`) gives the SAME checksum as HotSpot under both interpreter and JIT — there is
  NO miscompile here. It is purely slow: at N=100k, HotSpot 40ms vs CratonVM interpreter
  ~35000ms (~875×) and JIT ~18000ms (~450×). **The JIT is only ~2× faster than the interpreter**
  on this code, where it should be near-native for a tight int loop.
- **Why the JIT barely helps:** `chachaCore` makes ~64 `org.bouncycastle.util.Integers.rotateLeft`
  calls per invocation. That wrapper just delegates to `Integer.rotateLeft`, but
  `resolve_inline_site` (`interpreter.rs:14107`) rejects ANY callee whose body contains an invoke,
  so the wrapper is **never inlined** — each rotate is a full JIT-dispatch call. The per-call
  dispatch overhead, ×64 ×(millions of hash blocks), is the wall.

⇒ **The `org/bouncycastle/` JIT ban stays.** It was never going to be lifted by fixing one
miscompile; SPHINCS-256 signing needs near-HotSpot throughput across all of BC's crypto
(ChaCha + BLAKE + SHA-512), which CratonVM's JIT does not deliver. There is no further
"BC-JIT miscompile" to chase for SPHINCS — A (the AIOOBE loop overrun) was the only real
miscompile and it is fixed on `dev`.

### Throughput levers identified (future work, NOT required for correctness)

1. **`Integer`/`Long.rotateLeft`/`rotateRight` JIT intrinsic — DONE on this branch.** Added
   `IntRotateLeft`/`IntRotateRight` (`ROL`/`ROR r32, CL` + `MOVSXD`) and
   `LongRotateLeft`/`LongRotateRight` (`ROL`/`ROR r64, CL`) to the INT_BITS / LONG_BITS intrinsic
   regions (matcher in `jit/src/lib.rs`, codegen in `jit/src/x64.rs`). x86's `CL & 0x1f` (32-bit)
   / `CL & 0x3f` (64-bit) masking is byte-identical to the JDK rotate-mod-width definition, so no
   distance masking is needed. VERIFIED byte-identical to HotSpot via `RotProbe` (all edge
   distances 0/32/33/64/65/negative, plus `MIN_VALUE`/high-bit values); Sieve/Matrix/IntrinsicBench
   checksums unchanged (no regression). **Caveat:** this helps code that calls
   `Integer/Long.rotate*` DIRECTLY (e.g. JDK `sun.security.provider.SHA2` uses
   `Integer.rotateRight`). It does NOT speed up BC's `chachaCore` (`ChaProbe` timing unchanged),
   because BC funnels through the non-inlinable `Integers` wrapper — the rotate intrinsic only
   optimises the wrapper's BODY, not the per-call dispatch overhead that dominates.
2. **The real BC lever: inline single-invoke delegating wrappers when the inner call is an
   intrinsic.** Relaxing `resolve_inline_site` to permit a callee whose only invoke resolves to a
   recognised intrinsic would let `Integers.rotateLeft` inline into `chachaCore` as a bare `ROL`.
   This is the change that would actually move BC crypto throughput — but it is risky (deopt
   frame reconstruction across an inlined-call-turned-intrinsic) and only worth it if lifting the
   `org/bouncycastle/` ban becomes a goal. Not attempted here.

### Repro tooling added (this branch)

- `build-wt.bat` — the isolated worktree build (vcvars64 + unset `VCINSTALLDIR`/`VSCMD_ARG_TGT_ARCH`).
- `repro-sphincs.sh` — runs `Sphincs256Test` ban-lifted with full output capture.
- `ChaProbe.java` — direct `chachaCore` correctness + throughput probe (byte-identical check).
- `RotProbe.java` — rotate-intrinsic byte-identical verification (edge distances + negatives).
- `SphinxDriver.java` — drives `performTest()` with explicit flush + `Throwable` capture (defeats
  the buffered-stdout loss that hid the failure as "rc=1, output lost").
