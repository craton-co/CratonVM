# crypto/elytron: three distinct missing JCA engine-class registrations (KeyGenerator HmacSHA256, AlgorithmParameters OAEP, KeyStore BCFKS)

Status: open — three separate "not implemented" / "not available" gaps found together while triaging crypto/elytron

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Summary

Three distinct JCA engine-class algorithm lookups fail under CratonVM's crypto provider registration, each in a
different `crypto/elytron` test class:

1. **`ElytronHmacTest::testHmacSignaturesUsingKeyGen`** (1 of 3 tests in this class fails, others pass):
   ```
   => java.security.NoSuchAlgorithmException: HmacSHA256 KeyGenerator not available
      javax.crypto.KeyGenerator.<init>(KeyGenerator.java:176)
      javax.crypto.KeyGenerator.getInstance(KeyGenerator.java:244)
      org.keycloak.crypto.elytron.test.ElytronHmacTest.testHmacSignaturesUsingKeyGen(ElytronHmacTest.java:41)
   ```
   Notably, `HmacSHA256` works fine via `Mac.getInstance`/`SecretKeyFactory` elsewhere in the same class (the
   other 2 tests pass) — it's specifically the `KeyGenerator` engine class for this algorithm that's missing,
   while `Mac`/`SecretKeyFactory` for the same algorithm name are registered.

2. **`ElytronCryptoJWETest`** (CRASH, whole class):
   ```
   [cratonvm] main-vm run() returned Err: Error in thread "main" runtime error: not implemented: no AlgorithmParameters OAEP implementation in any provider
   ```
   `AlgorithmParameters.getInstance("OAEP")` (or similar) has no registered implementation at all — this is a
   direct CratonVM VM-level "not implemented" panic/error (not a Java-level exception the test could catch),
   crashing the whole process rather than failing a single test.

3. **`ElytronKeyStoreTypesTest`** (CRASH, whole class):
   ```
   [cratonvm] main-vm run() returned Err: Error in thread "main" runtime error: not implemented: no KeyStore BCFKS implementation in any provider
   ```
   Same pattern — `KeyStore.getInstance("BCFKS")` has no registered implementation under the Elytron provider
   configuration, causing a full crash rather than a catchable Java exception.

## Notes

- These are consistent with the general pattern already tracked elsewhere in this project of gaps in CratonVM's
  JCA engine-class registration tables (see memory: SegmentVarHandle is_vh prefix gap, RSA-OAEP/PSS cipher work,
  JCA synthetic crypto layers) — this session adds three more concrete missing/incomplete registrations to that
  list, specific to the Elytron crypto provider path.
- Items 2 and 3 crash the entire process (a Rust-level `not implemented` error surfaces as `[cratonvm] main-vm
  run() returned Err`), rather than being caught and surfaced as a normal Java `NoSuchAlgorithmException` the way
  item 1 is — this inconsistency (some missing-algorithm cases throw a catchable Java exception, others crash the
  whole VM process) is itself worth noting: ideally all "algorithm not found" cases should surface uniformly as
  `NoSuchAlgorithmException`/`NoSuchProviderException` rather than some being unrecoverable native panics.
- Separately (already flagged in the sibling KeyFactory/X509Extension doc from the same triage pass): BCFKS
  keystores also show up **corrupted on load** (`MAC calculation failed`) in a different module (`tests/base`'s
  `MutualTLSClientTest`) via `crypto/default`, not `crypto/elytron` — that's a different provider/code path where
  BCFKS clearly *is* at least partially implemented (it gets far enough to attempt MAC verification and fail),
  unlike here where BCFKS is reported as having no implementation at all under Elytron. These may or may not
  share underlying code; treat as related-but-distinct until investigated together.

## Next steps

1. Search CratonVM's JCA provider-registration source (likely `native-builtins/src/` — wherever
   `KeyGenerator`/`AlgorithmParameters`/`KeyStore` engine-class dispatch tables are defined) for the Elytron
   provider's registered algorithm list, and add the missing `HmacSHA256` `KeyGenerator`, `OAEP`
   `AlgorithmParameters`, and `BCFKS` `KeyStore` entries (or determine why they're conditionally excluded if
   that's intentional).
2. Consider whether "algorithm/provider not found" should always throw a catchable
   `NoSuchAlgorithmException`/`NoSuchProviderException` at the Java level instead of sometimes crashing the whole
   process via a Rust-level `not implemented` panic — items 2/3 here vs item 1's clean exception is an
   inconsistency worth fixing regardless of whether the underlying algorithms get implemented.

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.83.144.174
cd /data/data/data/wt-keycloak-nonpassed-1200-20260706
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronCryptoJWETest\n') \
  -TimeoutSec 60 -RunName repro-elytron-missing-registrations \
  -KeycloakRoot apps/keycloak-fresh \
  -Exe target/release/cratonvm-nonpassed1200-20260706 -JdkHome /data/data/data/jdk25-real
```

## Evidence

`/data/data/data/wt-keycloak-nonpassed-1200-20260706/apps/keycloak-suite-runner/.suite/results/nonpassed1200-20260706-shard1/others-jit/logs/crypto_elytron.org.keycloak.crypto.elytron.test.{ElytronHmacTest,ElytronCryptoJWETest,ElytronKeyStoreTypesTest}.{out,err}.log`, 2026-07-06 4-shard rerun with 1200s timeout.
