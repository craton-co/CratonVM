# BouncyCastle crypto-regression — the "rc=1 crash" was cross-session taskkill; real issue = interpreter slowness + RSA/AES non-termination

> **UPDATE 2026-06-04 (final):** fully re-diagnosed. Two independent things were
> conflated:
> 1. **The `rc=1` "crash"/"timeout" is a MEASUREMENT ARTIFACT** — other Claude
>    sessions on this shared machine run `taskkill /F /IM cratonvm.exe` loops,
>    which kill *every* `cratonvm.exe` by image name (incl. unrelated test runs);
>    `taskkill /F` sets exit code 1. NOT a CratonVM crash. Proven below.
> 2. **CratonVM is genuinely slow on BC** (interpreter; `org.bouncycastle.*`
>    JIT-banned). Most tests PASS when run with a uniquely-named binary, but
>    **RSA and AES do not finish** (>150 s / >360 s vs HotSpot 4.3 s / 7.7 s).
>
> The SunEC "fix under test" is **inert** here (BC uses its own EC). The earlier
> "mid-run crash" section in this doc was itself the taskkill artifact — corrected
> below.

## Symptom
BC core `org.bouncycastle.crypto.test.RegressionTest`, heap 4g:
- CratonVM-CPU (before EC fix): **rc=124 TIMEOUT 360.4 s**.
- HotSpot: OK 119.8 s.
- TornadoVM: OK 130.4 s.

The `LEA: StringIndexOutOfBoundsException: Range [0,-1)` line appears identically
on HotSpot AND TornadoVM → that is a BC-vs-JDK25 artifact, NOT the CratonVM
failure. The CratonVM failure is the **timeout**: it does not finish the full
crypto regression battery within 360 s (HotSpot needs ~120 s).

Reproduce:
```
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 4g \
  -cp "<bc core main;test;resources>" org.bouncycastle.crypto.test.RegressionTest
```

## Root cause (pre-fix)
The crypto RegressionTest battery includes EC/ECDSA/ECDH cases plus many other
ciphers. With `org.bouncycastle.*` JIT-banned (JIT miscompiles BC) and EC
multiply slow, the EC-bearing tests dominate wall time and push the whole suite
past 360 s. Non-EC ciphers are fine (crypto-prng-regression passes in 43.8 s).

## Fix under test
Same EC fix set as math-ec (`cdab8f7`, `bcfbb5c`, `b19250f`). Expected to help
only the EC-bearing fraction of the battery.

## After-fix result (investigated 2026-06-04, dev @ c7efb0d, worktree)

**The bug was misdiagnosed. It is NOT a timeout, and the EC fix is INERT here.**

### 1. The "fix under test" cannot affect this suite
`cdab8f7` / `bcfbb5c` accelerate **SunEC** (`sun.security.ec.ECOperations`,
`MontgomeryIntegerPolynomialP256`). BC's `crypto.test.RegressionTest` uses
BouncyCastle's **own** `org.bouncycastle.math.ec` / `org.bouncycastle.crypto.ec`
(verified: `ECTest.java` imports `java.math.BigInteger` + `org.bouncycastle.*`,
**no** SunEC). So the SunEC EC fix is byte-for-byte irrelevant to this battery.
Remove this suite from the EC-fix's scope.

### 2. The `rc=1`/empty-output "crash" is cross-session `taskkill` (NOT a CratonVM crash)
This machine runs **multiple concurrent Claude sessions**. Several execute
`taskkill /F /IM cratonvm.exe` repeatedly — e.g. an `AllocOnly` benchmark loop in
`C:/craton/CratonVM` (`for i in 1..6; do taskkill //F //IM cratonvm.exe; ./...cratonvm.exe ...; done`)
and a build in `C:/craton/CratonVM-bcmath` (`taskkill /F /IM cratonvm.exe; cmd /c build-cpu.bat`).
`taskkill /IM` matches by **image name**, so it kills *every* `cratonvm.exe`
system-wide — including unrelated test runs in other worktrees — and `taskkill /F`
sets **exit code exactly 1**. This reproduces the entire symptom: `rc=1`, empty
output, nondeterministic timing, killing only *long* runs (short ones finish
between kills). (Memory `reference_bintrees_measurement_loop` already hinted:
"taskkill stray cratonvm first or rc=1-empty".)

**Proof it is an external kill, not a self-exit:**
- An inline hook on **both** `kernel32!ExitProcess` and `ntdll!NtTerminateProcess`
  (added to `vm-cli/src/main.rs` under `CRATONVM_CRASH_PROBE`) fires on a *normal*
  exit (validated: `MD5DigestTest` shows `ExitProcess(0)` + `NtTerminateProcess(0,0)`
  intercepted) but **never fires** for a "crashing" AES run — i.e. the process's
  own termination path is never taken → it was killed from outside (the killer's
  `TerminateProcess` runs `NtTerminateProcess` in the *killer's* address space).
- It is **not**: a Rust panic (panic hook + crash_handler both silent), any
  CratonVM `process::exit`/`abort` site (all `eprintln` first; none logged;
  `abort()` here yields `0xC0000409`, not 1), mimalloc corruption (verbose clean),
  OOM (peak ~4.4 GB = just the `-Xmx4g` heap), or a hardware fault (no VEH/`hs_err`).
- **Definitive:** copy the binary to a unique name (`cratonvm_bcx.exe`) so
  `taskkill /IM cratonvm.exe` cannot match it → the run is no longer killed at
  ~58 s; it runs to its own `timeout` instead. All numbers in §3 use this.

### 3. Clean results (uniquely-named binary, JIT on, no taskkill interference)
Most BC tests PASS — they are just slow (interpreter). Two genuinely do NOT finish:

| test        | HotSpot              | CratonVM (clean)        |
|-------------|----------------------|-------------------------|
| MD5         | —                    | 1.9 s `MD5: Okay` ✓     |
| DSA         | —                    | 7.4 s `DSA: Okay` ✓     |
| DH          | —                    | 23.4 s `DH: Okay` ✓     |
| ElGamal     | 0.5 s                | 31.6 s `ElGamal: Okay` ✓|
| ECIES       | —                    | 33.1 s `ECIES: Okay` ✓  |
| ECTest      | ~1 s                 | 38.3 s `EC: Okay` ✓     |
| Ed25519     | —                    | 47.3 s `Ed25519: Okay` ✓|
| **NISTECC** | —                    | 1.9 s `NISTECC: Exception` ← real test failure |
| **RSA**     | 4.3 s `RSA: Okay`    | **>150 s, no finish (timeout)** |
| **AES**     | 7.7 s `AES: Okay`    | **>360 s, no finish (timeout)** |

So the real CratonVM issues, once the taskkill artifact is removed, are:
- **RSA and AES are very slow but NOT hung** (>150 s / >360 s vs HotSpot ~4–8 s).
  Confirmed via `--stack-dump-on-timeout 90` (the watchdog dumps the Java stack):
  successive snapshots show *advancing* `pc` values → forward progress, no deadlock.
  - **RSA** is in key generation, specifically `RSATest.test_CVE_2017_15361`
    (ROCA, generates a fresh RSA keypair) →
    `RSAKeyPairGenerator.generateKeyPair → chooseRandomPrime → isProbablePrime →
    org.bouncycastle.math.Primes.implHasAnySmallFactors` (and
    `BigIntegers.modOddIsCoprimeVar → math.raw.Nat.fromBigInteger`). All
    `org.bouncycastle.*` (JIT-banned) → the small-factor prime pre-screen runs
    interpreted over many candidates ⇒ minutes.
  - **AES** is in `AESTest.testCounter → verify → org.bouncycastle.util.Strings.fromUTF8ByteArray`
    → `new String`/`StringUTF16.toBytes/compress`: heavy byte[]↔String churn plus
    interpreted AES-CTR.
  Net: the perf gap is the interpreter, not a logic bug. (Worth a follow-up: BC's
  prime pre-screen `Primes/Nat` and AES `Strings`/engine are the hot interpreted
  frames to either JIT or native-accelerate.)
- **NISTECC fails** with an `Exception` (completes, but the test is red). Compare
  against HotSpot to confirm it is a real CratonVM bug vs a BC-vs-JDK25 artifact.
- The general slowness is the **`org.bouncycastle` JIT ban** (`vm/src/jit/skip_list.rs:482`):
  all BC runs in the interpreter. **Not EC-specific** — and the SunEC EC fix (§1)
  cannot help.

## What an agent should try next
1. **Fix measurement first.** The `rc=1`/empty-output result is the shared-machine
   `taskkill /IM cratonvm.exe` artifact, not a CratonVM bug. Run the BC suites with
   a **uniquely-named binary** (or on an isolated machine), and stop killing
   cratonvm by image name across sessions (kill by PID, or namespace the exe).
   Without this, *every* long CratonVM run on this box is corrupted.
2. **RSA / AES non-finish** are slowness, not hangs (verified via
   `--stack-dump-on-timeout`, see §3). They are interpreter-bound; the fix is the
   same as the general slowness (item 3) — or native-accelerate the specific hot
   frames: BC prime pre-screen (`org.bouncycastle.math.Primes.implHasAnySmallFactors`,
   `math.raw.Nat.fromBigInteger`) for RSA keygen, and `org.bouncycastle.util.Strings`
   / AES engine for AES. Repro: `cratonvm_bcx.exe ... RSATest`.
3. **General BC slowness** = the `org.bouncycastle` JIT ban. Lifting it is the only
   way to close the 5–50× gap, but risks the documented EC value-production
   miscompile (`skip_list.rs:456-481`); `package_allowed` only lifts the *whole*
   `org/bouncycastle/` ban (prefix-of match), so a sub-package allow-list needs a
   code change.
4. **NISTECC `Exception`** — diff against HotSpot to classify.
5. **Drop the SunEC EC fix from this bug** — inert here (§1).

## Notes / artifacts
- Worktree build recipe (libffi won't build from source on this box): seed
  `build/libffi-sys-*`, `.fingerprint/libffi*`, `deps/*libffi*` from the main
  checkout's `target/release`, then build with vcvars64 **but `VCINSTALLDIR` +
  `VSCMD_ARG_TGT_ARCH` unset** (matches the seed's fingerprint so cargo reuses it).
- `CRATONVM_CRASH_PROBE` (temporary, in `vm-cli/src/main.rs`) installs the
  ExitProcess/NtTerminateProcess inline hooks + panic/exit markers used above.
- Real CratonVM I/O quirks seen while instrumenting (minor, but they hid output):
  `java.io.FileWriter`/`RandomAccessFile.write` don't persist to disk (only
  `FileOutputStream`+`getFD().sync()` does); `FileDescriptor.sync()` is very slow;
  `System.out`/`err` are buffered and lost on a non-clean exit.
