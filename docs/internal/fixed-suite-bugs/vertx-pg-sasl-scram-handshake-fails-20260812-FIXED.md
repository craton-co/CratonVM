# Vert.x reactive Postgres SASL/SCRAM handshake fails — `expected SASL response, got message type 88 (08P01)` — FIXED

**Status:** FIXED (2026-08-12). Filed the same day against a real-Postgres,
3-GC-variant run of the hibernate-reactive suite (206 classes, `-Ddb=PostgreSQL`,
Testcontainers `postgres:18.4`) on Azure host `azureuser@20.80.105.49`, where it
failed essentially every DB-required class regardless of GC variant.

Root cause was **one unregistered method overload**, and it had nothing to do
with SASL, SCRAM, or the network.

## What it looked like

```
io.vertx.pgclient.PgException: FATAL: expected SASL response, got message type 88 (08P01)
```

The original filing read this as CratonVM emitting a malformed
`SASLInitialResponse`/`SASLResponse`, and looked for a broken
HMAC/PBKDF2/SecureRandom primitive corrupting the proof or a length prefix.

That reading was wrong in an instructive way. **Message type 88 is `'X'` —
Postgres's frontend Terminate message.** The server was not rejecting a corrupt
SASL payload; it was reporting that the client had hung up mid-handshake. The
defect was entirely client-side and the exception that caused it was being
swallowed by Vert.x's connection future.

## Root cause

`javax/crypto/Mac.doFinal([BI)V` — the two-argument output-buffer overload — was
never registered as a native.

`Mac` is served by natives that keep their state **off-object**, in
`phases_late::ssl_security::mac_state_table`, keyed by identity hash. The real
`javax.crypto.Mac` instance fields (`initialized`, `spi`, `provider`, `lock`) are
therefore never written. Any overload left unregistered runs the REAL JDK body,
reads `initialized == false`, and throws:

```
java.lang.IllegalStateException: MAC not initialized
    at javax.crypto.Mac.doFinal(Mac.java:620)
    at com.ongres.scram.common.CryptoUtil.hi(CryptoUtil.java:119)
    at com.ongres.scram.common.ScramMechanism.saltedPassword(ScramMechanism.java:212)
    at com.ongres.scram.common.ScramFunctions.saltedPassword(ScramFunctions.java:46)
    at com.ongres.scram.client.ClientFinalProcessor.<init>(ClientFinalProcessor.java:109)
    at com.ongres.scram.client.ScramClient.clientFinalMessage(ScramClient.java:188)
```

`CryptoUtil.hi` is SCRAM's PBKDF2. Its loop is
`update(byte[]); doFinal(byte[], 0)` — so it died on **iteration 2 of 4096**,
immediately after a no-argument `doFinal()` had already succeeded. `Mac` looked
completely healthy right up to the one call that wasn't there.

This is the defect species to remember: **an unregistered overload on an
off-object-state engine does not present as a missing feature.** It presents as
the object being in an impossible state.

### Why it hit two "different" SCRAM implementations

The filing treated `postgres-scram-sha256-pbkdf2-hmacsha384-missing-20260807`
(pgjdbc, shaded `ongres-scram`, client-side `SecurityException`) as probably
unrelated: different library, different symptom. They were closer than that.
`io.vertx:vertx-pg-client:5.1.5` does not implement its own SCRAM — it depends on
`com.ongres.scram:scram-client:3.4`, the *unshaded* copy of the same library
pgjdbc shades. Both reach `CryptoUtil.hi`, so both were blocked by this one
overload. That earlier doc's headline gap (`PBKDF2WithHmacSHA384` missing from
`SecretKeyFactory`) had already been closed by `e0598dc55`; what remained was
this, plus the `getProvider()` residual below.

## Fixes

All verified against stock HotSpot 25 on the same host, same classpath.

1. **`Mac.doFinal([BI)V` registered** — the blocker. Follows the JDK's ordering:
   initialized-check, then output-buffer size check, then consume the
   accumulator, so a `ShortBufferException` leaves the buffered data intact. The
   exception raised is a genuine, checked `javax.crypto.ShortBufferException`
   built through `new_object_initialized`, not an unchecked `RuntimeError` that
   would sail past a caller's `catch`.

2. **Three more `Mac` overloads registered**, all with the same failure mode
   latent in them: `init(Key, AlgorithmParameterSpec)`,
   `update(java.nio.ByteBuffer)` (drained through the buffer's own `get(byte[])`
   so heap and direct buffers take one path), and `getProvider()` (whose real
   body opens `synchronized (lock)` on a field the synthetic never wrote).

3. **A per-descriptor registration census** —
   `phases_late::every_public_mac_method_is_registered` — asserts all sixteen
   public `Mac` methods are registered. A census is the only thing that catches
   the next one; every other `Mac` caller in the tree stayed green through this
   entire defect.

4. **`SecretKeyFactory.getAlgorithm()` / `getProvider()` registered.** Same
   species: `getProvider()` threw `NullPointerException: Cannot enter
   synchronized block because "this.lock" is null` on a factory that derived keys
   correctly. Registered in `jca::cipher` — **not only** beside the identical
   trio in `phases_early::register_phase53_natives`, which is reachable only from
   `register_synthetic_overrides` and so is `#[cfg(feature = "synthetic-jdk")]`.
   Registering it there alone left it inert on a default real-JDK run, looking
   exactly like no fix at all.

5. **Seven previously-refused MAC algorithms implemented**: `HmacSHA224`,
   `HmacSHA512/224`, `HmacSHA512/256`, and `HmacSHA3-224/256/384/512`. These were
   refused with `NoSuchAlgorithmException` on the stated grounds that each needs
   its own HMAC block size and that lane could neither build nor run. The block
   sizes are 64 for SHA-224, 128 for the SHA-512 truncations, and the SHA-3
   sponge RATE — 144/136/104/72 — which SHRINKS as the digest grows. All seven
   are pinned to HotSpot-measured vectors, including a 200-byte-key set: a key
   longer than the block is hashed down first, which is the only path where the
   block-size constant changes the answer, so a short-key vector alone would not
   have caught a wrong one. `jca::provider_chain` advertises exactly the same
   twelve names `mac_algorithm_supported` serves, asserted in lockstep.

   Not academic: `com.ongres.scram` builds its advertised mechanism list by
   probing `Mac.getInstance`, so refusing these made CratonVM offer 8
   SCRAM mechanisms where HotSpot offers 12.

Still refused, deliberately: `Poly1305`, `AESCMAC`, `HmacPBESHA*` and the rest of
HotSpot's 28 `Mac` names. They are not HMAC-over-a-digest, and serving them would
mean a second unverified construction — the mistake the old
`_ => hmac_sha256(key, data)` fallback was removed for.

## Verification

A standalone `postgres:18.4` container with
`--auth-host=scram-sha-256 --auth-local=scram-sha-256` (`pg_hba.conf` confirmed
`host all all all scram-sha-256`), driven by two probes on the suite's own
classpath.

| arm | Vert.x `PgConnection.connect` | pgjdbc `DriverManager.getConnection` |
|---|---|---|
| HotSpot 25 | PASS | PASS |
| CratonVM, before | `expected SASL response, got message type 88 (08P01)` | — |
| CratonVM, after | PASS — connect, `select 40+2` → 42, clean close | PASS — full `SASLInitialResponse`→`SASLContinue`→`SASLResponse`→`SASLFinal`→`AuthenticationOk` trace |

RFC 7677's SCRAM-SHA-256 test vector, computed through `com.ongres.scram`'s own
`ScramFunctions` and through a full `ScramClient` message flow with a fixed
nonce, matches byte-for-byte after the fix (`clientProof`, `serverSignature`, and
the complete `client-final-message`); before the fix it threw at
`saltedPassword`.

## Related

- `jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — the other defect from
  the same investigation, fixed alongside this one. Unrelated root cause (a JNI
  handle encoding), same lesson about a symptom naming the wrong subsystem.
- `postgres-scram-sha256-pbkdf2-hmacsha384-missing-20260807` — the pgjdbc-side
  SCRAM filing, closed by this work plus `e0598dc55`.
