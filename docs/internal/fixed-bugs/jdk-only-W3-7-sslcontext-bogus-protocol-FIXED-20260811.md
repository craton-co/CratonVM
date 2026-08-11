> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkSecurity` passes in the 53/1 run. The out-of-file patches ARE applied: both `native-builtins/src/net_phase_e.rs:11992` and `native-builtins/src/phases_late/ssl_security.rs:1518` now raise through `jca::provider_chain::throw_no_such_algorithm_public` with HotSpot's `"<name> SSLContext not available"` wording. "The next failure behind it, and its patch" — `Security.getAlgorithms` writing a `String[]` into slot 0 of a real `java.util.HashSet` — was taken by W4-3 and fixed; `phases_early.rs` now builds that set with `make_hashset_with_elements`.
>
> Previous location: `docs/known-issues/jdk-only/W3-7-sslcontext-bogus-protocol.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# `SSLContext.getInstance("<bogus>")` threw an `IOException` wrapping the words "NoSuchAlgorithmException"

**Status:** FIXED in source 2026-08-07 (lane W3-7, JDK-only wave 3). Not yet
verified against a binary — see *How to verify* below.

## The failure

`regression-suite/src/RJdkSecurity.java` fails in **both** `--real-jdk` and
`--jdk-only` with byte-identical traces (the tell that this is an ordinary
Compatible-mode defect, not a strict-mode policy drop). HotSpot 25 runs the
class to `PASS RJdkSecurity (61 checks)`.

```
CK RJdkSecurity gcm=0388dace…bddf pbkdf2=120fb6cffcf8b32c verify=true
Exception in thread "main" java/io/IOException: NoSuchAlgorithmException: NO-SUCH-TLS
    at RJdkSecurity.main(RJdkSecurity.java:321)
    at RJdkSecurity.tls(RJdkSecurity.java:291)
```

Everything in `tls()` up to line 291 passes — the default context, the `"TLS"`
context, the engine, the supported/enabled protocol and cipher-suite lists,
`SSLParameters` round-trip, the pre-handshake session. Only the negative probe
fails:

```java
// RJdkSecurity.java:289-295
boolean threw = false;
try {
    SSLContext.getInstance("NO-SUCH-TLS");
} catch (NoSuchAlgorithmException expected) {
    threw = true;
}
check(threw, "an unknown SSL protocol must raise NoSuchAlgorithmException");
```

## Root cause 1 — the right words on the wrong class

`native_builtins/src/net_phase_e.rs`, `register_re6_ssl_context`:

```rust
return Err(ioex(format!("NoSuchAlgorithmException: {proto}")));
```

`ioex` builds a `cratonvm_types::error::RuntimeError::IOException`, i.e. a
`java.io.IOException` whose *message text* happens to contain the string
`"NoSuchAlgorithmException"`. The exception the test catches is
`java.security.NoSuchAlgorithmException`:

```
java.lang.Exception
 ├─ java.io.IOException                                   <- what we threw
 └─ java.security.GeneralSecurityException
     └─ java.security.NoSuchAlgorithmException            <- what is caught
```

The two hierarchies are disjoint below `Exception`, so `catch
(NoSuchAlgorithmException expected)` did not match, the throw propagated out of
`tls()`, and the harness printed it from `main`. This is the same defect shape
as the earlier `Class.forName` lane's `NoClassDefFoundError` wrapping a
`ClassNotFoundException`: a refusal that *is* a refusal, carrying the right
diagnosis, thrown as a type nobody catches.

The sibling registration `phases_late::ssl_security::register_p68_ssl` had the
same defect in a different costume — `IllegalArgumentException("No such
algorithm: …")`, which is *unchecked*, so it would have sailed past the catch
even more quietly. It is not the live registration today (`register_re6_…` runs
later in `register_essential_natives` and wins last-registered-wins), but it is
one registration-order change away from being it.

## Root cause 2 — three hand-rolled accept lists, all too narrow

Four registrations answered "is this protocol supported?", each with its own
list, none matching the platform:

| registration | accepted | file |
| --- | --- | --- |
| `net_phase_e::register_re6_ssl_context` (LIVE, real-JDK) | TLS, TLSv1.2, TLSv1.3, Default, SSL | `net_phase_e.rs` |
| `phases_late::ssl_security::register_p68_ssl` (real-JDK) | + TLSv1, TLSv1.1 — but **case-sensitive**, so `getInstance("tls")` was refused | `phases_late/ssl_security.rs` |
| `tls.rs::register_ssl_context` (synthetic-JDK) | + SSLv3 | `tls.rs` |
| `tls.rs::register_ssl_context_impl` (`sun.security.ssl.SSLContextImpl`) | `_ => 0` — **fabricated a "TLS" context for any string** | `tls.rs` |

Measured on the platform JDK (`jdk-25.0.3.9-hotspot`) by enumerating
`Security.getProvider("SunJSSE").getServices()`:

```
primaries : TLS, TLSv1, TLSv1.1, TLSv1.2, TLSv1.3, Default,
            DTLS, DTLSv1.0, DTLSv1.2
aliases   : SSL -> TLS, SSLv3 -> TLSv1
```

So every one of TLSv1, TLSv1.1, SSLv3, DTLS, DTLSv1.0, DTLSv1.2 was refused by
the live registration although the JDK services them. A too-narrow accept list
is the more dangerous half of this defect: the negative probe costs one
assertion, but refusing a valid `getInstance("TLSv1")` takes down every
HTTPS-using suite that pins a legacy protocol.

`jca/provider_chain.rs::seed_sunjsse_services` had the same four-name hole in
the service table itself, so `find_service_provider("SSLContext", "TLSv1")`
also answered "no provider".

## Normalisation: case, and nothing else

Wave 2's `Cipher` fix copied `jca::message_digest::algorithm_supported`, which
normalises by **stripping non-alphanumerics then upper-casing** — right for
digests, because the JDK's own tables carry `SHA256`/`SHA-256` alias pairs.
`SSLContext` carries no such aliases. Probed on the platform JDK:

| input | HotSpot |
| --- | --- |
| `TLSV1.2`, `tlsv1.3`, `SSLV3`, `dtls` | OK (case folded) |
| `TLSv12` | `NoSuchAlgorithmException` |
| `TLS `, ` TLS`, `T-L-S` | `NoSuchAlgorithmException` |

An alphanumeric-strip normalise would therefore have fabricated
`getInstance("TLSv12")` into a working context — the exact defect species this
predicate exists to close — and would have flipped the existing MUST-RAISE
assertion on `"TLS "` in `tls.rs`. The shared predicate folds case only.

## What changed

**`native-builtins/src/jca/provider_chain.rs`** (owned by this lane)

1. `seed_sunjsse_services` gains the five missing `SSLContext` services
   (`TLSv1`, `TLSv1.1`, `DTLS`, `DTLSv1.0`, `DTLSv1.2`, with their real SunJSSE
   SPI class names) and the `SSLv3 -> TLSv1` alias. Note the platform aliases
   `SSLv3` to **`TLSv1`**, not to `TLS`.
2. New `pub(crate) fn ssl_context_protocol_supported(protocol: &str) -> bool`,
   the single accept decision, shaped exactly like wave 1's
   `securerandom::secure_random_algorithm_supported`. It answers YES on two
   independent grounds and NO only when both fail:
   * the name is in the measured JDK-25 SunJSSE set (case-folded); **or**
   * a provider in the live chain registered an `SSLContext` service under that
     name — a caller-installed provider (Conscrypt, BC-JSSE, Elytron) may add
     protocols we have never heard of, and refusing those would be the same
     defect one layer up.
3. Four tests: the full positive set in the spellings callers write, the
   HotSpot-measured MUST-RAISE set including the three normalisation traps, the
   caller-registered-provider ground, and the seed table itself.

**`native-builtins/src/tls.rs`** (owned by this lane)

4. `register_ssl_context`'s `getInstance` delegates the accept decision to the
   shared predicate; the match that remains only picks which name
   `getProtocol()` echoes back. Indices 7/8/9 added for the DTLS protocols.
5. `register_ssl_context_impl`'s `getInstance` (the
   `sun.security.ssl.SSLContextImpl` alias) loses its `_ => 0` fabrication: it
   now asks the same question and refuses with the same catchable
   `NoSuchAlgorithmException`, and a null protocol raises `NullPointerException`
   like its sibling rather than silently becoming "TLS".
6. That same registration's `getProtocol` now shares `ctx_protocol_name`; its
   private copy knew only indices 1/2/3, so a context created as TLSv1 /
   TLSv1.1 / SSLv3 reported itself as "TLS".

**Out-of-file patches** (files this lane does not own) — see the lane report:
`net_phase_e.rs` and `phases_late/ssl_security.rs` both switch to the shared
predicate plus `provider_chain::throw_no_such_algorithm_public`, with HotSpot's
message wording `"<name> SSLContext not available"`.

## The next failure behind it, and its patch

Fixing only line 291 moves the assert to **line 311**, in `providers()`:

```java
check(Security.getAlgorithms("MessageDigest").contains("SHA-256")
        || …, "MessageDigest algorithms must include SHA-256");
```

`phases_early.rs`'s `Security.getAlgorithms` builds its return value as

```rust
let set = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 1);
…
ctx.set_field(set, 0, Value::Object(Some(arr)));   // a String[]
```

That is the **synthetic** HashSet layout. In real-JDK mode `java.util.HashSet`
has exactly one instance field — `transient HashMap<E,Object> map` — so slot 0
*is* `map`, and this writes a `String[]` into it. The set answers every question
until someone calls a HashSet method: `contains(Object)` is real JDK bytecode
that does `map.containsKey(o)`, i.e. an invokevirtual of `HashMap` on a
`String[]` receiver. The tree already carries two helpers built for exactly this
(`crate::build_real_layout_string_hashset`,
`cratonvm_native_collections::make_hashset_with_elements`), and
`jca/provider_chain.rs::provider_get_services_native` already uses the latter.
The patch is in the lane report.

## Scope of the behaviour change

`SSLContext.getInstance` now **accepts** six names it used to refuse (TLSv1,
TLSv1.1, SSLv3, DTLS, DTLSv1.0, DTLSv1.2) and, on the two synthetic-JDK
registrations, **refuses** names it used to fabricate. The accepted set is
exactly what a stock JDK 25 services, plus anything a registered provider
claims. No protocol a real deployment can ask for is newly refused — which is
the property that matters, because the alternative failure mode is a dead HTTPS
stack rather than a missed assertion.

This is a *resolution* decision, not a *policy* decision. `SSLv3` and `TLSv1`
resolve here exactly as they do on HotSpot; what keeps them off the wire is the
connector, whose `new13_build_connector` pins a TLS 1.2 floor.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins jca::provider_chain::tests
cargo test -p cratonvm-native-builtins tls::tests

javac -d regression-suite/build regression-suite/src/RJdkSecurity.java
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkSecurity
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkSecurity
java -cp regression-suite/build RJdkSecurity      # HotSpot 25 oracle
```

All three must reach `PASS RJdkSecurity (61 checks)`. The two CK lines this lane
unblocks are:

```
CK RJdkSecurity tls=TLSv1.3 engine=client
CK RJdkSecurity providerSun=true
```

`tls=TLSv1.3` (not `TLSv1.2`) is a real assertion: every in-tree
`SSLEngine.getSupportedProtocols` lists `TLSv1.3` first, and the test picks
`modern` from that array.

## Baselines

No `register*` row was added, removed, or re-kinded — every change is inside an
existing closure body, plus one new non-registering `pub(crate) fn` and five
`put_service`/`put_alias` seed calls (side-table data, not native registrations).
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
`scripts/baselines/jdk-only-bridge-ratchet.json` and
`native-builtins/tests/stub_ratchet.rs` all score identically.
