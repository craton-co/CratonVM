# TestLargeClientHello — PrivateKey field-layout out-of-bounds fixed

**Status:** FIXED and archived on 2026-07-15.
**Related:** [largeclienthello-string-size-nosuchmethod-FIXED.md](largeclienthello-string-size-nosuchmethod-FIXED.md)
and [largeclienthello-session-resumption-handshake-abort-FIXED.md](largeclienthello-session-resumption-handshake-abort-FIXED.md).

## Root cause

During `org.apache.tomcat.util.net.TestLargeClientHello.testLargeClientHelloWithSessionResumption`,
Tomcat stages the TLS private key in `KeyStore.setKeyEntry`. The runtime's
`java/security/PublicKey.getEncoded()[B` native is also selected for the
interface dispatch on the compact four-slot `java/security/PrivateKey` proxy
emitted by `keystore::engine_get_key`.

That proxy stores `(store_id, alias_hash)` in slot 3 and has no in-object DER
slot. `key_get_encoded` incorrectly assumed the five-slot KeyFactory layout
and unconditionally read slot 4, producing:

```
gen_heap::get_field: out-of-bounds field read dropped
class_name=java/security/PrivateKey index=4 num_slots=4
```

The guard correctly prevented memory corruption, but the DER read was invalid
and left the TLS setup dependent on fallback behavior.

## Fix

`key_get_encoded` now checks the receiver layout. Five-slot KeyFactory key
proxies still return their in-object DER; compact four-slot keystore proxies
resolve their key material through the keystore registry using the slot-3
handle. A focused unit regression covers this compact proxy path.

## Validation

1. `cargo test -p cratonvm-native-builtins compact_keystore_private_key_get_encoded_uses_registry_der --lib -- --nocapture`
   passed (1 test).
2. Built a fresh release VM in the unique target directory
   `C:\craton\CratonVM-target-largeclienthello-privatekey-closure-20260715-001`.
3. Ran `org.junit.runner.JUnitCore org.apache.tomcat.util.net.TestLargeClientHello`
   against the real JDK with `CRATONVM_REAL_NET_SOCKETS=1` and
   `CRATONVM_REAL_AQS=1`.

The acceptance run completed `OK (1 test)` in 30.6 seconds and emitted no
`out-of-bounds field read` warning.
