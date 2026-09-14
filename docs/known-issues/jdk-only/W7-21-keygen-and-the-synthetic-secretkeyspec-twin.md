# `KeyGenerator` ignored its algorithm, and the "shadowing" twin was shadowed

> **RUN AND VERIFIED 2026-08-12 (lane A32, record triage). PATCHES A, C AND D
> ALL HOLD UNDER EXECUTION, AND THE `StackWalker$Option` RESIDUAL IS CLOSED —
> THE ONE ITEM THIS RECORD HANDED OUT OF THE LANE IS NO LONGER TRUE.**
>
> Every pass above this one was source reading or a measurement on a *pre-fix*
> binary; the fourth pass's own verification was a reflective work-around, not a
> run of the fix. Measured here on `cratonvm-merged-dev.exe` against
> `jdk-25.0.3.9-hotspot`, `--jdk-only` and `--real-jdk`, HotSpot 25 as the
> same-session oracle, fixed inputs, hex by hand.
>
> **`StackWalker$Option` — CLOSED. Re-check before citing it.** This record
> states, as an out-of-lane open item, that "**Every** `java/lang/StackWalker$Option`
> constant reads back null in this VM" and that `DROP_METHOD_INFO` has no
> registration at all. Measured under `--jdk-only`:
>
> ```text
> ROW StackWalker RETAIN_CLASS_REFERENCE | v=RETAIN_CLASS_REFERENCE
> ROW StackWalker DROP_METHOD_INFO       | v=DROP_METHOD_INFO
> ROW StackWalker getInstance(Set)       | walker=true
> ```
>
> All three identical to HotSpot. The `Set.of(DROP_METHOD_INFO,
> RETAIN_CLASS_REFERENCE)` construction that killed
> `JceSecurityManager.<clinit>` — the exact call at its line 71 — now succeeds.
> That is the wall the fourth pass could only route *around*; it is gone, so the
> `getMaxAllowedKeyLength` natives are no longer the only thing standing between
> `KeyGenerator` and the real path.
>
> **The chokepoint answers HotSpot's values, from the public doors:**
> `Cipher.getMaxAllowedKeyLength("AES") = 2147483647` and
> `getMaxAllowedParameterSpec("AES") = null`, both arms, both identical to
> HotSpot — the `crypto.policy=unlimited` answers the applied fix specifies.
>
> **Patch A — verified by run, not by registry dump.** The deletion did not
> strand the 2-arg form:
> `KeyGenerator.getInstance("AES","SunJCE").generateKey().getEncoded()` is
> `len=32`, **not all zeros**, in both arms. Result 4's four
> `NullPointerException: … "this.spi" is null` rows are gone. The 1-arg family
> matches HotSpot's lengths across `AES` 32, `HmacSHA256` 32, `DESede` 24, `DES`
> 8, `Blowfish` 16, `ChaCha20` 32, and `generateKey().getAlgorithm()` answers
> `DESede` for `DESede` — the carrier no longer hardcodes `"AES"` on the path a
> caller can reach.
>
> **Patch C — verified, both halves, and the class distinction holds:**
>
> ```text
>                                      HotSpot 25 / --jdk-only / --real-jdk  (identical)
> new SecretKeySpec(new byte[0],"AES")  IllegalArgumentException: Empty key
> new SecretKeySpec(null,"AES")         IllegalArgumentException: Missing argument
> ```
>
> The null case raises `IllegalArgumentException`, **not** `NullPointerException`
> — which is the point the third pass makes about ordering the null test before
> `obj_arg`, now confirmed from outside.
>
> **The key-material copy boundary holds under a caller that scrubs**, which is
> the mechanism that produced the all-zero key. Construct from an array, scrub
> the array, re-read: unchanged. Mutate a `getEncoded()` result, re-read:
> unchanged. Same for `IvParameterSpec.getIV()`. All byte-identical to HotSpot.
>
> **Patch D — verified.** `String.format("%02x")` over
> `{00,01,0e,0f,10,ff}` gives `[00010e0f10ff]` in both arms, matching HotSpot.
> The `" e"`/`" f"` corruption is gone, so a hex-dumping probe is trustworthy
> again.
>
> **Scheduled evidence:** `PASS RCrypto (57 checks)` in both arms — the vector
> is in `run.sh`'s `CORE_CLASSES`, and its `maxKeyLen=2147483647` line is the
> `getMaxAllowedKeyLength` fix under permanent guard. `KeyMaterialCensus` and
> `KeyGenProbe2`, the two probes this record was measured with, are **not in the
> tree**, and `probes/` is not run by `run.sh` at any `SUITE=` value.
>
> **Still open, re-confirmed as open:** `java/security/Key.getAlgorithm`
> hardcoding `"AES"` (synthetic-jdk only, blocked on a missing `NativeContext`
> slot count — a missing capability, not a deferral), and
> `init(AlgorithmParameterSpec)` accepting and ignoring. Neither is reachable by
> the probes above in a shipping binary.

> **PATCH A IS VINDICATED, AND IT UNCOVERED A SECOND WALL ONE FRAME FURTHER IN.
> 2026-08-12 (fourth pass, JCA lane). `RCrypto` went RED after Patch A; the
> deletion was not the defect.**
>
> **Symptom.** `--jdk-only RCrypto` died with `ExceptionInInitializerError` at
> `RCrypto.java:545`, the new `KeyGenerator.getInstance("AES","SunJCE")` line.
>
> **The actual exception**, which the truncated trace hid, is
> `NullPointerException: Cannot invoke "Object.equals(Object)" because "e0" is
> null`, thrown in `java/util/ImmutableCollections$Set12.<init>` from
> `Set.of(…)` from `javax/crypto/JceSecurityManager.<clinit>` line 71:
>
>     WALKER = StackWalker.getInstance(
>         Set.of(Option.DROP_METHOD_INFO, Option.RETAIN_CLASS_REFERENCE));
>
> `e0` is `StackWalker$Option.DROP_METHOD_INFO`. **Every**
> `java/lang/StackWalker$Option` constant reads back null in this VM. The chain
> is all real JDK bytecode we do not intercept:
> `KeyGenerator.getInstance` → `JceSecurity.getInstance` →
> `GetInstance.getInstance` → `AESKeyGenerator.<init>` →
> `SecurityProviderConstants.getDefAESKeySize` →
> `Cipher.getMaxAllowedKeyLength("AES")` → `Cipher.getConfiguredPermission` →
> `getstatic JceSecurityManager.INSTANCE` → that clinit.
>
> **Not a regression, and not the deletion.** Measured on the pristine-dev
> `44044c7e2` control binary: `StackWalker$Option.RETAIN_CLASS_REFERENCE` is
> null there too, and `Cipher.getMaxAllowedKeyLength("AES")` raises the same
> `ExceptionInInitializerError`. The wall predates this wave. What Patch A
> changed is only that the real path is now WALKED far enough to reach it — the
> deleted shims used to return before the first real frame. The A/B is not
> "green→red on the same input": the control binary fails `RCrypto` too, one
> line later (`NullPointerException: … "this.spi" is null` at
> `KeyGenerator.generateKey`), which is precisely the defect Patch A cut out.
>
> **The existing `JceSecurityManager.getCryptoPermission` native cannot help.**
> It is an INSTANCE method reached through `getstatic INSTANCE`, and the
> getstatic is what runs the clinit. Its doc comment describes a later wall
> (`defaultPolicy` null ⇒ NPE in `getPermissionCollection`) that the class never
> survives long enough to reach on JDK 25. It is dead code today and stays only
> because it becomes live the moment `StackWalker$Option` is fixed.
>
> **Fix (applied).** Natives for `javax/crypto/Cipher.getMaxAllowedKeyLength`
> and its twin `getMaxAllowedParameterSpec`, next to `getCryptoPermission` in
> `register_cipher_clinit_shim` — the two PUBLIC doors into the chokepoint —
> answering `Integer.MAX_VALUE` and `null`. Those are measured on HotSpot 25 on
> this host, not assumed: `getMaxAllowedKeyLength("AES"|"DES"|"RC4") =
> 2147483647`, `getMaxAllowedParameterSpec("AES") = null`, which is what the
> `crypto.policy=unlimited` shipped by default since Java 9 gives. Both refuse
> exactly what the real method refuses and no more: an unmessaged
> `NullPointerException` for a null transformation (the explicit check in
> `getConfiguredPermission`, which runs BEFORE tokenizing, so it is NOT
> `NoSuchAlgorithmException: No transformation given`), and
> `tokenizeTransformation`'s own messages for a malformed one, via this file's
> existing `tokenize_transformation` port. Neither method validates that the
> ALGORITHM exists — measured, `getMaxAllowedKeyLength("Bogus")` is also
> `2147483647` — so the natives do not either.
>
> **Verification without a rebuild** (this lane could not build). The rest of
> the real path was proved sound on the wave binary by pre-seeding
> `sun.security.util.SecurityProviderConstants.DEF_AES_KEY_SIZE` to 256
> reflectively, which is exactly what a working `getMaxAllowedKeyLength` causes
> `getDefAESKeySize` to store, and which makes it return before calling into
> `Cipher`. With only that one call removed, `--jdk-only` gives
> `kg2Key.length = 32`, not all zeros, and `new SecretKeySpec(new byte[0],
> "AES")` ⇒ `IllegalArgumentException` — the three `keygen2arg` checks, exactly
> as HotSpot. Independently, all 16 `CK` lines `RCrypto` emits before that block
> already diff clean against a same-session HotSpot run.
>
> **Out of this lane, still open, NOT fixed here.**
> `java/lang/StackWalker$Option`'s constants are null under `--jdk-only`.
> `native-builtins/src/phases_late.rs:5222-5255` registers static-field natives
> for three of them (`RETAIN_CLASS_REFERENCE`, `SHOW_HIDDEN_FRAMES`,
> `SHOW_REFLECT_FRAMES`) — `DROP_METHOD_INFO`, added in JDK 22, has none — and
> none of the three is consulted for a `getstatic` when the real class bytes are
> authoritative. Anything else that reaches `JceSecurityManager` or a real
> `StackWalker.getInstance(Set)` hits this.
>
> **Refuted, both named as suspects and both innocent.** Patch C's
> `SecretKeySpec` guard reproduces the real `<init>`'s two checks in the real
> order and measures `IllegalArgumentException` for the empty key, matching
> HotSpot. `jca/message_digest.rs`'s `canonical_algorithm` fold matches only
> `SHAKE128`/`SHAKE256` and returns its argument unchanged otherwise; `RCrypto`'s
> `sha256` and `hmacSha256` lines are byte-identical to HotSpot's.

> **PATCHES A AND C ARE APPLIED, 2026-08-12 (third pass, JCA lane). Read this
> before the two reconciliation blocks below, both of which now overstate what
> is open.**
>
> * **Patch A — APPLIED as option 1, DELETION.** Both 2-arg
>   `KeyGenerator.getInstance` registrations, the enclosing
>   `register_keygen_dispatch`, and its call site are gone from
>   `native-builtins/src/jca/cipher.rs`; a tombstone stands where the function
>   was. `keygen_default_bits` was **not** made `pub(crate)` and is not needed —
>   option 2 would have wired a synthetic default into a path that now has a
>   real one. Verified before cutting: the shims' matching `init` /
>   `generateKey` natives live in `phases_early::register_phase53_crypto`, which
>   is reachable **only** from `lib::register_synthetic_overrides`, so in both
>   shipping modes the synthetic they returned had no working `init` at all —
>   and under real-JDK `try_alloc_concurrent_synthetic` upsizes it to the real
>   layout, where slot 0 is `spi`, so the algorithm String was written into the
>   SPI field. The real path they bypassed is live: thirteen (not twelve)
>   `SunJCE` `KeyGenerator` services under real JDK class names, and the
>   `sun/security/jca/GetInstance` named-provider **and** search bridges, both
>   gated on `ec_real`, whose `route_ec_to_real()` disjunct is default ON.
> * **Patch C — APPLIED as written**, `SecretKeySpec.<init>([BLjava/lang/String;)V`.
>   The null test runs **before** `obj_arg`, which is the whole point: `obj_arg`
>   raises `NullPointerException` and a caller's `catch
>   (IllegalArgumentException)` does not catch it, so ordering the checks the
>   other way would have kept the defect while looking fixed.
> * **Both are Compatible-mode behaviour changes**, deliberate and of the shape
>   §5 of W7-63 catalogues: from "returns an object that fails later on a
>   different line" and "accepts a zero-length key" to HotSpot's own refusals.
> * **Coverage.** `regression-suite/src/RCrypto.java` (in `CORE_CLASSES`) gains
>   three checks that fail on the pre-fix behaviour:
>   `KeyGenerator.getInstance("AES","SunJCE").generateKey().getEncoded()` is
>   **32 bytes** — SunJCE's JDK 25 AES default is 256-bit, measured on the
>   oracle, not the 128 the deleted shim hardcoded — is **not all zeros**, and
>   `new SecretKeySpec(new byte[0], "AES")` is `IllegalArgumentException`.
> * **Still open, unchanged:** the synthetic `KeyGenerator` serving a wider set
>   than the seed advertises, `init(AlgorithmParameterSpec)` accepting and
>   ignoring, and `java/security/Key.getAlgorithm` hardcoding `"AES"`.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md) — TWO OF THE FOUR
> "recorded, NOT applied" PATCHES ARE IN THE TREE.**
>
> * **Patch A** (`register_keygen_dispatch` reads `keygen_default_bits`) —
>   **NOT APPLIED.** `native-builtins/src/jca/cipher.rs:3288-3315` still
>   hardcodes `ctx.set_field(obj, 1, Value::Int(128)); // default key size` in
>   both 2-arg overloads; `keygen_default_bits` never appears in `cipher.rs`.
>   **Live.**
> * **Patch B** (ChaCha20 wired, nonce from `ChaCha20ParameterSpec`) —
>   **APPLIED**, `native-builtins/src/jca/cipher.rs:536-600` and the family arm
>   at `:1244`.
> * **Patch C** (`SecretKeySpec` empty/null key ⇒ `IllegalArgumentException`) —
>   **NOT APPLIED.** `native-builtins/src/jca/cipher.rs:3936-3949` copies the
>   array with neither a length nor a null check. **Live.**
> * **Patch D** (`%02x` losing its zero pad for `0x01`–`0x0f`) — **APPLIED**,
>   commit `c3f7b2d78`; the zero-pad set at
>   `native-builtins/src/lang_string.rs:7030-7034` is now explicit and its
>   comment names the old "ends with an ASCII digit" defect.
>
> Other residuals still open: the synthetic `KeyGenerator` serves a wider
> algorithm set than the real-mode provider seed advertises;
> `KeyGenerator.init(AlgorithmParameterSpec)` accepts and ignores; and
> `java/security/Key.getAlgorithm` still hardcodes `"AES"`.

> **RE-GREPPED 2026-08-12 (crypto lane, second pass). One residual closed from
> the other side; two patches still live; one patch discharged by a third
> lane.**
>
> * **Patch A — STILL NOT APPLIED, re-anchored, and its preferred form now has
>   evidence behind it.** `native-builtins/src/jca/cipher.rs::
>   register_keygen_dispatch` is at `:3334`; the two hardcoded defaults are at
>   `:3346` and `:3358`, not `:3288-3315`. `keygen_default_bits` still never
>   appears in `cipher.rs`. **Take option 1 — drop both registrations.** The
>   option-1 precondition this record named ("re-measure the doc comment's
>   stated NPE at `service.getProvider()`; `provider_chain` has been seeded with
>   real `Service` entries since that comment was written") is now settled in
>   source rather than needing a run: W7-39 seeded twelve `KeyGenerator`
>   services on 2026-08-12 (`provider_chain.rs:1418-1440`), and that file's
>   comment at `:1401` states outright that `KeyGenerator` is **not** natively
>   intercepted in `--real-jdk` mode. These two registrations are the only thing
>   making that sentence false, and Result 4 above is the cost of keeping them:
>   `getInstance(algo, "SunJCE")` succeeds and the next call NPEs on
>   `this.spi`, in `Compatible` and `--jdk-only` alike.
> * **Patch C — STILL NOT APPLIED, re-anchored.** `register_param_specs` is at
>   `cipher.rs:4078`; the `SecretKeySpec.<init>` that copies the array with
>   neither a length nor a null check is at `:4154-4170`, not `:3936-3949`.
> * **Patch B — DISCHARGED by a third lane, in the form this record predicted.**
>   The concurrent-work note in Result 3 was right: the tree does now carry a
>   second, whole `native-builtins/src/chacha20.rs` with its own
>   `chacha20_block`, `chacha20_apply`, **`poly1305`** and AEAD. The two-cores
>   question resolved the way this record said it should — the one wired to
>   `Cipher` is the one that survived (`jca/cipher.rs:2282`/`:2296`,
>   `provider_chain.rs:1226-1227`), and it carries the RFC 8439 §2.5.2 Poly1305
>   vector that both this record and W7-15 set as the non-negotiable
>   precondition for advertising the AEAD.
> * **Residual CLOSED — "the synthetic `KeyGenerator` serves a wider set than
>   real mode advertises", and it closed from the direction this record argued
>   for.** This record called the three-name seed "the narrower and more
>   suspicious of the two". W7-39 widened it to twelve
>   (`AES`, `ARCFOUR`, `Blowfish`, `ChaCha20`, `DES`, `DESede`, `HmacMD5`,
>   `HmacSHA1`, `HmacSHA224/256/384/512` — `provider_chain.rs:1418-1440`),
>   matching the table in *"What the synthetic shim now does"* above, and
>   through the real SunJCE generator classes rather than a reimplementation.
> * **Two residuals unchanged.** `init(AlgorithmParameterSpec)` still accepts
>   and ignores, which remains defensible. `java/security/Key.getAlgorithm`
>   still answers `"AES"`: the fallback lives in
>   `native-builtins/src/phases_early.rs::carrier_algorithm` (`:14466-14473`)
>   and fires only when slot 1 is not a readable String. The blocker stated in
>   Result 2 is unchanged — `NativeContext` exposes no slot count and
>   `Heap::get_field` asserts rather than answering null — so this is not a
>   deferral, it is a missing capability. Synthetic-jdk only.

**Status:** FIXED in source 2026-08-11 (lane W7-21). **Nothing was rebuilt** —
this lane could not run `cargo build`, so every number below is either a
measurement taken against the *pre-fix* release binary at
`target/release/cratonvm.exe` (dated 2026-08-11 19:41) and HotSpot 25, or a
statement about source. No claim is made that the new code works.

Opened to discharge the three out-of-file patches recorded by
W7-15-cipher-silently-wrong-algorithm.md, which fixed `javax.crypto.Cipher` and
the `SecretKeySpec` aliasing that produced an all-zero AES key. All three
patches were re-measured before being applied, and **one of the three was
premised on a source claim that is backwards**. That is the main result here.

---

## Result 1 — the synthetic twin was the *shadowed* copy, not the shadowing one

W7-15's Patch 1 named a second `javax/crypto/spec/SecretKeySpec` in
`native-builtins/src/phases_early.rs::register_phase53_crypto`, carrying the
same aliasing `<init>`/`getEncoded`, and reasoned:

> That phase runs **after** `register_essential_natives`, and registration is
> last-write-wins, so under `--synthetic-jdk` this one shadows the fixed copy in
> `jca/cipher.rs` and the zero-key defect survives there.

The registration order is real; the call order is the other way round.
`register_synthetic_overrides` (`native-builtins/src/lib.rs`) does this, in this
sequence:

```rust
    register_phase53_natives(registry);          // -> register_phase53_crypto: the twin
    …
    crate::jca::cipher::register_cipher_clinit_shim(registry);   // -> register_param_specs: the fix
```

`register_cipher_clinit_shim` reaches `register_param_specs`, which registers
the *fixed* `SecretKeySpec`. It is called **after** `register_phase53_natives`,
so in synthetic mode the fixed copy overwrote the twin. And in real-JDK and
`--jdk-only` modes the twin never registered at all: `register_phase53_crypto`
is reachable only from `register_synthetic_overrides`, which is
`#[cfg(feature = "synthetic-jdk")]` *and* gated on `config.use_synthetic_jdk`
(`vm/src/vm/vm_init.rs`). The Cargo feature is not the runtime mode, and a
registrar behind both gates is absent from a default binary rather than
present-and-declining — `docs/architecture/natives-over-real-jdk-classes.md` §2.

**So the fixed copy won in every mode, and the twin ran nowhere.** §3 of that
same architecture document states the rule this lane needed and W7-15 missed:

> One registrar reachable only from `register_synthetic_overrides` — then it
> **cannot register at all** in real-JDK mode, and the other copy wins by
> default rather than by ordering. A "shadowing" verdict that ignores this is
> backwards.

### Measured, four ways, so this is not only a source reading

`--dump-native-registry` on the pre-built binary, real-JDK mode, is direct
evidence of what exists:

| class | registrations | which registrar |
|---|---|---|
| `javax/crypto/spec/SecretKeySpec` | **5** — `<clinit>`, `<init>([BLjava/lang/String;)V`, `getEncoded()[B`, `getAlgorithm`, `getFormat` | exactly `register_param_specs`'s set |
| `javax/crypto/spec/IvParameterSpec` | **2** — `<init>([B)V`, `getIV()[B` | exactly `register_param_specs`'s set |
| `javax/crypto/spec/GCMParameterSpec` | **3** — `<init>(I[B)V`, `getIV()[B`, `getTLen()I` | exactly `register_param_specs`'s set |
| `javax/crypto/KeyGenerator` | **2** — only the two 2-arg `getInstance` overloads | `register_keygen_dispatch` only |
| `javax/crypto/SecretKey` | **0** | — |
| `java/security/Key` | **0** | — |

The `KeyGenerator` row is the decisive one. `register_phase53_crypto` registers
the 1-arg `getInstance`, four `init` overloads and `generateKey`; **none of them
is in the registry.** Neither is either key carrier. That registrar is simply
not there in real-JDK mode.

Three behavioural corroborations, same binary, `--real-jdk`:

```
KEYGEN DES default EXC=java.security.NoSuchAlgorithmException: DES KeyGenerator not available
```
the twin's `getInstance` ignores the algorithm entirely and would have accepted
`DES`;

```
KEYGEN AES default len=32
```
the twin hardcodes 128 bits and would have answered 16 bytes;

```
KEYGEN DESede default alg=DESede
```
the twin's `SecretKey` carrier hardcodes `getAlgorithm()` to the string `"AES"`.

This also settles W7-15's stated falsifying observation ("if
`generateKey()` still returns zeros, a **third** `SecretKeySpec` registrar is
winning; find it with `--dump-native-registry`"). There is no third registrar:
five triples, one registrar, the fixed one.

### Deleted, not fixed — and the reachability is *why* that was available

The twin is deleted. Nine registrations go (`SecretKeySpec` ×4,
`IvParameterSpec` ×2, `GCMParameterSpec` ×3), and `register_param_specs`
registers all nine plus a `<clinit>`, so the surviving surface is a strict
superset. A tenth, `Cipher.getIV()[B`, went with them for the same reason —
real `Cipher.getIV` ends `return (iv == null) ? null : iv.clone()`
(`CipherCore.getIV`, JDK 25 `src.zip`), this copy handed back the stored array,
and `jca::cipher`'s copy builds a fresh one from its own side table.

Deleting rather than correcting is the point. A shadowed second implementation
of a key-material primitive is **not a dormant defect, it is a live trap**:
swapping those two call sites in `register_synthetic_overrides` — an edit nobody
would read as touching crypto — would silently restore the all-zero-key bug,
and no test in the tree would notice. W7-15's own recommendation
("**Better still: delete both registrations**") was right for a reason it did
not have.

The deleted `<init>` carried a second, unrelated defect worth recording because
it is a pattern: it returned `Ok(Some(Value::Object(None)))` from a `void`
method, where the fixed copy returns `Ok(None)`. `native-builtins/src/lib.rs`
documents that exact shape elsewhere as pushing "a spurious null onto the
operand stack".

**Mode: synthetic-jdk only.** Nothing changes in `Compatible` or `--jdk-only`,
because nothing in this registrar was ever registered there.

---

## Result 2 — `KeyGenerator` never read the algorithm it was given

W7-15's Patch 2 named the hardcoded 128-bit default. Re-measuring it moved the
diagnosis twice.

**First**, the defect is **synthetic-mode only, plus one live real-mode
casualty**. In real-JDK mode the real SunJCE `KeyGenerator` classes run and the
defaults are already right — measured, and identical to HotSpot:

```
                CratonVM --real-jdk    HotSpot 25
AES             len=32                 len=32
DESede          len=24                 len=24
HmacSHA256      len=32                 len=32
```

`getInstance` for `DES`, `HmacSHA1/224/384/512`, `Blowfish`, `ChaCha20`, `RC2`
and `ARCFOUR` is refused in real mode with the JCA's own
`NoSuchAlgorithmException: <a> KeyGenerator not available`, because the SunJCE
seed advertises only the three. The hardcoded 128 lives in
`register_phase53_crypto` (synthetic) and in `register_keygen_dispatch`'s two
2-arg overloads (all modes — see Result 4).

**Second**, the DESede warning in W7-15 was aimed at the wrong arm. In real mode
DESede is already perfect, because the real `DESedeKeyGenerator` bytecode runs:

```
KEYGEN DESede init(112) 24 bytes, every byte odd parity, K3 == K1
KEYGEN DESede init(168) 24 bytes, every byte odd parity, K3 != K1
KEYGEN DESede init(128) InvalidParameterException: Wrong keysize: must be equal to 112 or 168
```

byte-for-byte the same shapes HotSpot produces. It is the **synthetic** shim
that had no parity, no 24-byte rule and no size validation — it would have
handed back 16 unconditioned random bytes for a "DESede" key.

### What the synthetic shim now does, and where every number came from

Nothing here was recalled. The defaults are not a family rule and several are
surprising, so each row is `getEncoded().length * 8` measured on HotSpot 25:

| algorithm | default bits | bytes | notes |
|---|---|---|---|
| `AES` | 256 | 32 | 256 since JDK 21; the shim said 128 |
| `DESede` | 168 | **24** | bits ≠ bytes×8 — the parity bits are carried, not counted |
| `DES` | 56 | **8** | same |
| `HmacSHA1`, `HmacMD5` | 512 | 64 | the **block** size, not the digest size |
| `HmacSHA224` | 224 | 28 | digest size |
| `HmacSHA256` | 256 | 32 | |
| `HmacSHA384` | 384 | 48 | |
| `HmacSHA512` | 512 | 64 | |
| `Blowfish`, `RC2`, `ARCFOUR` | 128 | 16 | |
| `ChaCha20` | 256 | 32 | key generation only; `Cipher.getInstance("ChaCha20")` stays refused |
| anything else | — | — | `NoSuchAlgorithmException`, no default arm |

`None` means refuse, as in the `Cipher` admission table and for the same reason:
a default arm is how `getInstance("Blowfish")` came to answer with a key
generated under another algorithm's rules.

Size admission, in HotSpot's measured wording:

```
AES    init(129) -> java.security.InvalidParameterException: Wrong keysize: must be equal to 128, 192 or 256
DESede init(128) -> java.security.InvalidParameterException: Wrong keysize: must be equal to 112 or 168
```

The HMAC family has no fixed set — HotSpot really does return an 8-byte key for
`HmacSHA256` `init(64)` — so any positive multiple of 8 is accepted, which is
the JCE rule.

`init(SecureRandom)` reset the size to the literal 128 as well. It now resets to
the *algorithm's* default, which is what "re-initialise to the provider default"
means.

### The DESede parity bits, verified rather than asserted

This is the part W7-15 flagged as "genuine crypto work and should not be landed
unverified", so it was verified against the JDK's own validator rather than
against a recollection of FIPS 46-3.

The rule — SunJCE's `DESKeyGenerator.setParityBit` — is that the low bit of each
byte is set so the byte's population count is **odd**. Transcribed into Java,
run on a fixed non-random buffer, and handed to
`javax.crypto.spec.DESedeKeySpec.isParityAdjusted`:

```
raw24    = 00070e151c232a31383f464d545b626970777e858c939aa1
parity24 = 01070e151c232a31383e464c545b626870767f858c929ba1
DESedeKeySpec.isParityAdjusted(parity24, 0) = true

raw8     = 101b26313c47525d
parity8  = 101a26313d46525d
DESKeySpec.isParityAdjusted(parity8, 0)     = true
```

Identical on HotSpot 25 and on CratonVM's pre-built binary. Both vectors are now
Rust unit tests (`des_parity_matches_the_jdk_checker`, `des_parity_single_block`)
alongside two properties a merely-plausible rule would fail: parity must be
**idempotent** (a rule that flipped bit 0 unconditionally passes the vector and
fails this), and the 2-key fold must not disturb it.

The 112-bit shape was measured, not assumed. SunJCE's `DESedeKeyGenerator` draws
16 bytes for `init(112)` and copies K1 over K3; three HotSpot runs gave
`K3==K1: true, K2==K1: false, isParityAdjusted: true` every time. The shim now
parity-adjusts and then folds, in that order, mirroring SunJCE (which adjusts
the 16-byte draw before copying, so K3 arrives already adjusted).

**Mode: synthetic-jdk only.**

### The key carrier

`generateKey`'s `javax/crypto/SecretKey` carrier had one slot, and its
`getAlgorithm()` answered the string `"AES"` for every algorithm — so
`KeyGenerator.getInstance("DESede").generateKey().getAlgorithm()` returned
`"AES"` in synthetic mode. A key that misreports its own algorithm is the same
species of defect as a cipher that ignores the requested one: every caller that
branches on `getAlgorithm()` branches wrong, with nothing raised. The algorithm
now travels in slot 1. `generateKey` is the only allocator of this shape, so
there is no older 1-slot instance to stay compatible with.

`java/security/Key.getAlgorithm` was deliberately **left** answering `"AES"`.
That triple serves any object whose own class is literally `java/security/Key`,
this file alone mints several unrelated key shapes, `NativeContext` exposes no
slot count, and `Heap::get_field` **asserts** on an out-of-range index rather
than answering null — so a speculative read of slot 1 there would turn an
unknown 1-slot carrier into a panic. Narrowing the fix to the carrier whose
allocators are all known is the part that can be made safe without building.

---

## Result 3 — ChaCha20: one core, generalised in place; Poly1305 still absent

W7-15's Patch 3 was verified against the source and was accurate:
`crypto_impl::chacha20_keystream_fill` is a correct RFC 8439 core that (a)
writes its keystream instead of XOR-ing and (b) hardcodes the block counter to
0.

It is now `chacha20_xor(key, nonce, initial_counter, buf)`, with
`chacha20_keystream_fill` retained as a wrapper that zeroes the buffer first —
so `secure_random_fill`'s fallback behaviour is unchanged and there is **one**
ChaCha20 in this file, not two. The zeroing is load-bearing: the fallback is
handed a caller's buffer whose prior contents are arbitrary, and entropy that
depends on them is entropy nobody audited.

The vectors are **measured, not recalled**. HotSpot 25's own SunJCE
`Cipher.getInstance("ChaCha20")` was initialised with
`ChaCha20ParameterSpec(nonce, counter)` and asked to encrypt an all-zero
plaintext, which yields the raw keystream:

| key | nonce | counter | first 32 bytes |
|---|---|---|---|
| `00…00` | `00…00` | 0 | `76b8e0ada0f13d90405d6ae55386bd28bdd219b8a08ded1aa836efcc8b770dc7` |
| `0001…1f` | `000000000000004a00000000` | 1 | `224f51f3401bd9e12fde276fb8631ded8c131f823d2c06e27e4fcaec9ef3cf78` |
| `0001…1f` | `000000000000004a00000000` | 0 | `af051e40bba0354981329a806a140eafd258a22a6dcb4bb9f6569cb3efe2deaf` |

Two things fall out of that table, and both are now tests:

* the first row is **byte-identical to the expectation the in-tree KAT already
  carried**, which is independent evidence that the pre-existing core was
  already correct. What it lacked was a counter and an XOR, not a working
  permutation.
* rows 2 and 3 are the same key and nonce, so the counter is the only variable —
  and block 2 of the counter-0 stream equals block 1 of the counter-1 stream.
  `chacha20_xor_counter_is_a_block_index` asserts exactly that. It is the test a
  core that *ignored* `initial_counter` could not pass, whereas it would still
  pass a vector test whose expectation had been taken from itself.

Two more: a 70-byte request (partial final block, also measured from HotSpot)
and the involution property — XOR-ing the same keystream twice restores the
plaintext, which is what makes the function a cipher and what the old
`copy_from_slice` body did not have.

**`Cipher.getInstance("ChaCha20")` is still refused, and this lane did not
change that.** Wiring the family needs a `CipherFamily::ChaCha20` arm in
`jca/cipher.rs` and a name added back to the `SunJCE` seed in
`provider_chain.rs`; both files are outside this lane. The generalised core is
reachable but unwired, which is a state this campaign has a name for — see
"an INERT registration looks exactly like a MISSING feature". It is recorded
here so the next reader does not mistake `chacha20_xor`'s existence for a
working ChaCha20.

**`ChaCha20-Poly1305` stays refused, and must.** There is no Poly1305 in this
tree. An AEAD transformation whose `doFinal` cannot fail on a bad tag is worse
than a missing cipher — W7-15 measured CratonVM accepting a tampered ciphertext
where HotSpot raises `AEADBadTagException: Tag mismatch` — and adding a real
ChaCha20 without its authenticator would make that *easier* to reach, not
harder. Implement first, advertise second.

> **Concurrent-work note.** While this lane ran, another worktree
> (`fix/chacha20-real-cipher-20260811`) was committing a new
> `native-builtins/src/chacha20.rs` carrying its own `chacha20_block`,
> `chacha20_apply`, `poly1305` and AEAD encrypt/decrypt, cross-checked against a
> crate. If both land, **the tree has two ChaCha20 cores**, which is precisely
> the shape Result 1 above is about. They should not both survive the merge. The
> one to keep is whichever ends up wired to `Cipher`; the other's KATs should be
> re-pointed at the survivor rather than deleted, since the two sets were
> derived from different oracles and agreeing is worth knowing. If that lane
> lands a Poly1305 with its RFC 8439 §2.5.2 test vector, the refusal above is
> discharged — by that lane, not this one.

**Mode: all modes structurally** (`crypto_impl` is not feature-gated), **no
behaviour change in any mode**, because the only caller today is
`secure_random_fill`'s OS-entropy fallback and its output is unchanged.

---

## Result 4 — a new defect, found while measuring: the 2-arg `getInstance` NPEs

Not previously recorded, and **live in `Compatible` and `--jdk-only`**, not just
synthetic. Measured on the pre-built binary, `--jdk-only`:

```
KG2 AES/SunJCE        EXC=java.lang.NullPointerException: Cannot invoke "javax.crypto.KeyGeneratorSpi.engineGenerateKey()" because "this.spi" is null
KG2 DESede/SunJCE     EXC=java.lang.NullPointerException: … "this.spi" is null
KG2 HmacSHA256/SunJCE EXC=java.lang.NullPointerException: … "this.spi" is null
KG2 AES init(256)     EXC=java.lang.NullPointerException: Cannot invoke "javax.crypto.KeyGeneratorSpi.engineInit(int, java.security.SecureRandom)" because "this.spi" is null
```

HotSpot returns a key for all four.

The cause is visible in the registry dump above: `register_keygen_dispatch`
registers the two 2-arg `getInstance` overloads in **all** modes, and they
allocate a 2-field synthetic `KeyGenerator` — but `init` and `generateKey` are
registered only by `register_phase53_crypto`, which is synthetic-only. So in
real-JDK mode `getInstance("AES", "SunJCE")` hands back a synthetic object that
only the *absent* shims know how to drive, and the next call falls through to
real JDK bytecode which reads `this.spi` and finds null.

Its own doc comment says the overloads exist because the real path "NPEs at
`service.getProvider()`". It replaced one NPE with another, one method later.
That is the half-wired shape: an object model split across two registrars where
only one of them is reachable.

`register_keygen_dispatch` lives in `jca/cipher.rs`, outside this lane — patch
recorded below.

---

## Every place these two files hand key material across the boundary

The rule is the JDK's: key arrays are copied in **both** directions, because
callers scrub. `AESKeyGenerator.engineGenerateKey` scrubs its buffer the instant
the key object exists, which is how aliasing produced an all-zero key.

| site | before | now |
|---|---|---|
| `phases_early` `SecretKeySpec.<init>` | aliased the caller's array | **deleted** — `register_param_specs` clones |
| `phases_early` `SecretKeySpec.getEncoded` | returned the stored array | **deleted** — `register_param_specs` clones |
| `phases_early` `IvParameterSpec.getIV` | returned the stored array | **deleted** |
| `phases_early` `GCMParameterSpec.getIV` | returned the stored array | **deleted** |
| `phases_early` `Cipher.getIV` | returned the stored array | **deleted** |
| `phases_early` `SecretKey.getEncoded` | returned the stored array | **copies** (`clone_key_bytes_field`) |
| `phases_early` `Key.getEncoded` | returned the stored array | **copies** |
| `phases_early` `KeyGenerator.generateKey` | scrubs its Rust-side buffer already | unchanged, still scrubs |
| `phases_early` `IvParameterSpec`/`GCMParameterSpec` `<init>` | already copied | deleted as duplicates |
| `crypto_impl` `SecureRandom.nextBytes` | fills the caller's array | unchanged — correct |
| `crypto_impl` `SecureRandom.generateSeed` | returns a fresh array | unchanged — correct |
| `crypto_impl` `SecureRandom.setSeed([B)` / `<init>([B)` | reads, retains nothing | unchanged — correct |

`crypto_impl` registers natives for `java/security/SecureRandom` and nothing
else; every other function in it takes `&[u8]` and returns `Vec<u8>`, so it
copies by construction. **No aliasing key-material boundary remains in either
file.**

---

## Before and after

"Before" is measured on the pre-built binary. **"After" is source-level intent —
nothing was rebuilt.** Synthetic-mode "before" rows are read from the source of
the shim that was live there, since this binary carries no `synthetic-jdk`
feature and refuses `--synthetic-jdk` outright.

| call | before | after |
|---|---|---|
| `--real-jdk` `KeyGenerator.getInstance("AES").generateKey()` | `0000…0000` (32 zero bytes) | random 32 — fixed on `dev` by W7-15's `SecretKeySpec` clone, not by this lane |
| `--real-jdk` same, `HmacSHA256` | `0000…0000` | as above |
| `--real-jdk` same, `DESede` | random 24, odd parity — already correct | unchanged |
| `--real-jdk` `getInstance("AES","SunJCE").generateKey()` | `NullPointerException: this.spi is null` | unchanged by this lane — patch recorded, file out of scope |
| synthetic `getInstance("AES").generateKey()` | 16 bytes | 32 bytes |
| synthetic `getInstance("DESede").generateKey()` | 16 unconditioned bytes | 24 bytes, odd parity per byte |
| synthetic `getInstance("DESede").init(112)` | 14 bytes | 24 bytes, odd parity, K3 == K1 |
| synthetic `getInstance("DESede").init(128)` | accepted, 16 bytes | `InvalidParameterException: Wrong keysize: must be equal to 112 or 168` |
| synthetic `getInstance("AES").init(129)` | accepted, 16 bytes | `InvalidParameterException: Wrong keysize: must be equal to 128, 192 or 256` |
| synthetic `getInstance("HmacSHA1").generateKey()` | 16 bytes | 64 bytes |
| synthetic `getInstance("Blowfish").generateKey()` | 16 bytes under a wrong default | 16 bytes at the measured default |
| synthetic `getInstance("CRATONVM-NO-SUCH-KEYGEN")` | accepted, 16 bytes | `NoSuchAlgorithmException: … KeyGenerator not available` |
| synthetic `generateKey().getAlgorithm()` | `"AES"` for every algorithm | the requested algorithm |
| synthetic `SecretKey.getEncoded()` then scrub the result | mutated the key | unaffected |
| `Cipher.getInstance("ChaCha20")` | refused (fixed by W7-15) | still refused |
| `Cipher.getInstance("ChaCha20-Poly1305")` | refused (fixed by W7-15) | still refused, deliberately |

The pre-built binary predates W7-15's fix in one direction only: it still shows
the **old** `SecretKeySpec` aliasing, so the two all-zero rows above are
observations of a defect already fixed on `dev`. They are listed because they
are what this lane could observe, not because they are outstanding.

---

## Out-of-file patches — recorded, NOT applied

Code is exact. Anchor on the enclosing function name; line numbers rot.

### Patch A — `register_keygen_dispatch` mints a KeyGenerator nothing can drive

`native-builtins/src/jca/cipher.rs`. This is Result 4, and it is the highest
priority item here because it is the only one live outside synthetic mode.

Both overloads in `register_keygen_dispatch` allocate a synthetic
`KeyGenerator` whose `init`/`generateKey` shims are registered only by
`phases_early::register_phase53_crypto`, which does not run in real-JDK mode.
Two candidate fixes, in preference order:

1. **Drop both registrations.** Then `getInstance(algo, "SunJCE")` runs real
   JDK bytecode end to end, which is what already works for the 1-arg form —
   measured, all three algorithms, correct lengths and non-zero bytes. The doc
   comment's stated reason for the shims (an NPE at `service.getProvider()`)
   should be re-measured first: `provider_chain` has been seeded with real
   `Service` entries since that comment was written, and the 1-arg form's
   success suggests the lookup now resolves.

2. If the shims must stay, they must register `init`/`generateKey` in all modes
   too, and share `phases_early`'s algorithm table rather than restating 128:

```rust
    // SunJCE's per-algorithm default, not a literal — see
    // `phases_early::keygen_default_bits`. AES is 256, DESede 168.
    let bits = match keygen_default_bits(&algo_str) {
        Some(bits) => bits,
        None => return Err(throw_jca_exc(
            ctx,
            "java/security/NoSuchAlgorithmException",
            &format!("{algo_str} KeyGenerator not available"),
        )),
    };
    ctx.set_field(obj, 1, Value::Int(bits));
```

Note the hazard either way: a `getInstance` that succeeds while `generateKey`
cannot is worse than a `getInstance` that refuses, because the failure surfaces
at a call site that names nothing about providers.

### Patch B — wire ChaCha20, once, after Poly1305 exists

`native-builtins/src/jca/cipher.rs` and
`native-builtins/src/jca/provider_chain.rs`. `crypto_impl::chacha20_xor` is
ready; what is missing is a `CipherFamily::ChaCha20` arm reading the 12-byte
nonce from `ChaCha20ParameterSpec` field 0 and the counter from field 1, and
then the name added back to `seed_direct_native_engine_services` with the
matching arm deleted from
`every_advertised_sunjce_cipher_is_serviceable`.

**Do this only after resolving the two-cores question** in the concurrent-work
note above. Wiring `Cipher` to one core while the other keeps the KATs is how a
tree ends up with a tested implementation and an untested one that runs.

`ChaCha20-Poly1305` stays out of the seed until a Poly1305 with its RFC 8439
§2.5.2 vector exists. That order is not negotiable and
`every_advertised_sunjce_cipher_is_serviceable` enforces it.

### Patch C — `SecretKeySpec` accepts an empty key

`native-builtins/src/jca/cipher.rs::register_param_specs`. Measured:

```
                          CratonVM --real-jdk                  HotSpot 25
new SecretKeySpec(new byte[0], "AES")   ACCEPTED               IllegalArgumentException: Empty key
new SecretKeySpec(null, "AES")          NullPointerException: null object argument
                                                               IllegalArgumentException: Missing argument
```

The real `<init>` is:

```java
    if (key == null || algorithm == null) throw new IllegalArgumentException("Missing argument");
    if (key.length == 0) throw new IllegalArgumentException("Empty key");
```

A zero-length key is exactly the artefact the all-zero-key defect family
produces, so accepting it silently removes the one place the platform would have
caught it. The null case is a *class* difference, not just wording — a caller
catching `IllegalArgumentException` does not catch `NullPointerException`.

### Patch D — `String.format("%02x")` drops the zero pad for `0x01`–`0x0f`

Not a crypto defect, but it corrupts crypto probes, which is how it was found.
Measured, `--jdk-only`:

```
String.format(%02x) over {00,01,0e,0f,10,ff} = [0001 e f10ff]   HotSpot: [00010e0f10ff]
```

`0x00` and `0x01` pad correctly; `0x0e` and `0x0f` render as `" e"` and `" f"`.
Any probe that hex-dumps key material with `%02x` silently produces
mis-aligned, unequal-length strings — the first version of this lane's own
`KeyGenerator` census did, and its DESede rows were unusable until the padding
was done by hand. Belongs with the format-conversion work in
W7-3-format-conversions-and-stringbuilder-bounds.md.

---

## What is deliberately still missing

* **Synthetic `KeyGenerator` serves a wider set than real mode advertises.** The
  shim now generates for the algorithm set HotSpot's SunJCE serves (the table
  above), while the real-mode provider seed advertises three. That asymmetry is not new and is
  not made worse here, but the two should be reconciled — the seed is the
  narrower and more suspicious of the two, since `DES` and the wider HMAC family
  are all things this VM can generate correctly.
* **`KeyGenerator.init(AlgorithmParameterSpec)`** still accepts and ignores.
  That is defensible (the interface is an empty marker) and unchanged.
* **`java/security/Key.getAlgorithm`** still answers `"AES"`. Closing it needs a
  slot count on `NativeContext`, or every allocator of a `java/security/Key`-
  classed object audited; see Result 2.

---

## How to verify

Rebuild, then re-run the two probes in both arms. `KeyMaterialCensus` and
`KeyGenProbe2` print bytes rather than verdicts so the runs diff line for line —
and note Patch D: do the hex padding by hand, not with `%02x`.

Rows that must change, synthetic mode:

```
KEYGEN AES default len=32          (was 16)
KEYGEN DESede default len=24 parity=oooooooooooooooooooooooo   (was 16, no parity)
KEYGEN DESede init(112) K3==K1:true
KEYGEN DESede init(128) EXC=java.security.InvalidParameterException: Wrong keysize: must be equal to 112 or 168
KEYGEN <unsupported> EXC=java.security.NoSuchAlgorithmException: … KeyGenerator not available
generateKey().getAlgorithm() == the requested algorithm
```

Rows that must **not** change, any mode: every `AES/GCM/NoPadding`,
`AES/CBC/*`, `AES/ECB/*`, `RSA*` and `AESWrap*` ciphertext, and every
`SecureRandom` behaviour — `chacha20_keystream_fill`'s output is unchanged by
construction and its KAT proves it.

`cargo test -p cratonvm-native-builtins` should pick up eleven new pure-function
tests (four ChaCha20, seven KeyGenerator/parity) that need no VM. The existing
`key_generator_basics` in `vm/src/vm/tests.rs` calls `init(256)` before
`generateKey` and asserts 32 bytes; it stays green.

## The single falsifying observation

If a rebuilt binary refuses `KeyGenerator.getInstance("AES")` in **real-JDK**
mode, then `keygen_default_bits`'s table has been reached from a path this lane
believed was synthetic-only, and the reachability argument in Result 1 is wrong
in the direction that matters. The registry dump above is the check: real-JDK
mode must show exactly two `javax/crypto/KeyGenerator` registrations, both
`getInstance`, and zero `javax/crypto/SecretKey` registrations. If it shows
`generateKey`, `register_phase53_crypto` is running somewhere this document says
it cannot, and every "synthetic-jdk only" claim here needs re-deriving before
anything else is changed.
