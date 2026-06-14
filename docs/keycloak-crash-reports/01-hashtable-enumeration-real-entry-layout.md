# 01 — `Hashtable.keys()` / `elements()` misread real `Hashtable$Entry` layout

**Status:** FIXED (worktree `CratonVM-kcsuite`)
**Affected keycloak classes (4):** DefaultCertificateIdentityExtractorTest,
DefaultCryptoJWKTest, DefaultCryptoRSAVerifierTest, PemUtilsBCTest
**Surface symptom:** `java.lang.IllegalArgumentException: Unknown object id - CN - passed to distinguished name`
wrapped as `RuntimeException: Error creating X509v1Certificate.`

## Repro (minimal, no keycloak)
```java
Hashtable<String,Integer> h = new Hashtable<>();
h.put("cn", 1); h.put("o", 2); ... // 15 entries
Enumeration<String> en = h.keys();
while (en.hasMoreElements()) { String k = en.nextElement(); }  // CratonVM: internal error
```
CratonVM: `Error in thread "main" internal error: checkcast: not an object reference`.
HotSpot: enumerates 15 keys cleanly.

Via BouncyCastle:
```java
new org.bouncycastle.asn1.x500.X500Name("CN=Test");   // throws "Unknown object id - CN"
org.bouncycastle.asn1.x500.style.BCStyle.INSTANCE.attrNameToOID("CN"); // same
```

## Root cause
`new X500Name("CN=…")` parses the DN with `BCStyle.INSTANCE`. `BCStyle`'s constructor
builds its **instance** `defaultLookUp` table by `copyHashTable(DefaultLookUp)`, which
enumerates `DefaultLookUp.keys()`. CratonVM intercepts `Hashtable.keys()`/`elements()`
with a native (`deprecated_util.rs::collect_hashtable`,
`deprecated_io_util.rs::register_hashtable_enumerations::snapshot`). That native walked
the bucket nodes assuming **CratonVM's native HashMap node layout** — `key=slot0,
value=1, hash=2, next=3`.

In **real-JDK mode** (the keycloak harness runs `--java-home <real jdk-25>`),
`Hashtable.<init>`/`put` execute **real bytecode**, so the receiver is a genuine
`java.util.Hashtable` whose nodes are real `Hashtable$Entry` with layout
**`hash(0,int) key(1) value(2) next(3)`**. The native therefore read slot 0 (the
primitive `hash`) as the *key* → the enumeration array contained `Value::Int` → the
caller's `checkcast String` failed ("not an object reference"); and BC's `copyHashTable`
produced a corrupt per-instance `defaultLookUp`, so `attrNameToOID("CN")` missed and
threw "Unknown object id - CN".

The static `DefaultLookUp.get("cn")` worked because `get`/`put` are real bytecode and
use the real layout — only the **intercepted enumeration** misread it.

## Fix
Make both native copies layout-aware. `next` is slot 3 in *both* layouts; only key/value
slots differ. Discriminate on slot 0: a real `Entry.hash` is a primitive `int`
(`Value::Int`), a native node's key is always an object reference.

```rust
let real_layout = matches!(ctx.get_field(node, 0), Value::Int(_));
let v = if real_layout {
    if want_keys { ctx.get_field(node, 1) } else { ctx.get_field(node, 2) }
} else if want_keys { ctx.get_field(node, 0) } else { ctx.get_field(node, 1) };
```

Files: `native-builtins/src/deprecated_util.rs` (`collect_hashtable`),
`native-builtins/src/deprecated_io_util.rs` (`snapshot`).
