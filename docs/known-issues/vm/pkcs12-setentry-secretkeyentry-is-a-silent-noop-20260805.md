# `KeyStore.setEntry` with a `SecretKeyEntry` is a silent no-op on PKCS12

| | |
|---|---|
| **Status** | OPEN — reproduced and bounded, root cause not located |
| **Severity** | medium — silent data loss; the store succeeds and writes a valid, empty keystore |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` are identical; not a strict-mode defect |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `security` section (L8, criterion 6) |

## What happens

```java
KeyStore ks = KeyStore.getInstance("PKCS12");
ks.load(null, null);
char[] pw = "changeit".toCharArray();
ks.setEntry("secret",
        new KeyStore.SecretKeyEntry(new SecretKeySpec(new byte[16], "AES")),
        new KeyStore.PasswordProtection(pw));
System.out.println(Collections.list(ks.aliases()) + " isKey=" + ks.isKeyEntry("secret"));
```

| | HotSpot 25 | CratonVM |
|---|---|---|
| aliases immediately after `setEntry` | `[secret] isKey=true` | **`[] isKey=false`** |
| `store(…)` output size | 405 bytes | **32 bytes** |
| aliases after `load` of that output | `[secret] size=1 isKey=true` | `[] size=0 isKey=false` |
| `getKey("secret", pw)` | `AES/16` | `null` |

The entry is already gone **before** anything is serialized, so this is
`engineSetEntry`, not the PKCS12 writer. Nothing throws:
`KeyStoreException` would be the correct answer if secret keys were
unsupported, and 32 bytes is a structurally valid empty PKCS12 — so a caller
that writes a keystore and checks for an exception is told it worked.

## Why the probe noticed and a unit test would not

`JdkOnlyPlatformProbe` prints the round-trip result as a value
(`p12=false`) rather than as an absence of exceptions. This is the
"print values, not `ok`" rule from
[L8](../../internal/jdk-only-wave2-L8-strict-corpus-green-RETIRED-20260805.md)
earning its place: every step here *succeeded*.

## Where to start

`setEntry` is not registered as a native anywhere in the tree — `grep -rn
'setEntry' --include=*.rs` returns nothing — so the real `KeyStore.setEntry`
bytecode runs and delegates to `keyStoreSpi.engineSetEntry`. The question is
which SPI object the CratonVM `KeyStore.getInstance("PKCS12")` path installs:
`native-builtins/src/phases_early.rs:16026` and `native-builtins/src/tls.rs:1723`
both register `java/security/KeyStore` families and both allocate a
`alloc_concurrent_synthetic("java/security/KeyStore", …)`, so the receiver may
never reach the real `sun.security.pkcs12.PKCS12KeyStore` at all.

The discriminating measurement is one line: print
`ks.getClass().getName()` and the SPI's class next to HotSpot's. If the SPI is
CratonVM's, the gap is `engineSetEntry`'s missing `SecretKeyEntry` arm
(`native-builtins/src/keystore.rs:792` already knows PKCS12 stores those as a
SecretBag, so the writer side may be present and only the setter missing). If
the SPI is the real one, the gap is in whatever `Key.getEncoded()` /
`SecretKeySpec` surface it calls.

Certificate and private-key entries were not measured; this record covers
`SecretKeyEntry` only. `KeyStore.getInstance("JKS")` reading the image's own
`cacerts` works and is byte-identical to HotSpot (144 entries, matching alias
digest), so the read path for certificates is sound.
