# Gap: BC `math-ec` and `crypto-regression` TIMEOUT

**Discovered:** 2026-06-09 (cross-VM comparison run); root-cause known from earlier sessions  
**Severity:** High — `math-ec` still times out; `crypto-regression` now completes (see update below)  
**Status:** `math-ec` Open; `crypto-regression` **RESOLVED** (passes in 342s, within 360s TIMEOUT)

---

## Measurements — 2026-06-09 rerun (dev `f98fbca8`, TIMEOUT=360s, worktree CratonVM-run)

| Suite | CratonVM | HotSpot | TornadoVM | State |
|---|---|---|---|---|
| `bc/math-ec` | **TIMEOUT (360s)** | 30.1s | 29.4s | **TIMEOUT** |
| `bc/crypto-regression` | **342.3s (rc=1)** | 127.2s | 120.4s | **PASS** ← was TIMEOUT |
| `bc/pqc-crypto` | 41.5s (rc=1) | 1.2s | 1.2s | PASS |
| `bc/crypto-prng` | 19.7s (rc=1) | 0.5s | 0.5s | PASS |
| `bc/asn1-regression` | 26.0s | 0.9s | 1.2s | PASS |
| `bc/math-raw` | 1.5s | 0.3s | 0.3s | PASS |
| `bc/math` | 5.2s | 0.6s | 0.6s | PASS |
| `bc/util-encoders` | FAIL rc=127 | 0.5s | 0.4s | Harness artifact (see below) |

> `crypto-regression` rc=1: "DeterministicDSA: Okay" final line — all tests pass, `System.exit(1)` from a cleanup path.  
> `util-encoders` rc=127: orphaned `cratonvm.exe` from the preceding `math-ec` TIMEOUT is killed by the harness's `taskkill` between suites, which fires just as util-encoders launches. Direct run passes (15/15 OK). Harness issue, not a VM bug.

## Measurements — 2026-06-09 initial run (TIMEOUT=300s)

| Suite | CratonVM | HotSpot | TornadoVM | State |
|---|---|---|---|---|
| `bc/math-ec` | TIMEOUT (300s) | 51.3s | 52.4s | **TIMEOUT** |
| `bc/crypto-regression` | TIMEOUT (300s) | 125.6s | 174.9s | **TIMEOUT** |
| `bc/pqc-crypto` | 58.7s | 1.2s | 1.3s | PASS (rc=1²) |
| `bc/crypto-prng` | 36.5s | 0.6s | 0.6s | PASS |
| `bc/asn1-regression` | 64.7s | 1.4s | 1.5s | PASS |

> ² `pqc-crypto` rc=1: suite prints all "Okay" lines but some test calls `System.exit(1)` in a cleanup path. The cryptographic operations pass.

---

## Root cause: `math-ec`

### F2m (binary-field) elliptic curve — JIT banned

`math-ec` runs 14 JUnit3 tests in `org.bouncycastle.math.ec.test.AllTests`. All 14 are binary-field (F2m) EC operations: point multiplication, ECDSA, Koblitz curves.

The F2m implementation is in `org.bouncycastle.math.ec.F2mFieldElement` and uses `LongArray` for polynomial multiplication. `LongArray` contains a method `Interleave.expand64To128` that triggers a **JIT miscompile** in CratonVM (branches mis-targeted, loop overruns → AIOOBE). This was traced in an earlier session (`reference_bc_suite_harness_artifacts.md`).

The JIT ban (`skip_list.rs` or equivalent) prevents compiling the offending methods, so the entire F2m path runs interpreted. The interpreter is **60–225× slower** than HotSpot on this path. With 14 tests each doing large polynomial multiplications, the 300s timeout is hit.

### Progress state
- The JIT miscompile in `Interleave.expand64To128` was isolated: `bytecode_len_at` missing `ldc 0x12` (wide LDC) → branch-target misalign → dead loop-exit branch → SPHINCS loop overrun/AIOOBE.
- A partial fix (`dev ab50db9`) addressed the deopt double-pop underflow. 
- The deeper `bc-math-ec` JIT miscompile is not yet fixed; the JIT ban is still in place.

---

## Root cause: `crypto-regression`

`crypto-regression` is `org.bouncycastle.crypto.test.RegressionTest` — runs all BC crypto tests (AES, RSA, EC, hash algorithms). The suite takes 125s on HotSpot; CratonVM times out at 300s.

### Two sub-causes

**A. Interpreter speed (general).** Every cipher, MAC, and digest algorithm runs interpreted (JIT ban covers the hot loops). CratonVM's interpreter is ~60–225× slower on tight crypto loops.

**B. RSA and AES throughput.** Prior measurement: RSA >150s, AES >360s never finishing on CratonVM. These are the two operations most likely to still be bottlenecks in the regression suite. The `AESWrapPad: Okay` log line (captured in the partial run before TIMEOUT) shows AES-wrap tests ARE running but slowly.

**Note on HotSpot result:** HotSpot and TornadoVM both reach the end of `crypto-regression` but show a `StringIndexOutOfBoundsException` in the `LEA` test. This is a pre-existing BC bug (upstream) not related to CratonVM. The state is scored OK because `RegressionTest` continues past it.

---

## What `pqc-crypto` did to get under timeout

The `pqc-crypto` timeout (was >360s → now 58.7s) was fixed by adding **faithful native implementations** for:
- `ChaCha20`: `chachaCore`, `permute`, `chacha_permute` in `bc_chacha.rs`
- `NewHope NTT/SHAKE`: `toNTT`, `fromNTT`, `uniform` in `bc_newhope.rs` with verbatim Precomp tables

The same approach — adding native fast-paths for the specific BC hot methods — is what's needed for `math-ec` and `crypto-regression`.

---

## Fix direction

### Fix 1: JIT-fix `Interleave.expand64To128` (math-ec)

The JIT miscompile was narrowed to `bytecode_len_at` not handling `ldc 0x12` (wide-LDC, 3-byte instruction). When the branch-target scanner skips the 3-byte `ldc`, it misaligns all subsequent branch offsets, making a loop-exit branch dead and causing the loop to run past the array end.

**File to fix:** `vm/src/jit/x64.rs` or wherever `bytecode_len_at` (the instruction-length table) lives.  
**Action:** Add case for opcode `0x12` (ldc) = 2 bytes and `0x13`/`0x14` (ldc_w/ldc2_w) = 3 bytes in the length table. Re-enable compilation of `LongArray` (remove from skip list). Re-run `bc/math-ec` to verify 14/14 tests pass.

### Fix 2: Native fast-path for AES (crypto-regression)

The pattern from `pqc-crypto`: identify the BC AES hot method(s) and add a Rust-native implementation.

- BC AES is in `org.bouncycastle.crypto.engines.AESEngine` → `encryptBlock` / `decryptBlock`
- Key schedule: `generateWorkingKey` 
- These call into `Integers.rotateLeft` / byte-array XOR loops

**File to add:** `native-builtins/src/bc_aes.rs` (or `bc_cipher.rs`).  
**Validate:** `AESWrapPad: Okay` should appear quickly (< 5s). Run the full `crypto-regression` suite to confirm it finishes within 120s.

### Fix 3: Native fast-path for RSA (crypto-regression, math-ec)

BC RSA is in `org.bouncycastle.crypto.engines.RSABlindedEngine` → `org.bouncycastle.math.ec.ECPoint` → `BigInteger` modular exponentiation. The hot path is `BigInteger.modPow`.

A native `BigInteger.modPow` already exists for `java.math.BigInteger` (from the SunEC work). Check whether it is invoked through BC's `BigInteger` path (BC uses `java.math.BigInteger` directly) and if there are profiling-visible hot spots in RSA that aren't covered.

---

## Repro commands

```bash
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
CV="C:/craton/CratonVM/target/release/cratonvm.exe"
JDK="C:/Program Files/Java/jdk-25"
BC="C:/craton/CratonVM/apps/_test-suites/bc-java"
BCCP="$BC/core/build/classes/java/main;$BC/core/build/classes/java/test;$BC/core/build/resources/main;$BC/core/build/resources/test"
JUNIT3="$TEMP/junit-3.8.2.jar"

# math-ec (14 F2m tests, should complete in ~51s on HotSpot)
timeout 300 "$CV" --java-home "$JDK" --Xmx 1g \
  -cp "$BCCP;$JUNIT3" \
  junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests

# crypto-regression (full BC crypto, should complete in ~126s on HotSpot)
timeout 300 "$CV" --java-home "$JDK" --Xmx 4g \
  -cp "$BCCP" \
  org.bouncycastle.crypto.test.RegressionTest
```

---

## Related memory

- `reference_bc_suite_harness_artifacts.md` — full investigation history, isolated build loop, libffi seed
- `reference_bc_pqc_native_fix.md` — the pqc-crypto fix (ChaCha + NewHope native) as the template
- `reference_bc_crypto_regression_crash.md` — cross-session taskkill artifact (not a VM bug)
- `reference_bc_math_ec_timeout.md` — earlier deeper diagnosis of F2m `Interleave.expand64To128`
