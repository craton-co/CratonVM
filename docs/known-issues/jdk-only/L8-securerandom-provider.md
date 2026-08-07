# `SecureRandom.getProvider()` was null, and `getInstance` fabricated a PRNG for any string

**Status:** FIXED in source 2026-08-06 (lane L8, JDK-only wave 2). Not yet
verified against a binary — see *How to verify* below.

## The failure

`regression-suite/src/RJdkSecurity.java` fails in **both** `--real-jdk` and
`--jdk-only`; HotSpot 25 runs it to `PASS RJdkSecurity (61 checks)`. Both
CratonVM arms produce the identical trace, which is the tell that this is an
ordinary Compatible-mode defect and not a strict-mode policy drop:

```
CK RJdkSecurity sha256abc=ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
CK RJdkSecurity hmac=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843
Exception in thread "main" java/lang/AssertionError: provider
    at RJdkSecurity.main(RJdkSecurity.java:319)
    at RJdkSecurity.secureRandoms(RJdkSecurity.java:136)
    at RJdkSecurity.check(RJdkSecurity.java:53)
```

Both digest values are the correct published vectors (SHA-256 of `"abc"`, RFC
4231 HMAC case 2), so the JCA plumbing around them is genuinely right. The
failing line is:

```java
// RJdkSecurity.java:135-136
check(sr.getAlgorithm() != null && !sr.getAlgorithm().isEmpty(), "algorithm name");
check(sr.getProvider() != null, "provider");
```

Line 135 **passes** and line 136 fails, on the same receiver. That pair is the
whole diagnosis: `algorithm` is populated and `provider` is not, so this is not
a layout problem, not a by-name-read problem, and not a dispatch problem —
`getProvider()` is plain JDK bytecode reading a field nobody ever wrote.

## Root cause

`java.security.SecureRandom` in JDK 25 declares two instance fields that the
real construction path (`getDefaultPRNG` for the constructors,
`sun.security.jca.GetInstance` for the factories) always stamps together:

```java
private Provider provider;      // returned verbatim by getProvider()
private String   algorithm;     // returned verbatim by getAlgorithm()
```

`native-builtins/src/securerandom.rs` intercepts every construction route
(module header explains why: the real `<init>` reaches
`Providers.getProviderList()`, which we no-op, and NPEs). Both interception
points wrote **only** `algorithm`:

* `secure_random_record_algorithm` (was `securerandom.rs:592-602`) — the shared
  body of `<init>()V` / `<init>([B)V`. Its own doc comment records the wave-3
  stub removal that added `algorithm` and stops there.
* `make_secure_random` (was `securerandom.rs:1282-1292`) — the allocator behind
  `getInstance(String)`, `getInstance(String, String|Provider)` and
  `getInstanceStrong()`. Same shape: `set_field_by_name(sr, "algorithm", …)`
  and nothing else.

So every `SecureRandom` this VM has ever handed out reported `getProvider() ==
null`. Real code hits this the moment it logs or audits its RNG source
(`sr.getProvider().getName()` is an NPE), which is why the regression vector
asserts it.

### Which registration is actually live

Worth pinning, because `SecureRandom` is registered from two files:
`native-builtins/src/crypto_impl.rs:1336` (`register_crypto_impl_natives`) also
claims `nextBytes`, `generateSeed`, `setSeed(J)V`, `setSeed([B)V` and
`<init>([B)V`, with a comment asserting it wins on registration order.

It wins only in **synthetic-jdk** mode, behind two independent gates:
`register_crypto_impl_natives` is called from `lib.rs:23623`, inside
`register_synthetic_overrides` (`lib.rs:20953`), which
`vm/src/native/builtins.rs:29` replaces with a no-op shim unless the
`synthetic-jdk` *feature* is on — and `vm/src/vm/vm_init.rs:1518-1522` reaches
that whole block only when `config.use_synthetic_jdk` is also true, which
`--real-jdk` and `--jdk-only` both leave false. The failing arms run
`register_essential_natives` (`lib.rs:6709`) only, and its
`crate::securerandom::register_random_and_securerandom_natives(registry)` call
at `lib.rs:18695` is the one that binds. So the fix below is on the live path
for both failing arms. `crypto_impl.rs` is untouched — see *Out of scope*.

## The second failure behind it

Fixing only line 136 would have moved the assert down 29 lines. `secureRandoms`
continues:

```java
// RJdkSecurity.java:159-165
boolean threw = false;
try {
    SecureRandom.getInstance("NO-SUCH-PRNG");
} catch (NoSuchAlgorithmException expected) {
    threw = true;
}
check(threw, "an unknown PRNG must raise NoSuchAlgorithmException");
```

`native_secure_random_get_instance` validated only that the name was non-empty
and then fabricated a working OS-CSPRNG instance for **any** string. Real JDK
resolves the name through the provider service map and raises
`NoSuchAlgorithmException("<algo> SecureRandom not available")`. The
consequence is worse than the failed assert: ordinary probing code shaped
`try { getInstance(x) } catch (NoSuchAlgorithmException e) { fallback }` never
took its fallback, and an outright typo in a config file went undetected.

Note the two-argument overloads were already correct here — they run
`provider_chain::check_provider_ownership`, which consults the service table —
so the one-argument form was the only hole.

The rest of `secureRandoms` was audited and needs nothing: `getSeed(8)` routes
through the real static body (`new SecureRandom()` + `generateSeed`),
`setSeed(12345L)` is a documented no-op for non-SHA1PRNG algorithms and the
following `nextBytes` draws fresh OS entropy so the "must not replay" check
holds, `getInstanceStrong()` returns non-null, and `getInstance("SHA1PRNG")`
already reported `getAlgorithm() == "SHA1PRNG"` (HotSpot's oracle line is `CK
RJdkSecurity prng=SHA1PRNG distinct=true`).

## What changed

All in `native-builtins/src/securerandom.rs`. No registration rows were added,
removed, or re-kinded, so no `NativeKind` question arises and no ratchet moves.

**1. A new `Algorithm names and the owning Provider` section** with four items:

| item | what it does |
| --- | --- |
| `DEFAULT_ALGORITHM` | names the `"OS-CSPRNG"` literal that was duplicated at three sites |
| `secure_random_static_provider(algo) -> Option<&'static str>` | the JDK-25 name → provider table: `DRBG`/`SHA1PRNG`/`NativePRNG{,Blocking,NonBlocking}` → `SUN`, `Windows-PRNG` → `SunMSCAPI`, plus our own `OS-CSPRNG` → `SUN`. Alphanumeric-only + upper-case normalisation, matching `jca::message_digest::algorithm_supported` and the JCA case-insensitivity rule |
| `secure_random_algorithm_supported(algo) -> bool` | static table **or** `provider_chain::find_service_provider("SecureRandom", algo)` |
| `secure_random_attach_provider(ctx, this, algo) -> ObjectRef` | writes the `provider` field; returns the forwarded receiver |

**2. `secure_random_record_algorithm`** now calls `secure_random_attach_provider`
after stamping `algorithm`, so both constructors populate both fields.

**3. `make_secure_random`** returns `secure_random_attach_provider(ctx, sr,
algorithm)` instead of `sr`.

**4. `native_secure_random_get_instance`** raises
`provider_chain::throw_no_such_algorithm_public(ctx, "<algo> SecureRandom not
available")` when `secure_random_algorithm_supported` says no. That helper
builds a genuine `java/security/NoSuchAlgorithmException` via
`new_object_initialized`, so it is caught by name — the same call
`jca::message_digest.rs:165` already makes for unknown digests. A
`RuntimeError::SecurityException` would have been unchecked and sailed straight
past the test's `catch`.

### Two things the implementation is deliberate about

**The provider is resolved, not fabricated.**
`secure_random_attach_provider` goes through
`provider_chain::resolve_or_make_provider`, which returns the caller's **real**
registered `Provider` object when one is on file and only falls back to a
synthetic. And `secure_random_provider_name` asks
`find_service_provider("SecureRandom", algo)` *before* the static table, so a
user provider that registered `SecureRandom.<algo>` via `Security.addProvider`
owns its own algorithm — the JDK's search order, not a hardcode. The static
table is the fallback, not the authority; that ordering matters because
`provider_chain` already seeds `SUN → {DRBG, SHA1PRNG}` and `SunMSCAPI →
Windows-PRNG` at `provider_chain.rs:1095-1147`.

**The receiver is re-read after the allocation.**
Materialising a `Provider` allocates several objects, so `this` can be
GC-forwarded across the call. `secure_random_attach_provider` pins,
allocates, `read_native_pin`s, writes, unpins — and *returns* the forwarded
reference, because `make_secure_random` hands its result straight back to Java.
Dropping the return value there would have been exactly the stale-oop shape the
module header already warns about for `create_string`.

**Why the static table is consulted first in
`secure_random_algorithm_supported` but second in `secure_random_provider_name`.**
Support is a yes/no that must not depend on whether
`jca::provider_chain::register` has run yet (it seeds the service table from
inside the registration pass); ownership is a name that should reflect whatever
the live table says. Different questions, different orders.

## Scope of the behaviour change

`getInstance` now refuses names it used to accept. The accepted set is every
`SecureRandom` algorithm a stock JDK 25 carries on Windows *or* Unix, plus
anything a registered provider claims, plus the `OS-CSPRNG` name this module
itself stamps (so a caller round-tripping `getAlgorithm()` back through
`getInstance` is not refused). The only in-tree callers are
`regression-suite/src/RJdkSecurity.java:150`,
`probes/JdkOnlyPlatformProbe.java:231` and
and the internal repro
`fixed-suite-bugs/repros/jca-provider-lookup-parity/ProviderLookupProbe.java`,
all of which ask for `SHA1PRNG`. H2's `TestAll` — the app that motivated the
`getInstance` interception in the first place
(internal record `fixed-suite-bugs/app-jvm-bugs/bug-h2-securerandom-sha1prng.md`)
— also asks for `SHA1PRNG`.

## Out of scope (files this lane does not own)

`native-builtins/src/crypto_impl.rs:1377` registers `SecureRandom.<init>([B)V`
to `native_secure_random_init_seed_bytes`, a pure no-op that records neither
`algorithm` nor `provider`. In synthetic-jdk mode that registration wins over
`securerandom.rs`'s, so `new SecureRandom(byte[])` still answers null from both
accessors there. `RJdkSecurity` does not use the seeded constructor, so this is
not part of the present failure; the clean fix is to delete that one
registration and let `securerandom.rs`'s `native_secure_random_init` (which is
semantically identical — both discard the seed — but stamps both fields) serve
it in every mode.

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins securerandom::tests

javac -d regression-suite/build regression-suite/src/RJdkSecurity.java
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkSecurity
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkSecurity
java -cp regression-suite/build RJdkSecurity      # HotSpot 25 oracle
```

All three must reach `PASS RJdkSecurity (61 checks)`. The two CK lines this
lane unblocks are:

```
CK RJdkSecurity prng=SHA1PRNG distinct=true
CK RJdkSecurity providerSun=true
```

`prng=SHA1PRNG` proves the `getInstance` name check did not over-reject;
`providerSun=true` comes from `providers()` at line 300 and is downstream of
sections this lane did not touch, so a failure there is a different lane's.

## Baselines: none need re-freezing

No `register*` call was added or changed, so
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`,
`scripts/baselines/jdk-only-bridge-ratchet.json` and
`native-builtins/tests/stub_ratchet.rs` all score identically. No file under
`scripts/baselines/` or `native-builtins/tests/` mentions `securerandom`, so
the ~110 inserted lines cannot stale a fixed line band.
