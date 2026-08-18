# `SecureRandom.getProvider()` was null, and `getInstance` fabricated a PRNG for any string

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** `getProvider()` answering null, and
  `getInstance` fabricating a PRNG for any string, were fixed in source
  2026-08-06 by commit `8429fcbbc` —
  `native-builtins/src/securerandom.rs:610` (`secure_random_record_algorithm`
  now attaches the provider), `:629` `secure_random_static_provider`, `:668`
  `secure_random_algorithm_supported`, `:693` `secure_random_attach_provider`,
  `:1399` (`make_secure_random` returns the attached receiver), `:1427` (unknown
  names refused). The verification this record said it was missing was taken on
  2026-08-12 against the dev binary at `ba65f1a19`: `RJdkSecurity` runs to
  `PASS RJdkSecurity (61 checks)` in **both** `--jdk-only` and `--real-jdk`.
  That is this record's own falsifier, and it is green.
* **Residual: APPLIED 2026-08-12 (lane W7-92-L8, this wave). Nothing was
  rebuilt.** The three shadowing registrations in
  `native-builtins/src/crypto_impl.rs::register_crypto_impl_natives` —
  `setSeed(J)V`, `setSeed([B)V`, `<init>([B)V` — are deleted, together with
  their three now-dead no-op bodies `native_secure_random_set_seed_long`,
  `native_secure_random_set_seed_bytes` and
  `native_secure_random_init_seed_bytes`. `nextBytes([B)V` and
  `generateSeed(I)[B` stay, as this record prescribed: both draw from the OS
  CSPRNG in either file, so the shadowing is behaviour-neutral and the
  `VULN(secrand)` / `VULN(secrand-collision)` block comment above them stays
  attached to live registrations. The stale registration-order comment is
  replaced with the text at the foot of this record.

  **The scope claim was re-derived before acting, not inherited.** README §2.1
  called this "synthetic-jdk only"; the registrars agree.
  `register_crypto_impl_natives` has **exactly one call site tree-wide** —
  `native-builtins/src/lib.rs:24167` — and it is inside
  `pub fn register_synthetic_overrides` (`lib.rs:21449`, attribute
  `#[cfg(feature = "synthetic-jdk")]` at `:21448`). Within that function
  `register_security_natives` (`:23877` → `securerandom::
  register_random_and_securerandom_natives` at `:36051`) is called **before**
  `register_crypto_impl_natives`, which is what made the deleted bodies win
  there — a same-function call ordering, not the phase ordering the old comment
  claimed. `securerandom.rs` registers all three deleted triples
  (`<init>([B)V` at `:1580`, `setSeed(J)V` at `:1581`, `setSeed([B)V` at
  `:1590`), so the surviving surface is a strict superset and nothing is left
  unserved in any mode.

  Line citations that had rotted are re-anchored above. The previous pass read
  `register_synthetic_overrides` at `lib.rs:21151` and its call site at
  `lib.rs:23845`; both moved again.
* **The residual's stated REASON was wrong, and the 2026-08-11 pass already
  corrected it — do not re-derive it a third time.** The defect is *not* the
  discarded constructor seed; that discard matches HotSpot and is deliberate
  (see *Verdict 3* below). It is that the two `setSeed` no-ops undo SHA1PRNG
  **reseeding**, which HotSpot does make reproducible and which is the one
  replay guarantee the JDK gives a `SecureRandom`. Anyone who reads only the
  older *Out of scope* framing will delete the wrong row.
* **Line citations in this record have rotted by thousands of lines.** The
  registrations are at `crypto_impl.rs:1408-1425`, not `:1336`/`:1377`;
  `register_crypto_impl_natives` is called from `lib.rs:23845`, not `:23623`;
  `register_synthetic_overrides` is at `lib.rs:21151`, not `:20953`. The patch
  *text* still applies verbatim; only the anchors moved.

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

> **SUPERSEDED FRAMING — DO NOT APPLY AS WRITTEN. Reconciled 2026-08-12.** The
> section below names the discarded constructor seed as the defect. It is not:
> `new SecureRandom(seed)` selects DRBG on HotSpot, and DRBG's `engineSetSeed`
> *reseeds* rather than replaces, so the discard **matches** the oracle — the
> measurement is under *Verdict 2* further down this record. Acting on the
> paragraph below alone deletes the wrong row and leaves the real defect in
> place. The live prescription is the *Out-of-file patch* at the foot of this
> record: delete the two `setSeed` no-ops, whose bodies undo `securerandom.rs`'s
> SHA1PRNG **reseeding** — the one replay guarantee the JDK gives a
> `SecureRandom` — together with the constructor row. Index entry:
> W7-55-record-reconciliation.md §2.4. Kept below unedited because its
> registration facts are still accurate; only its verdict is not.

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

---

# Residual pass, 2026-08-11 — the seed, and which registration is absent

The campaign-wide audit kept this record for one residual: *"the synthetic-jdk
`SecureRandom([B)V` no-op is still registered"*, on the reasoning that *"a
seeded `SecureRandom` that ignores its seed is a fabricated success of the most
dangerous kind — the caller believes it has determinism (or entropy) it does not
have."*

The registration is live. **The reasoning is not, and it points at the wrong
half of the problem.**

## Verdict 1 — the registration is live, and it is synthetic-jdk ONLY

`native-builtins/src/crypto_impl.rs` still carries
`native_secure_random_init_seed_bytes` (two argument-shape checks and
`Ok(None)`) and still registers it for `java/security/SecureRandom.<init>([B)V`
inside `register_crypto_impl_natives`. Confirmed by reading the tree, not
recalled — this record's *Out of scope* section predicted it and the prediction
holds.

Which mode it bites in matters, and the distinction is the one
`docs/architecture/natives-over-real-jdk-classes.md` §2 exists to make:

* `register_crypto_impl_natives` has exactly one call site in
  `native-builtins/src/lib.rs`, and it is inside `register_synthetic_overrides`.
* `register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`, and
  `vm/src/vm/vm_init.rs` reaches it only when `config.use_synthetic_jdk` is also
  true.
* `synthetic-jdk` is in **no crate's default feature set**.

So in a plain CLI build the registrar **never compiles in at all**. This is not
a registration that is present and declined by policy — it is *absent from the
binary*, which is a different thing and a weaker claim than "still registered"
suggests. In `--real-jdk` and `--jdk-only`, `<init>([B)V` is served by
`securerandom.rs`'s `native_secure_random_init`, which stamps both `algorithm`
and `provider`.

Registration order within the synthetic arm, since it is last-write-wins:
`register_synthetic_overrides` calls `register_security_natives` (which calls
`securerandom::register_random_and_securerandom_natives`) **before** it calls
`register_crypto_impl_natives`. The `crypto_impl` bodies therefore win in
synthetic mode, as its own comment claims — though the comment's stated reason
(a phase ordering in `lib.rs`) is not the mechanism; both calls are in the same
function and it is their order *within* it that decides.

## Verdict 2 — "ignores its seed" is what HotSpot does, and the record already said so

Measured this lane on Eclipse Adoptium jdk-25.0.3.9-hotspot
(`java.version=25.0.3`), drawing 16 bytes from two identically seeded
instances:

```
ctor(seed) alg=DRBG prov=SUN
  d1=ad1b218bf7da9b2ce93f0dbfb20efbb2
  d2=5d8d4d0bd95ab80a64e952d612b64831
  ctor(seed) reproducible? false
SHA1PRNG+setSeed reproducible? true v=484cd4718bc11f0673790ab384813637
ctor() alg=DRBG prov=SUN
getInstanceStrong alg=Windows-PRNG
```

`new SecureRandom(seed)` selects DRBG, and DRBG's `engineSetSeed` *reseeds* —
it does not replace the state. **The caller cannot have determinism from
HotSpot either**, so discarding the constructor seed is not a fabricated
success; it is the measured behaviour. Nor is entropy lost: every draw in this
module reads the OS CSPRNG directly, and supplementing a fully seeded CSPRNG is
a no-op.

`securerandom.rs`'s registration already states exactly this, in place, and the
measurement above is an independent confirmation of it rather than a discovery:

> `SecureRandom(byte[] seed)`: the seed argument is DISCARDED, and that matches
> HotSpot rather than merely being convenient. […] So `new SecureRandom(seed)`
> is not reproducible on HotSpot either […]

So on the live paths there is **no silent discard to fix**: the discard is
documented, oracle-matched, and the constructor is not a no-op — it stamps
`algorithm` and `provider`. The instruction "make the seed actually seed, or
make the constructor refuse" has a third correct answer here, which is the one
already in the tree: match the oracle and say so.

## Verdict 3 — the real defect in the shadowing block, and it is not the one that was named

What is wrong with `crypto_impl`'s `<init>([B)V` is not the discarded seed. It
is that the body stamps **neither `algorithm` nor `provider`** — so under
`--synthetic-jdk`, `new SecureRandom(byte[])` still answers `null` from
`getAlgorithm()` *and* `getProvider()`. That is this record's own headline
defect, surviving in one mode because a later registrar overwrote the fix.

And the same block shadows more than the constructor. `register_crypto_impl_natives`
registers five triples, all of which `securerandom.rs` has already registered:

| triple | `crypto_impl` body | what it shadows |
|---|---|---|
| `nextBytes([B)V` | OS CSPRNG | equivalent |
| `generateSeed(I)[B` | OS CSPRNG | equivalent |
| `<init>([B)V` | no-op | loses `algorithm` + `provider` (verdict 3) |
| `setSeed(J)V` | no-op | **loses SHA1PRNG reseeding** |
| `setSeed([B)V` | no-op | **loses SHA1PRNG reseeding** |

Both `setSeed` shadows are worse than the constructor one, and they are the
place the audit's "determinism the caller does not have" sentence is actually
true. `securerandom.rs`'s bodies check `secure_random_is_sha1prng(ctx, this)`
and route SHA1PRNG through real reseeding — that is the wave-4 STUB-REMOVAL
whose own comment records the symptom (*"`getInstance("SHA1PRNG")` seeded twice
alike yields identical bytes on HotSpot and yielded different bytes here"*).
The `crypto_impl` no-ops undo it wholesale in synthetic mode. The measurement
above pins the property they break: **`SHA1PRNG` + `setSeed` IS reproducible on
HotSpot**, and it is the one place the JDK guarantees replay for a
`SecureRandom`. This record's own *Rest of `secureRandoms`* paragraph and the
H2 `TestAll` motivation (internal record
`fixed-suite-bugs/app-jvm-bugs/bug-h2-securerandom-sha1prng.md`) both hang off
`SHA1PRNG`.

## One divergence deliberately left alone

HotSpot reports `new SecureRandom().getAlgorithm()` as `DRBG` (provider `SUN`),
and `getInstanceStrong()` as `Windows-PRNG` on this host. This module stamps
`OS-CSPRNG` for both. That is a divergence, and it is the honest one: the name
describes what the module actually does — read fresh OS entropy on every draw —
whereas stamping `DRBG` would assert a deterministic-random-bit-generator
construction that is not there. `RJdkSecurity` asserts only that the name is
non-null and non-empty. Recorded so that nobody "fixes" `getAlgorithm()` to say
`DRBG` while `nextBytes` still goes straight to the OS.

## Out-of-file patch — APPLIED 2026-08-12

> **This section is kept as the record of what was deleted and why. It is no
> longer a prescription.** The lane that opened this record did not own
> `native-builtins/src/crypto_impl.rs`; the 2026-08-12 lane did, and applied it
> verbatim. Nothing was rebuilt.

The fix is a deletion, and it is the one this record's *Out of scope* section
already prescribed — widened to the two `setSeed` rows, which it did not cover.

In `register_crypto_impl_natives`, these three registrations were deleted:

```rust
    r.register(
        "java/security/SecureRandom",
        "setSeed",
        "(J)V",
        native_secure_random_set_seed_long,
    );
    r.register(
        "java/security/SecureRandom",
        "setSeed",
        "([B)V",
        native_secure_random_set_seed_bytes,
    );
    r.register(
        "java/security/SecureRandom",
        "<init>",
        "([B)V",
        native_secure_random_init_seed_bytes,
    );
```

and, once nothing references them, the three now-dead bodies
`native_secure_random_set_seed_long`, `native_secure_random_set_seed_bytes` and
`native_secure_random_init_seed_bytes`. All six deletions landed together;
`dead_code` is `allow` workspace-wide, so leaving the bodies would have compiled
and left the trap in place, which is the failure mode this record's Result 1
argues against.

`nextBytes([B)V` and `generateSeed(I)[B` should **stay**. Both draw from the OS
CSPRNG in either file, so the shadowing is behaviour-neutral, and the block
comment above them is the live record of the `VULN(secrand)` /
`VULN(secrand-collision)` fixes — deleting the registrations would orphan it.

The registration-order comment on that block must be corrected at the same
time. It reads:

> IMPORTANT (registration order): `register_crypto_impl_natives` runs AFTER
> `securerandom::register_random_and_securerandom_natives` (lib.rs phase
> ordering: register_security_natives ~line 9773 vs register_crypto_impl
> ~line 10010), so these last-write registrations WIN.

Two things are wrong with it. The line numbers are stale by thousands of lines
(both calls now sit inside `register_synthetic_overrides`, in that order). More
importantly it presents the win as a phase-ordering fact when it is a
*same-function* call ordering, and it does not say that the whole block is
unreachable outside `--synthetic-jdk` — which is the first thing a reader needs
in order to scope any defect found here.

Replace with:

```rust
    // Registration order: both this registrar and
    // `securerandom::register_random_and_securerandom_natives` are called from
    // `register_synthetic_overrides`, this one SECOND, so these bodies win —
    // `register()` is last-registration-wins. That scope is the whole story:
    // `register_synthetic_overrides` is `#[cfg(feature = "synthetic-jdk")]`,
    // the feature is in no crate's default set, and `vm_init` reaches it only
    // when `config.use_synthetic_jdk` is also true. So none of this exists in a
    // default CLI build, and `--real-jdk` / `--jdk-only` are served by
    // `securerandom.rs` — see docs/architecture/natives-over-real-jdk-classes.md §2.
    // Only `nextBytes` / `generateSeed` are registered here: both draw from the
    // OS CSPRNG in either file, so the shadowing is behaviour-neutral. The
    // `setSeed` and seeded-ctor rows were REMOVED (L8 residual pass, 2026-08-11)
    // because their no-op bodies silently undid two fixes in `securerandom.rs`:
    // SHA1PRNG reseeding, which HotSpot makes reproducible and which is the one
    // replay guarantee the JDK gives a `SecureRandom`; and the `algorithm` /
    // `provider` stamping this record exists for.
```

**Mode: synthetic-jdk only, in both directions.** Deleting these rows cannot
change Compatible-mode or `--jdk-only` behaviour by any amount, because the
registrar is not reachable in either. For the same reason it cannot move
`scripts/baselines/jdk-only-bridge-ratchet.json`, which is taken in Compatible
mode — see `docs/architecture/natives-over-real-jdk-classes.md` §7.

---

## Adjudicated in `--synthetic-jdk` — 2026-08-12 (lane A31)

"Verdict 1 — the registration is live, and it is synthetic-jdk ONLY" and the
closing "**Mode: synthetic-jdk only, in both directions**" were both derived from
the call graph, correctly, and neither had ever been run: `--synthetic-jdk` had
never been launched on a `--features synthetic-jdk` binary. It has now.

**The scoping claim holds. The repair's intended effect does not appear.**

The stated purpose of deleting `native_secure_random_set_seed_long` /
`_set_seed_bytes` / `_init_seed_bytes` from `crypto_impl.rs` was that their
no-op bodies "silently undid two fixes in `securerandom.rs`: SHA1PRNG reseeding,
which HotSpot makes reproducible and which is the one replay guarantee the JDK
gives a `SecureRandom`; and the `algorithm` / `provider` stamping this record
exists for." Both are still missing in the mode where `securerandom.rs` is
supposed to be the survivor:

```
                                        HotSpot 25    --jdk-only     --synthetic-jdk
R secureRandom.sha1prng.reproducible    equal=true    equal=true     equal=false
   (two SHA1PRNG instances, setSeed(42L), 8 bytes each)
R secureRandom.setSeedBytes.reproducible equal=true   equal=true     equal=false
   (same, setSeed(new byte[]{1,2,3}))
R secureRandom.nextInt.deterministic    equal=true    equal=true     equal=false
   (same, setSeed(1234L), one nextInt())
R secureRandom.default.algo             algo=DRBG     algo=OS-CSPRNG NoSuchMethodError:
                                        provider=SUN  provider=SUN   java.security.SecureRandom
                                                                     .getAlgorithm()Ljava/lang/String;
R secureRandom.sha1prng.algo            SHA1PRNG      SHA1PRNG       (same NoSuchMethodError)
R security.getProviders                 count=13      count=13       count=5
                                        first=SUN     first=SUN      first=SUN
```

Two separate findings, and they should not be merged:

1. **`SecureRandom.getAlgorithm()` is not registered at all in `--synthetic-jdk`.**
   The `algorithm` stamping this record exists for cannot be read by any Java
   caller in that mode — the accessor is absent, so the field's value is
   unobservable whatever `securerandom.rs` writes. The same applies to
   `getProvider()` (the probe dies on `getAlgorithm()` first).
   `--jdk-only` answers `SHA1PRNG` correctly, so the stamping half is genuinely
   working in the shipping modes and only there.

2. **SHA1PRNG reseeding is still not reproducible in `--synthetic-jdk`.** Three
   independent formulations — `setSeed(long)`, `setSeed(byte[])`, and
   `nextInt()` after `setSeed(long)` — all disagree between two identically
   seeded instances. That is precisely the replay guarantee the deletion was
   made to restore, in the only mode the deleted registrations were ever live
   in. Either the deletion did not take effect on this build, or
   `securerandom.rs`'s SHA1PRNG path is itself not seeded-deterministic in
   synthetic mode. **This lane did not distinguish those two** — it writes no
   Rust and did not build a second binary — and that is the open question.

Green in `--synthetic-jdk`, so the surface is not wholesale broken:
`generateSeed(8)` returns 8 non-zero bytes; `MessageDigest.getInstance("SHA-256")`
digests `"abc"` to `ba7816bf8f01cfea…`, byte-identical to HotSpot;
`SecureRandom.getInstance("SHA1PRNG")` returns a `java.security.SecureRandom`
without throwing.

**Verdict: the residual is CONFIRMED LIVE in `--synthetic-jdk`, and the record's
"in both directions" scoping is confirmed** — every `--jdk-only` cell matches
HotSpot, so nothing here can move a Compatible-mode baseline or
`scripts/baselines/jdk-only-bridge-ratchet.json`, exactly as the record says. It
is **not retired**: the fix's own falsifier fails. See lane A31's NOMINATION A31-7.
