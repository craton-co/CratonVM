# crypto-prng HMacDRBG #9.1 KAT failure — root cause + FIX (2026-06-01)

> **RESOLVED.** Fixed by making the long `getfield`/`putfield`,
> `getstatic`/`putstatic`, every `invoke*` argument/return marshalling path,
> and the JIT-call arg convention **kinds-aware** (read `KIND_LONG` slots
> bit-exact via `as_long_unchecked`) instead of decoding via the lossy
> `tag()` / `decode_by_descriptor(b'J')` i2l-widening heuristic. The minimal
> repro below now matches HotSpot; `crypto-prng RegressionTest` reports
> "HMacDRBG: Okay / All tests successful"; commons-math 3204/3204 and the BC
> green suites stay green. See "Fix (applied)" at the bottom.


`bc-crypto-prng-regression` reports exactly one failure on CratonVM (HotSpot +
TornadoVM pass): `HMacDRBG: Test #9.1 failed`. Traced it to the bottom.

## The chain (each step verified)
1. **DRBG vector #9** = HMAC-SHA384, security-strength 192, prediction-resistance
   **false**, with an 111-byte personalization string. (#10 is identical but
   PR=true, which reseeds before generating and so **masks** a bad instantiate
   state — that's why only #9, not #10, fails.)
2. The wrong output comes from a wrong **instantiate** state: `_K`/`_V` after
   `HMacSP800DRBG` construction already differ from HotSpot.
3. Step-by-step replay of `hmac_DRBG_Update`: the first HMAC (`K1`) matches;
   **`V1 = HMAC(K1, V0)` diverges** (V0 = 48×0x01).
4. The divergence is **not** HMac/Memoable: a *manual* HMAC built from raw
   `SHA384Digest` calls diverges identically.
5. HMAC with key=`0xFF×48` is **correct**; HMAC with key=`K1` is wrong for any
   message → it's the SHA-384 hashing of the ipad-key block, not HMAC logic.
6. **Minimal repro — pure SHA-384, no HMAC/DRBG:**
   ```
   SHA-384( hex
     "43a9f4d4e500b0c9820c82cf119fd17b4a06f8bf33f398e94cb2c94071adee80"
     "ef0694515b40ef8a84c216957d40537c"  + "36"*80 )   // 128 bytes
   HotSpot : 28d6fb83a3320463841f9a1c0c4f592cebe257971a755704057c53fe113e8154...
   CratonVM: 3a9d1b13b9a4a4fea0e33ea28d8de610a6c4ad297c21ab93e65bf017a105f6df...  (WRONG)
   ```

## Root cause: data-dependent 64-bit-long bug in the SHA-512 core (interpreter)
Tightly bounded by these facts:
- **Not native** — there is no native SHA-384/512; this is BouncyCastle's pure-Java
  `LongDigest`/`SHA384Digest`.
- **Not JIT** — `--nojit` reproduces it identically.
- **Data-dependent** — `SHA-384` of `mk(128)` (a different 128-byte block) is
  CORRECT; only this specific block is wrong. Inputs ≤127 bytes are correct;
  the failure starts at exactly one full block.
- **IV-dependent, NOT the compression function** — `SHA-512` of the SAME bytes
  is CORRECT. SHA-384 and SHA-512 share the identical 64-bit compression
  function and differ ONLY in the initial H values + output truncation. So the
  compression code is fine; with the SHA-384 IV, this input drives a working
  variable to a 64-bit value that the interpreter mishandles.

This is the **category-2 long-bit-collision family** (`types/src/compact_value.rs`):
a `long` whose 47-bit payload looks like a tagged `int` gets truncated on some
operand-stack / local / field / array path. Commit `69d1401` fixed several such
paths (lload/lstore/returns/invoke-arg-pops/OSR/putstatic); SHA-512's long ops
(H1–H8 long *fields*, the long working schedule, `Long.rotateRight`/`>>>`/`+`)
evidently hit a path NOT yet covered. The SHA-384 IV + this block produce a
colliding long value; the SHA-512 IV (same block) and `mk(128)` (same length)
do not.

## Why it's narrow / why other crypto passes
Only manifests when a SHA-384/512 working variable transiently equals a
collision-shaped long. Most inputs never hit it (the whole BC asn1/util/math
crypto suite, HashDRBG, HMAC-SHA1/256, SHA-512 DRBG vectors all pass). DRBG
vector #9's specific personalization → K1 → ipad block is one that does.

## Fix (applied)
Bisected with `LongBug.java` (round-trips `0xFFFC0000FACE1234L` — top 14 bits
collide with the NaN-box + `SUB_INT` sub-tag, **and** payload bits 47-32 are 0
so it is bit-identical to `CompactValue::int(0xFACE1234)`). Result: `local`,
`long[]`, `lxor/ladd/lor/land`, `lshift` round-trip fine, but **`field`,
`call`, and `Long.rotateRight` corrupt** `0xFFFC0000FACE1234` →
`0xFFFFFFFFFACE1234` (decoded as a tagged int, truncated to the low 32 bits,
sign-extended).

Root cause: those paths decoded the slot with `cv.tag()` /
`decode_by_descriptor(b'J')`, whose `SUB_INT`-with-`payload < 2^32` arm applies
JVMS i2l widening — correct for a genuine int that forgot its `i2l`, but it
truncates a collision-shaped long. The ambiguity is unresolvable from the 8
bytes alone (see `docs/bc-ec-mod-mododdinverse-investigation.md`); the resolver
is the operand stack's parallel `kinds[]` array (`KIND_LONG`), which
`pop_long`/`pop_double` already consult. These paths simply weren't consulting
it.

Made them kinds-aware (mirrors commit `69d1401`'s `pop_long` fix), so a slot a
genuine long producer pushed (`lload`/`ladd`/long return/long getfield) is read
bit-exact, while the `KIND_UNKNOWN` fallback keeps the i2l-widening crutch for
synthetic int-where-long:
- **Putfield-J / Putstatic-J** → pop via `ValueStack::pop_long`.
- **Getfield-J / Getstatic-J** → push via `push_compact_long(_checked)`
  (mark `KIND_LONG`; D sibling marks `KIND_DOUBLE`).
- **`push_invoke_return_value`** → `push_compact_long` / `push_compact_double`.
- **All `invoke*` arg marshalling** (slow virtual/static, cached fast paths,
  intrinsic, JIT-call ABI) → pop with the new
  `ValueStack::pop_compact_with_long_mark[_unchecked]` /
  `pop_arg_for_descriptor_checked` and decode through `decode_arg_kind_aware`.
- **Fast-path `lreturn`** → kinds-aware decode.

No `CompactValue::decode_by_descriptor` change (its bit-only heuristic stays as
the documented fallback). No allow-list entries, no stubs — pure
value-representation correctness.

## Separately noticed (real, unrelated, easy)
`String.format("%02x", aByte)` sign-extends on CratonVM ("ffffffb0" vs HotSpot
"b0") — a `java.util.Formatter` `%x`-on-`Byte`/`Short` bug. Harmless to the DRBG
(which compares bytes), but a genuine correctness gap for any code formatting
bytes; worth a separate fix.
