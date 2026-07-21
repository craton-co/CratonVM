# Embedded Tomcat server intermittently fails `KeyStore.setKeyEntry` with "Private key must be accompanied by certificate chain"

Status: FIXED 2026-07-21 on `fix/tomcat-keystore-emptycertchain-20260720`.

## Symptom

`reactive.HttpComponentsClientHttpConnectorBuilderTests` (and possibly other
classes that boot an embedded Tomcat with an SSLBundle-configured HTTPS
connector) intermittently failed at **server startup**, before any client
connection was attempted:

```
org.springframework.boot.web.server.WebServerException: Unable to start embedded Tomcat server
  at org.springframework.boot.tomcat.TomcatWebServer.start(TomcatWebServer.java:248)
Caused by: java.lang.IllegalArgumentException: standardService.connector.startFailed
Caused by: org.apache.catalina.LifecycleException: Protocol handler start failed
Caused by: java.lang.IllegalArgumentException: Error creating SSLContext
  at org.apache.tomcat.util.net.AbstractEndpoint.createSSLContext(AbstractEndpoint.java:439)
Caused by: java.lang.IllegalArgumentException: Private key must be accompanied by certificate chain
  at java.security.KeyStore.setKeyEntry(KeyStore.java:1210)
```

Reproduction (isolated `-ClassList`, `-Parallel 1`, back-to-back runs of the
same binary): **2/25 (8%)** — matches the originally observed ~7% rate. The
failure surfaced specifically in `connectWithSslBundleAndOptionsMismatch`,
consistently **deep into the test-class run** (after ~20 prior embedded
Tomcat start/stop cycles in the same process), never on the first few
sub-tests.

## Root Cause

Tomcat's `SSLUtilBase.getKeyManagers()` has a PKCS#8-key compatibility path
(triggered when the loaded key's `getFormat()` is `"PKCS#8"` and the
keystore type isn't `DKS`) that rebuilds the identity into a fresh in-memory
keystore:

```java
Certificate[] chain = ks.getCertificateChain(alias);   // <- our engineGetCertificateChain
ks2.setKeyEntry(alias, key, password, chain);           // real bytecode; throws if chain is null/empty
```

`ks.getCertificateChain(alias)` runs our `engine_get_certificate_chain`
native (`native-builtins/src/keystore.rs`), which resolves the loaded
`LoadedKeyStore` through a `store_id` keyed by the `KeyStoreSpi` object's
**identity hash** (`store_id_by_identity()`  —  the *only* tier of
`get_store_id`'s three-tier lookup that can resolve a real-JDK
`PKCS12KeyStore`/`JavaKeyStore` SPI object, since its real field layout has
no room for a synthetic `store_id` field).

The identity hash itself was not actually stable. `jit/src/x64.rs`'s inline
`new` bytecode fast path deliberately leaves the header's
`identity_hash_code` field at 0 (TLAB-zeroed) rather than paying a mint on
every allocation, per its own comment: *"the lazy-mint contract in
`System.identityHashCode()` handles it on demand."* That contract was never
actually implemented anywhere. Every reader of the header's stored hash —
including `System.identityHashCode()`'s own native
(`native-builtins/src/lang_system.rs::native_system_identity_hash_code`) —
funneled through `NativeContext::identity_hash_code`
(`vm/src/vm/vm_exec.rs`), whose fallback for a zero-valued header derived an
*ephemeral* value from the object's **current pointer** on every call,
without writing anything back:

```rust
let p = obj.as_ptr() as usize;
let mixed = (p as u32 ^ (p >> 32) as u32) as i32;
```

That value is only "stable" as long as the object never moves. This VM's
young generation is copying/moving (semi-space Cheney-style on all three
heap backends — Generational, G1, ZGC), so any `KeyStoreSpi` object
allocated via the JIT's fast path (overwhelmingly likely once the relevant
`new`/factory bytecode has tiered up — hence the "only after ~20 prior
Tomcat cycles" empirical pattern) would report a *different* "identity hash"
before and after the next minor GC.

Sequence that produced the bug:
1. `engineLoad` runs, calls `set_store_id(ctx, this, id)` →
   `ctx.identity_hash_code(this)` reads 0 from the header → fallback derives
   `H1` from the object's address at that moment → `store_id_by_identity()[H1] = id`.
2. A GC runs between `engineLoad` and the later `getKey()`/`getCertificateChain()`
   calls (Tomcat's PKCS#8 path does a `KeyStore.getInstance()` + a second
   `engineLoad` for the ephemeral `ks2` in between — both allocation-heavy),
   relocating the `KeyStoreSpi` object. Its header's `identity_hash_code`
   field is still 0 (nothing ever wrote a real value into it), and the
   *bytes* of that 0 are preserved verbatim by the move.
3. `engine_get_certificate_chain` runs: `ctx.identity_hash_code(this)` reads
   0 again → fallback derives `H2 ≠ H1` from the object's new address →
   `store_id_by_identity()[H2]` is not found → `get_store_id` returns `0` →
   `keystore_lookup(0)` finds nothing → the native returns `null` for the
   chain → Tomcat's real-bytecode `KeyStore.setKeyEntry` throws.

This is architecturally the same *family* of bug already fixed once in this
codebase for `t27_tls.rs`'s `engine_table`/`sslparams_alpn_table` (see
`reactive-httpcomponents-connector-flaky-tls-engine-identity-and-pool-cipher-leak-FIXED.md`),
but at a lower layer: that fix moved those two tables from hashing the raw
`ObjectRef` pointer to `ctx.identity_hash_code()` — this bug shows that
`ctx.identity_hash_code()` itself was not GC-move-stable for any object
whose header hash had never been eagerly minted, so the same failure mode
could resurface in *any* per-object identity-keyed side table, not just the
two already fixed. (`store_id_by_identity()` was already using
`ctx.identity_hash_code()` correctly — the bug was one layer beneath it.)

## Fix

Made the heap's `identity_hash_code()` itself GC-move-stable by fulfilling
the "lazy-mint" contract the JIT fast path already assumed existed: on first
read of a zero header field, mint a fresh value from the same monotonic
counter the eager allocators use (`next_hash()`) and durably **CAS it into
the header** (`0 -> minted`, so a losing racer's mint is discarded and every
caller converges on one value). Once non-zero, every mover in this codebase
already copies the header's bytes verbatim, so the minted value survives
every future relocation.

Applied to all three heap backends (`gc/src/gen_heap.rs::GenerationalHeap`,
`gc/src/g1.rs::G1Collector`, `gc/src/zgc.rs::ZgcRealHeap`), each gaining a
`mint_identity_hash_code` helper alongside their existing `next_hash()`.
`vm/src/vm/vm_exec.rs`'s `NativeContext::identity_hash_code` was simplified
to trust the heap's value unconditionally (the old per-call ephemeral
pointer-derived fallback is now dead code — the heap never returns 0 for a
live object).

This is a general correctness fix, not a keystore-specific patch: it also
closes the same latent hazard for every other `ctx.identity_hash_code()`
consumer in the codebase (`VarHandle` root dedup, exception-identity dedup
in `vm_util.rs`/`vm_init.rs`/`invokedynamic.rs`, and any future per-object
identity-keyed side table), and restores the documented
`Object.hashCode()`/`System.identityHashCode()` stability contract those
callers already assumed held.

## Verification

Built `cratonvm-keystore-emptycertchain-fixed.exe` in a dedicated worktree
(`fix/tomcat-keystore-emptycertchain-20260720`, branched from `dev`).

Isolated repro (`reactive.HttpComponentsClientHttpConnectorBuilderTests`,
`-Parallel 1`, same class, back-to-back):

| Binary | Runs | "Private key must be accompanied by certificate chain" |
|---|---|---|
| Unfixed (baseline) | 25 | **2 (8%)** |
| Fixed | 30 | **0** |

Regression (fixed binary):
- Full `module/spring-boot-http-client` module (32 classes): 24 PASS, 5 FAIL,
  1 HANG, 2 EMPTY (abstract base classes). Diffed class-by-class against the
  same 32 classes on the unfixed binary — identical failure set except one
  extra `HttpComponentsClientHttpRequestFactoryBuilderTests` FAIL
  (`ConnectionClosedException`), confirmed pre-existing flakiness by running
  that single class 6x on each binary (old: 4 PASS/2 FAIL, fixed: 3 PASS/3
  FAIL — same order of magnitude, not a new failure mode).
- Broad sample of 152 classes across 76 modules (evenly sampled from the
  full ~1975-class suite): 132 PASS, 13 FAIL, 3 HANG, 4 EMPTY. Every
  failure/hang matches a pre-existing, unrelated pattern (exception-message
  text mismatches vs. real JDK, MongoDB/R2DBC connection issues, JUnit
  discovery errors) — no crashes, corruption, or new failure signatures
  attributable to the identity-hash change.

None of the residual failures in either regression pass involve this bug's
signature (`Private key must be accompanied by certificate chain`).

## Related, unrelated residual found during this investigation

`connectWithSslBundleAndOptionsMismatch` (both GET and POST) fails almost
every isolated run with `AssertionError: Expecting code to raise a
throwable` — the deliberately cipher-mismatched TLS handshake succeeds when
the test expects it to fail. This reproduced identically on both the
baseline and fixed binaries (present in essentially every non-hung run of
both), confirming it is a **separate, pre-existing bug**, not a residual of
this fix or a regression from it. Not investigated further here — flagged
as its own follow-up.

## Affected classes

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests` | Was ~7-8% intermittent; 0/30 after fix |
