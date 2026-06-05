# BouncyCastle pqc-crypto-regression — timeout (>360 s) — ✅ RESOLVED

## Symptom (original)
BC core `org.bouncycastle.pqc.crypto.test.RegressionTest` (`Sphincs256Test` +
`NewHopeTest`), heap 2g:
- CratonVM-CPU: **rc=124 TIMEOUT 360.2 s**.
- HotSpot: OK 1.3 s. TornadoVM: OK 1.5 s.

## Status: FIXED (2026-06-04, branch `dev`)
```
Sphincs256: Okay
NewHope: Okay
All tests successful.        EXIT=0   123.8 s   (was rc=124 TIMEOUT 360.2 s)
```
Both PQC schemes now pass, **with the `org/bouncycastle/` JIT ban still in
place** — the fix sidesteps the ban entirely via faithful native intrinsics
rather than lifting it (the ban must stay for the unrelated F2m-EC value-model
collision; see `reference_bc_math_ec_timeout`).

## Real root cause (the pre-fix "SPHINCS miscompile" handoff was WRONG)
This was **never** a JIT miscompile. It is pure **interpreter throughput** under
the JIT ban. A fresh `--stack-dump-on-timeout` dump pinned it precisely — the
PQC schemes are dominated by a handful of hot kernels that run interpreted:

**SPHINCS-256** (`Sphincs256Test`):
- PRG: `Seed.prg → Salsa20Engine.processBytes → ChaChaEngine.generateKeyStream
  → ChaChaEngine.chachaCore → Integers.rotateLeft`. The ChaCha core's ~dozens of
  per-block `Integers.rotateLeft` *method calls* crush the interpreter (each is a
  full frame setup, not a `ROL`).
- Hash: `Tree/Wots/Horst → HashFunctions.hash_n_n/hash_2n_n →
  Permute.chacha_permute → Permute.permute` (a second, distinct ChaCha
  permutation — no input-add, used as the hash), plus per-hash `byte[64]`
  allocation + 32 `Pack` conversions, called millions of times in WOTS/treehash.

**NewHope** (`NewHopeTest`, 1000 key-exchange rounds):
- `Poly.toNTT`/`fromNTT → NTT.core` — the number-theoretic transform, pure
  `short[]` Montgomery butterflies (the profiled dominant frame).
- `Poly.uniform` — SHAKE128 (interpreted Keccak) rejection sampling for the `a`
  polynomial.
- `Poly.getNoise` — ChaCha20 PRG (heavy part = `chachaCore`, now native) +
  binomial sampling.

## The fix — faithful, validated native intrinsics (NOT stubs)
Mirrors the existing BC AES/RSA native fast-paths. All `Intrinsic`-tagged,
registered in the always-on essentials path (`native-builtins/src/lib.rs`).

- `native-builtins/src/bc_chacha.rs` — verbatim ports of `ChaChaEngine.chachaCore`
  (PRG; used by SPHINCS **and** NewHope), `Permute.permute` + `Permute.chacha_permute`
  (SPHINCS hash), and the SPHINCS `HashFunctions.hash_n_n`/`hash_2n_n`(+`_mask`)
  byte glue. Validated by the RFC 8439 ChaCha20 block KAT + HotSpot ground-truth
  vectors. Registered as `register_bc_chacha` (chachaCore/permute via invokestatic;
  chacha_permute + hash_* via invokevirtual — both dispatch through the native
  override check).
- `native-builtins/src/bc_newhope.rs` + `bc_newhope_tables.rs` — verbatim port of
  `NTT.core`/`bitReverse`/`mulCoefficients` + `Reduce.montgomery`/`barrett` (exact
  i32-wrapping arithmetic) with the `Precomp` tables extracted verbatim from the
  `.java`, plus `Poly.uniform` via the workspace's `sha3` crate (standard SHAKE128
  == BC's `SHAKEDigest(128)`). `Poly.toNTT`/`fromNTT`/`uniform` registered as
  `register_bc_newhope` (invokestatic). **Validated element-for-element against
  HotSpot** (`NHNttProbe` dumps) — all 1024 coefficients of toNTT, fromNTT, and
  uniform match exactly.

`Poly.getNoise` is intentionally left interpreted — native `chachaCore` already
covers its heavy frame; its residual XOR/sampling glue is minor.

## Why this is the right approach (vs lifting the JIT ban)
The ban can't be lifted: BC's F2m EC path hits a `CompactValue` long↔object
NaN-box collision (a `long`-only bug). NewHope's NTT is `short`/`int`-only and
SPHINCS is byte/int, so they *would* be safe to JIT — but Keccak (`long[]`) is
not, and the native route is faithful, validated, and ban-independent. It also
matches the "no synthetic stubs — real bytecode or a faithful intrinsic"
project rule.

## Remaining headroom (optional)
123.8 s passes comfortably (3× the 360 s wall) but is far from HotSpot's 1.3 s —
the residual is interpreted protocol glue (`getNoise` XOR/sampling,
`pointWise`/`add`, the 1000-round driver). Native `Poly.getNoise` (ChaCha20
state setup is standard — see `ChaChaEngine.setKey`) would roughly halve NewHope
if more margin is ever wanted. Not required to pass.
