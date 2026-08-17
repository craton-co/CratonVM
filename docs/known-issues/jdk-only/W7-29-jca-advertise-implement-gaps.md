# The JCA engines that answer names they never advertised

> ## RETIRED as work, 2026-08-12 (lane A3) — re-verified in source, and the one item this record left open is now closed
>
> **Method: source verification, not a run.** This lane could not build. Every
> claim below is a grep/read of the tree at this branch, and is labelled as such.
> No CratonVM binary was produced and no probe was executed on this pass; the
> HotSpot columns in the body remain the only measurements in this file.
>
> ### The advertise-vs-serve enumeration, re-taken
>
> The shape this record is about — *"the provider advertises an algorithm it
> cannot actually serve"* — is now held by **ratchets that derive one side from
> the registry**, not by a list, which is what makes re-enumerating it a matter
> of checking the ratchets exist rather than re-counting names. All present:
>
> | engine | direction | ratchet, verified present |
> |---|---|---|
> | `CertificateFactory` | both | `certificate_factory_serves_exactly_the_advertised_types` + `certificate_factory_stub_only_stands_in_for_x509` (`native-builtins/src/phases_late/ssl_security.rs`) |
> | `Cipher` | both | `provider_chain::every_advertised_sunjce_cipher_is_serviceable` |
> | `Mac` | both | `provider_chain::every_advertised_sunjce_mac_is_computable` |
> | `KeyGenerator` | implemented → advertised | `provider_chain::every_keygenerator_the_engine_implements_is_advertised` |
> | `Signature` | accepted ⊆ offered | `jca::signature`/`provider_chain::signature_name_is_offered` |
>
> ### The five residuals — all five FIXED in source, checked by symbol
>
> 1. **`MD2`** — `real_md2` exists in `native-builtins/src/jca/message_digest.rs`.
>    Implemented, per this record's own "strictly better" alternative, rather
>    than de-advertised.
> 2. **SHAKE** — `canonical_algorithm` in `message_digest.rs`, which is the
>    alias half this record's own measurement said was the part that had not
>    landed.
> 3. **Mutable set** — `wrap_unmodifiable` in `jca/provider_chain.rs`.
> 4. **`ML-DSA` umbrella** — gone from the `SUN` `KeyFactory` seed; the seed loop
>    at `provider_chain.rs:1131` carries `DSA, ML-DSA-44/65/87` only, and the
>    `Signature` loop 6 lines later still carries the umbrella, which is the
>    asymmetry the record prescribed. The `SunJCE` `KeyFactory` `ML-KEM` twin
>    that "neither record names" is also absent and is now reasoned in place
>    (`provider_chain.rs:1265`). Both are held by a test that names the pair
>    `[("SUN", "ML-DSA"), ("SunJCE", "ML-KEM")]`.
> 5. **`Signature.getInstance` accepts every name** — `signature_name_is_offered`
>    exists and is the disjunction gate W7-63 §3 #5 describes, not the
>    `find_service_provider`-only gate prescribed here.
>
> **Required companion edit: APPLIED.** `vm/src/vm/tests.rs:51239` carries the
> comment *"in-tree Java callers ask for \"X.509\". W7-29."* beside the real
> string.
>
> **Coverage: `regression-suite/src/RJdkSecurity.java::advertisedVersusServed`
> exists**, which is the scheduled vector the second pass said the five had
> gained.
>
> ### The one item this record left open, now CLOSED
>
> "What was deliberately not done" — `getType()` on the synthetic
> `CertificateFactory` fallback answering `null` where HotSpot echoes the
> caller's spelling. The record's reasoning for leaving it was exactly right and
> is worth keeping: the fallback wrote `Value::Object(None)` into **raw slot 0**,
> and in real-JDK mode the allocation funnel widens the object to
> `java.security.cert.CertificateFactory`'s real three-field layout
> (`provider`, `certFacSpi`, `type`), so slot 0 is `provider`, not `type`.
>
> Fixed 2026-08-12 in `register_p68_security_cert`: the raw slot-0 write is
> **removed** (nothing reads slot 0 of this receiver — grepped) and replaced
> with `ctx.set_field_by_name(obj, "type", <the caller's spelling>)`, which
> lands on `type` whatever the layout is and is a no-op when the field is
> absent (synthetic-stub mode), so it is safe on both. That is the
> `set_field_by_name` pair the record named and could not verify.
>
> **Not verified by running it.** The acceptance vector is this record's own:
> `CertificateFactory.getInstance("x509").getType()` must answer `x509`, and
> `getInstance("X.509").getType()` must answer `X.509` — the caller's spelling,
> not a canonical one. Note this arm is reached only when the real SPI declines
> to build AND the type folds to X.509, so a run must force that path (or read
> the value in `--synthetic-jdk`, where `set_field_by_name` is the no-op and the
> answer legitimately stays `null`).
>
> **Why this belongs to a TLS-stream lane at all:** it is the same defect
> species as the one that lane fixed — a native writing its own idea of a field
> over whichever real field happens to sit at index 0. See
> `W7-61-sslengine-layout-and-tls-blocking.md`'s third-pass section.
>
> **Nothing else in this file is work.** The line citations throughout the body
> have rotted and are a picture of the pre-fix tree.

> **SUPERSEDED for its residuals, 2026-08-12 — W7-63-jca-advertise-vs-serve.md.**
> All five residuals below are **FIXED in source** there, together with W4-3's
> five live patches. Read W7-63 §3 rather than working from the residual
> blocks in this file; two of the prescriptions here were not what was done,
> and one of those would have been a regression:
>
> * **Residual 1** (`MD2`) — implemented rather than de-advertised, which is
>   the alternative this record already named as "strictly better".
> * **Residual 2** (SHAKE) — implemented, then advertised, in that order, with
>   the alias/service split and the normalisation-agreement assertion this
>   record asked for. **The alias half was still open until a second pass on
>   2026-08-12.** This record's own measurement — *"`getInstance("SHAKE128")`
>   and `("SHAKE256")` both resolve on HotSpot and return byte-identical
>   digests"* — was the half that did not land: the `put_alias` rows went in and
>   were ratcheted through `get_service_entry`, but `MessageDigest.getInstance`
>   reads no registry, so both bare spellings still threw. Closed by
>   `canonical_algorithm` in `native-builtins/src/jca/message_digest.rs`; the
>   reasoning is in W7-63 §3 #2.
> * **Residual 3** (mutable set) — applied as written.
> * **Residual 4** (`ML-DSA` `KeyFactory`) — de-advertised, on this record's
>   own reasoning. The census also found `SunJCE` `KeyFactory` `ML-KEM`, an
>   identical defect eleven lines away that neither record names.
> * **Residual 5** (`Signature.getInstance` accepts every name) — gated, but
>   **NOT on `find_service_provider` alone as prescribed here.** That gate
>   would refuse every signature-algorithm OID at `getInstance`, because the
>   registry carries friendly names only while `signature::algo_idx`
>   deliberately carries the OIDs that X.509 `cert.verify()` resolves by. It
>   would have broken certificate verification outright. The shipped gate is a
>   disjunction; W7-63 §3 #5 has the reasoning. This record's follow-on
>   instruction to delete the `"Unknown"` sentinel as unreachable is likewise
>   wrong under that gate — nine advertised `SunRsaSign` names still reach it.
>
> Everything else here stands, including the warning about Patch E and the
> headline `CertificateFactory` fix, which is applied.

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** The **required
> companion edit** is **APPLIED**: `vm/src/vm/tests.rs:51208-51213` no longer
> passes `Value::Object(None)` — it builds `let x509 = create_java_string(&shared,
> "X.509");` and says so in a comment naming W7-29. The lane's own fix is
> present at `native-builtins/src/phases_late/ssl_security.rs:5422`/`:5508`/
> `:5511`, commit `b4e105f65`.
>
> ~~**All five recorded residuals are STILL OPEN**~~ — **STRUCK 2026-08-12
> (second pass).** All five landed with W7-63 later the same day and were
> re-verified in the tree: `real_md2` (1), the two `SHAKE` arms plus the alias
> resolution (2), `wrap_unmodifiable` (3), the `ML-DSA` umbrella gone from the
> `SUN` `KeyFactory` seed (4), and `signature_name_is_offered` (5). The line
> citations below have rotted with them; they are kept as a picture of the
> pre-fix tree, not as work. **Nothing in this file is work any more.** The five
> now have their first coverage in a scheduled vector —
> `RJdkSecurity.advertisedVersusServed()`, expect `PASS RJdkSecurity (80
> checks)` — where before they were held only by Rust unit tests and by
> `probes/JcaAdvertisedVsServedProbe.java`, which `regression-suite/run.sh` does
> not run. The as-written text follows:
>
> **All five recorded residuals are STILL OPEN**, re-grepped 2026-08-12:
> (1) `MD2` still seeded at `native-builtins/src/jca/provider_chain.rs:1088`;
> (2) no `SHAKE` service rows — the only mention is a comment at
> `provider_chain.rs:1299`; (3) neither `security_get_algorithms` nor
> `provider_get_services_native` wraps its result unmodifiable; (4) the `ML-DSA`
> umbrella is still advertised at `provider_chain.rs:1092`/`:1098`, though
> `jca/signature.rs:255-260` now carries an `SIG_MLDSA` arm; (5)
> `Signature.getInstance` still accepts every name.
>
> **These five overlap W4-3's Patches A/B/C/F** — they are the same defects seen
> from the other end. Fix them once, in one place, and close both records.
>
> Note this record's own warning, which is correct and load-bearing: W4-3's
> Patch E (refuse ChaCha20) is **superseded and must not be applied** — ChaCha20
> landed for real.

**Status:** one defect FIXED in source 2026-08-11 (lane W7-29) —
`CertificateFactory.getInstance` served *every* type name with an X.509 parser.
Five further gaps re-verified as **live against a running binary** and recorded
below as out-of-file patches, because they fall in files this lane does not own.
Nothing was rebuilt; every measurement here is of the pre-change binary.

Predecessor: W4-3-security-getalgorithms-short-list.md, whose 2026-08-11
residual pass produced the census this lane was asked to close. That census was
read from source. This lane **ran** it, on both VMs, and the run moved three
verdicts.

## What was measured, and with what

* Oracle: `C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`,
  `java.version=25.0.3`, `java.vm.name=OpenJDK 64-Bit Server VM`, Windows.
* Subject: `C:\craton\CratonVM\target\release\cratonvm.exe`, built 2026-08-11
  20:54 (dev HEAD at the time of the run: `78a0428efe8`, 21:08 — so the binary
  trails dev by one commit, in the threads area, not here).
* Every probe was run in **both** the default (`--real-jdk`) arm and the
  `--jdk-only` arm. All the findings below reproduce identically in both; where
  a mode matters it is called out.
* The probes print objects, class names and bytes. No probe prints a verdict.

## The headline: a census cannot see this direction

W4-3 asked "what is advertised but not implemented", and answered it by reading
source. Both halves of that are the problem.

`Security.getAlgorithms("CertificateFactory")` is `[X.509]` on HotSpot 25 and
`[X.509]` on CratonVM. The two lists have agreed the whole time. A census
comparing them finds nothing — and yet:

```
                              CratonVM                     HotSpot 25
getInstance("PKCS7")          a factory, getType()==null   CertificateException: PKCS7 not found
getInstance("AES")            a factory, getType()==null   CertificateException: AES not found
getInstance("")               a factory                    CertificateException:  not found
getInstance(null)             a factory                    NullPointerException: null type name

getInstance("PKCS7").generateCertificate(<a real 714-byte X.509 DER>)
   CratonVM: sun.security.x509.X509CertImpl, subject CN=jcagap, 714 bytes, no exception
   HotSpot:  never reached — the throw is at getInstance
```

A caller that asked for PKCS#7 got an X.509 certificate and no error anywhere on
the path. The advertised set never moved, so no comparison of the two lists
could have found it: **the set of names an engine will accept is not the set of
names it advertises, and only the second one is enumerable.** That asymmetry is
why the fix is a refusal at `getInstance` plus a ratchet, and not a corrected
list.

## Advertised versus implemented, per engine, before and after

Advertised = what `Security.getAlgorithms(type)` returned, measured. Accepted =
what `getInstance` returned an object for, measured. Serviceable = what the
engine then computes correctly.

| engine | advertised | accepted before | accepted after | HotSpot advertises |
|---|---|---|---|---|
| `CertificateFactory` | `X.509` (1) | **every string, including `null` and `""`** | `X.509` and its `X509`/case aliases only | `X.509` (1) |
| `MessageDigest` / SUN | 13 (incl. `MD2`) | 12 — `MD2` refused | unchanged (out of file) | 15 (incl. `SHAKE128-256`, `SHAKE256-512`) |
| `KeyFactory` / SUN chain | 18 (incl. `ML-DSA`) | 17 — `ML-DSA` refused | unchanged (out of file) | 20 |
| `Signature` | 42 | **every string** | unchanged (out of file) | 64 |
| `Mac` / SunJCE | 5 | 5 | 5 | 28 |
| `KeyManagerFactory` | `NEWSUNX509`, `SUNX509` | registry-gated | unchanged | same 2 |
| `TrustManagerFactory` | `PKIX`, `SUNX509` | registry-gated | unchanged | same 2 |
| `SSLContext` | 9 | registry-gated | unchanged | same 9 |
| `Provider("SUN").getServices()` | 35 rows | — | unchanged (out of file) | 65 rows |

`Mac`, `KeyManagerFactory`, `TrustManagerFactory` and `SSLContext` are the four
engines already closed by earlier lanes, and they are the four that hold up
under a run — which is the evidence that gating `getInstance` on the live
service registry is the shape that works.

## The one fix in a file this lane owns

`native-builtins/src/phases_late/ssl_security.rs`,
`register_p68_security_cert`'s `CertificateFactory.getInstance(String)`.

The registration used to try `provider_chain::try_build_real_certificate_factory`
and, on **any** failure, fall through to a bare one-field synthetic factory,
unconditionally. The fallback's only behaviour is parsing X.509 DER, so it
became the default arm for every name the real path could not resolve.

Three gates now run before either construction path:

1. a `null` type argument is `NullPointerException: null type name` — HotSpot's
   wording, measured, where it used to return a working-looking factory;
2. a type no provider in the live chain services is
   `java.security.cert.CertificateException: <type> not found` — HotSpot's
   wording, measured, and the *checked* exception
   `CertificateFactory.getInstance(String)` declares: *"@throws
   CertificateException if no `Provider` supports a `CertificateFactorySpi`
   implementation for the specified type"*. An unchecked `SecurityException`
   would sail past the caller's `catch`, the mistake `md_get_instance` and
   `mac_no_such_algorithm` both record having made and corrected;
3. a type the registry *does* carry but whose real SPI would not build falls to
   the synthetic factory only when it is X.509. Anything else is refused with
   the same exception, because an X.509 parse under another name is the same lie
   one layer down.

Gate 2 asks `provider_chain::find_service_provider`, which is the same predicate
`KeyManagerFactory`/`TrustManagerFactory.getInstance` in this file already use
and the same registry `Security.getAlgorithms` is answered from
(`algorithms_for_service`). Advertised and serviceable therefore cannot drift by
construction, a caller-registered `Provider` that genuinely implements PKCS#7 is
still honoured, and the alias/case folding is the JDK's own — `X509`, `x.509`
and `x509` all resolve, matching HotSpot, which echoes the caller's spelling
back from `getType()`.

**Mode: all.** `register_p68_security_cert` is reached from
`register_essential_natives_with_shims`, and the defect reproduced in both
`--real-jdk` and `--jdk-only`. This is a deliberate Compatible-mode behaviour
change, from "parses X.509 under the wrong name" to `CertificateException`.

### The ratchet

Two tests, in the same file:

* `certificate_factory_serves_exactly_the_advertised_types` — the shape the
  `Cipher` lane established with
  `provider_chain::every_advertised_sunjce_cipher_is_serviceable`: it asserts
  the advertised set and the serviceable set against each other in both
  directions, so widening one forces widening the other. It depends on the
  provider-chain seed having run, so it builds its registry through
  `crate::jca::register_jca_natives`, exactly as the neighbouring
  `KeyManagerFactory`/`TrustManagerFactory` tests do. **Known exposure:** the
  service registry is process-global and `provider_chain`'s own tests clear it
  under a lock this module cannot take. That exposure is inherited from the
  existing neighbours, not introduced here; if this test ever flakes, that is
  the reason, and the fix is to expose the lock rather than to weaken the
  assertion.
* `certificate_factory_stub_only_stands_in_for_x509` — a **pure function** test,
  so it cannot be voided by registry state at all. It pins gate 3's fold and,
  critically, pins the gate *ordering*: the fold is deliberately looser than the
  JDK alias table (it also folds `X-509`), which is safe only because gate 2
  runs first and the registry carries no such alias. The test asserts both
  halves of that pairing, so a future reordering of the gates fails here.

### What was deliberately not done

`getType()` on the synthetic fallback still answers `null` where HotSpot echoes
the caller's spelling (`getInstance("x509").getType()` is `x509`). The fallback
writes `Value::Object(None)` into raw slot 0, and in real-JDK mode
`try_alloc_concurrent_synthetic` upsizes the object to
`java.security.cert.CertificateFactory`'s real three-field layout
(`provider`, `certFacSpi`, `type` — `javap -p`), so slot 0 is `provider`, not
`type`. Fixing it means a `set_field_by_name` pair this lane could not build or
run to verify, and the fallback is now only reachable for X.509 in modes where
the real SPI would not construct. Recorded, not attempted.

### REQUIRED companion edit — one in-tree test asserts the defect

`vm/src/vm/tests.rs`, `certificate_factory_p68` (around line 51053) is the only
place in the tree that calls this native from Rust, and it calls it like this:

```rust
        let factory = call_native(
            &shared, &mut thread, cf,
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/cert/CertificateFactory;",
            &[Value::Object(None)],          // <- a NULL type argument
        )
        .unwrap()
        .unwrap();
        assert!(matches!(factory, Value::Object(Some(_))));
```

It passes `null` and asserts a factory comes back — which is only possible
because the registration ignored its argument entirely. **This test will fail
against the change in this branch, and it should**: HotSpot answers
`NullPointerException: null type name` for that exact call, measured. The test
is not testing `getInstance`; it is a fixture for the `generateCertificate` /
`X509Certificate` assertions below it, and the type argument was incidental.

The fix is the same like-for-like substitution the
`rabbitautoconfigurationtests` lane already made in
`trust_manager_factory_default_returns_concrete_x509_impl` when
`TrustManagerFactory.getInstance` started validating: pass a real, registered
name. Replace `&[Value::Object(None)]` with a `"X.509"` string, exactly as the
three in-tree *Java* callers do —
`regression-suite/src/RJdkX509Intercept.java:74`,
`vm/tests/resources/cratonvm/SslServerSocketEcho.java:162` and the
`pemcertificates-clientauth` repro all ask for `"X.509"` and are unaffected by
this change.

That file is outside this lane's ownership, so the edit is **not** applied here.
It is the one thing that must land with this commit.

## The five live residuals in files this lane does not own

All five were re-verified against the running binary before being written down —
this campaign has documented fifteen records claiming a patch was never applied
when it was already in the tree, so nothing below is taken from a prior record's
word.

### Residual 1 — `MD2` is advertised and refused (LIVE)

```
CratonVM: Security.getAlgorithms("MessageDigest") contains MD2
          MessageDigest.getInstance("MD2")
              -> java.security.NoSuchAlgorithmException: MD2 MessageDigest not available
HotSpot:  getDigestLength()=16
          digest("")    = 8350e5a3e24c153df2275c9f80692773
          digest("abc") = da853b0d3f88d99b30283a69e6ded6bb   (provider SUN)
```

Both arms. `jca::message_digest::algorithm_supported` has no MD2 arm and nothing
in `native-builtins` or `native-builtins-crypto` implements RFC 1319.

W4-3's Patch B still applies unchanged: drop `"MD2"` from the `SUN`
`MessageDigest` list in `jca::provider_chain::seed_direct_native_engine_services`
— it is the first element of the array literal three lines above the comment
that declines to advertise SHAKE *for exactly this reason*. Implementing MD2
instead is the strictly better fix and closes three advertisements rather than
one, because `SunRsaSign` and `SunMSCAPI` both advertise `MD2withRSA`, which
resolves `MessageDigest.getInstance("MD2")` internally. Nothing in the corpus
asks for MD2 today. The vectors above are the acceptance test either way.

**Mode: all.**

### Residual 2 — SHAKE can be implemented, and should be, before it is advertised (LIVE)

```
CratonVM: MessageDigest.getInstance("SHAKE128-256")
              -> NoSuchAlgorithmException: SHAKE128-256 MessageDigest not available
          (same for SHAKE256-512, SHAKE128, SHAKE256)
          getAlgorithms("MessageDigest") does NOT list them  <- correct, as the tree stands
```

Re-measured on jdk-25.0.3.9-hotspot this lane, confirming W4-3's table exactly:

| algorithm | `getDigestLength()` | digest of `""` | digest of `"abc"` |
|---|---|---|---|
| `SHAKE128-256` | 32 | `7f9c2ba4e88f827d616045507605853ed73b8093f6efbc88eb1a6eacfa66ef26` | `5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8` |
| `SHAKE256-512` | 64 | `46b9dd2b0ba88d13233b3feb743eeb243fcd52ea62b81b82b50c27646ed5762f` `d75dc4ddd8c0f200cb05019d67b592f6fc821c49479ab48640292eacb3b7c4be` | `483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739` `d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4` |

And the alias half, which is the part that decides whether the *advertised*
count lands on 15 or 17: `MessageDigest.getInstance("SHAKE128")` and
`("SHAKE256")` both resolve on HotSpot and return **byte-identical** digests to
the `-256`/`-512` spellings (measured, above), while
`Security.getAlgorithms("MessageDigest")` lists only the two hyphenated primary
names. They are `Alg.Alias.MessageDigest.*` rows, so they belong in `put_alias`,
never `put_service`.

The dependency is already in place: `sha3 = "0.10"` is a direct dependency of
both `native-builtins` and `native-builtins-crypto`
(`native-builtins/Cargo.toml:149`), and `native-builtins-crypto/src/bc_newhope.rs`
already drives `sha3::Shake128` through `ExtendableOutput`/`XofReader` with a
NIST known-answer test beside it.

W4-3's Patch C is correct and still applies, with one correction to its step 1:
`compute_digest`'s normalisation is `algo.to_uppercase().replace(['-', '/'], "")`
(`native-builtins/src/lib.rs:35638`), which gives `SHAKE128256` /
`SHAKE256512`; `algorithm_supported`'s is alphanumeric-only filtering
(`native-builtins/src/jca/message_digest.rs:496`), which gives the same two
strings. The two normalisations agree here, which is what makes the single pair
of arm names valid in both files — that agreement should be asserted, not
assumed, because it is a coincidence of these particular names and not a
property of the two functions.

**Order is not negotiable: land the `compute_digest` /
`algorithm_supported` / `digest_length_bytes` arms and pin the four vectors
above as a unit test, and only then add the two `put_service` rows and the two
`put_alias` rows.** Advertising before implementing is the defect this whole
record exists for.

**Mode: all.**

### Residual 3 — `Security.getAlgorithms` and `Provider.getServices` return a mutable set (LIVE)

```
                                          CratonVM              HotSpot 25
getAlgorithms("MessageDigest").getClass() java.util.HashSet     Collections$UnmodifiableSet
   .add("ZZZ")                            SUCCEEDED             UnsupportedOperationException
getAlgorithms("NoSuchEngineType")         n=0 HashSet, mutable  n=0 Collections$UnmodifiableSet, immutable
getAlgorithms("")  /  ("Foo.")            n=0 HashSet, mutable  n=0 Collections$EmptySet, immutable
getAlgorithms(null)                       n=0 HashSet           n=0 Collections$EmptySet   (no NPE on either)
two calls return the same object?         false                 false
SUN.getServices().getClass()              java.util.HashSet     Collections$UnmodifiableSet
   .add(null)                             SUCCEEDED             UnsupportedOperationException
SUN.getServices().size()                  35                    65
```

Both arms. W4-3's Patch A applies unchanged and is well specified — the wrapper
goes on **every** path including the empty ones, must be built per call (HotSpot
returns a distinct object each time), and the failure path returns the plain set
rather than converting a correct answer into a throw. The `Collections$EmptySet`
vs `Collections$UnmodifiableSet` split HotSpot shows for `""`/`"Foo."` is not
worth reproducing: both are immutable and both are size 0, which is the entire
observable contract.

The `35` versus `65` in the last row is a separate, milder under-advertisement of
`SUN`'s service rows, in the same file. It is not covered by Patch A and is not
covered here; it is recorded so the next lane does not read the row as evidence
that Patch A landed badly.

**Mode: all.**

### Residual 4 — `SUN` `KeyFactory` advertises an `ML-DSA` umbrella it refuses (LIVE)

```
CratonVM: getAlgorithms("KeyFactory") lists ML-DSA, ML-DSA-44, ML-DSA-65, ML-DSA-87
          KeyFactory.getInstance("ML-DSA")
              -> NoSuchAlgorithmException: ML-DSA KeyFactory not available
          Signature.getInstance("ML-DSA")   -> OK, getAlgorithm()=ML-DSA
HotSpot:  KeyFactory.getInstance("ML-DSA")  -> OK, getAlgorithm()=ML-DSA, provider SUN
          Signature.getInstance("ML-DSA")   -> OK, getAlgorithm()=ML-DSA, provider SUN
```

Both arms. Exactly as W4-3's Patch F describes: `signature::algo_idx` carries the
umbrella arm and `key_factory::algo_idx` does not, so two engines disagree about
the same name. Dropping `"ML-DSA"` from the `SUN` `KeyFactory` seed list is the
smaller and more honest change while no corpus application asks for it.

Two things the source-read census did not have, both found by running it, and
both arguing that dropping the name is the *only* safe direction for now:

* the three parameter-set names that `getInstance` **does** accept produce a
  partly-unusable object — `KeyFactory.getInstance("ML-DSA-44").getProvider()`
  raises `NullPointerException: Cannot enter synchronized block because
  "this.lock" is null`, and so do `-65`, `-87` and `EdDSA`. HotSpot answers
  `SUN version 25`. So "add the umbrella arm" would widen a surface that is
  already broken one accessor in;
* `Signature.getInstance("ML-DSA*")` succeeds but `getProvider()` returns
  `null`, where HotSpot returns `SUN version 25`.

Neither is an advertise-versus-implement gap and neither is in scope here; both
are recorded because the next lane to touch `key_factory.rs` will meet them
immediately.

**Mode: all.**

### Residual 5 — `Signature.getInstance` accepts every name (LIVE, NEW this lane)

The census filed `Signature` / SUN as "7 advertised, 7 implemented, no gap". It
was read from source, and it missed the direction this record is about.

```
                        CratonVM                                  HotSpot 25
getAlgorithms("Signature").size()      42                         64
getInstance("SHA256withRSA")           OK, getAlgorithm()=SHA256withRSA   OK
getInstance("ML-KEM")                  OK, getAlgorithm()=Unknown  NoSuchAlgorithmException: ML-KEM Signature not available
getInstance("AES")                     OK, getAlgorithm()=Unknown  NoSuchAlgorithmException: AES Signature not available
getInstance("HmacSHA256")              OK, getAlgorithm()=Unknown  NoSuchAlgorithmException: HmacSHA256 Signature not available
getInstance("NO-SUCH-SIG")             OK, getAlgorithm()=Unknown  NoSuchAlgorithmException: NO-SUCH-SIG Signature not available
getInstance("")                        OK, getAlgorithm()=Unknown  NoSuchAlgorithmException:  Signature not available
```

**And now the part that decides how bad it is, which is the part a source read
cannot supply.** The engine does *not* fabricate a cryptographic result. Signing
and verifying through one of these objects both fail closed:

```
Signature.getInstance("NO-SUCH-SIG").initSign(rsaPriv).update(msg).sign()
  -> java.security.SignatureException: Signature.sign() could not be performed for
     Unknown (this VM has no native implementation for that algorithm): refusing to
     report a cryptographic result for an operation that never ran.

Signature.getInstance("AES").initVerify(rsaPub).update(msg).verify(garbage)
  -> the same SignatureException.   It does NOT return true, and it does NOT return false.
```

For contrast, the same probe on the real algorithm gives the same answer on both
VMs: `SHA256withRSA` signs 256 bytes with a byte-identical prefix
(`a2d219c8d491b3bd7dda1315ca6f56cc3ff86afec7f0a9c0…`), verifies `true` on the
correct message and `false` on a tampered one.

So this is **not** the `Cipher`/`Mac` species. It is a *deferred and mistyped*
refusal: the failure that HotSpot raises at `getInstance` as
`NoSuchAlgorithmException` surfaces here at `sign()`/`verify()` as
`SignatureException`. Both are checked, but they are different types, so a
caller doing

```java
try { s = Signature.getInstance(name); } catch (NoSuchAlgorithmException e) { fallback(); }
```

takes the wrong branch — it believes the algorithm is available, and finds out
much later, at a point where its own `catch` clauses are written for a bad
signature rather than a missing algorithm. Probing an engine for a name it does
not have is an ordinary thing for library code to do.

The fix is the shape used four times over now: gate `Signature.getInstance` on
`provider_chain::find_service_provider("Signature", algo)` before allocating a
receiver, and refuse with `throw_no_such_algorithm_public` in HotSpot's measured
wording, `<name> Signature not available`. The `getAlgorithm()` sentinel
`"Unknown"` — the value that made the fabricated object *look* like a valid one —
becomes unreachable and should be deleted with it.

The 42-versus-64 advertised gap is the ordinary under-advertisement direction
and must **not** be closed by widening the seed list: 22 of HotSpot's names are
not implemented here, and after the gate above they would each become a
`NoSuchAlgorithmException`, which is the truthful answer.

**Mode: all** — `Signature.getInstance` answered every name in both the default
and the `--jdk-only` arm.

## Recorded, not patched: intersects the concurrent ChaCha20 work

A separate session is implementing real ChaCha20 with Poly1305 on branch
`fix/chacha20-real-cipher-20260811`, in `native-builtins/src/jca/cipher.rs`,
`native-builtins/src/jca/provider_chain.rs`,
`native-builtins/src/crypto_impl.rs`, `native-builtins/src/phases_early.rs` and
a new `native-builtins/src/chacha20.rs`. This lane touched none of it and probed
none of it.

Two items above land in files that session is editing, and whoever applies them
must sequence against it rather than around it:

* **Residuals 1 and 2** (the `MD2` de-advertisement and the SHAKE
  service/alias rows) are edits to
  `jca::provider_chain::seed_direct_native_engine_services` — the same seed
  function whose `SunJCE` `Cipher` list that session is changing. Different
  array literals inside one function; a textual conflict is likely, a semantic
  one is not.
* **Residual 3** (Patch A, the unmodifiable wrapper) touches
  `security_get_algorithms` and `provider_get_services_native` in the same file,
  well away from the `Cipher` seeds.

`W4-3`'s Patch E (refuse `ChaCha20` / `ChaCha20-Poly1305` rather than serve them
as AES-256-ECB) is **superseded** if that session lands a real implementation:
the correct end state is the algorithms implemented *and* advertised, not
refused. Do not apply Patch E on top of a landed ChaCha20. Patch E's other half
— removing `AES/KW/PKCS5Padding` and `AES/KWP/NoPadding` from the `SunJCE`
`Cipher` seed list — is independent of ChaCha20 and stands on its own; W7-15's
`every_advertised_sunjce_cipher_is_serviceable` ratchet is the test that will
say so either way.

## How to verify this lane's fix

Against a binary built from this branch, in both arms:

```
CF[X.509]            RETURNED ... type=X.509 provider=SUN version 25
CF[X509]             RETURNED ...
CF[x.509]            RETURNED ...
CF[NO-SUCH-CERT-TYPE] THREW java.security.cert.CertificateException: NO-SUCH-CERT-TYPE not found
CF[PKCS7]            THREW java.security.cert.CertificateException: PKCS7 not found
CF[]                 THREW java.security.cert.CertificateException:  not found
CF[null]             THREW java.lang.NullPointerException: null type name
2arg[X.509,SUN]      RETURNED ...
2arg[BOGUS,SUN]      THREW java.security.cert.CertificateException: NO-SUCH-CERT-TYPE not found
2arg[X.509,NOPROV]   THREW java.security.NoSuchProviderException: no such provider: NoSuchProv
```

which is line-for-line what jdk-25.0.3.9-hotspot printed for the same class.
The two-argument overload needs no change of its own: it resolves the provider
first and then delegates to the one-argument registration, which is the ordering
HotSpot shows (bogus provider is `NoSuchProviderException`, bogus type behind a
good provider is `CertificateException`).

## The single falsifying observation

If a TLS or PKIX path regresses with
`CertificateException: <something> not found` where it used to work, then some
in-tree caller asks `CertificateFactory.getInstance` for a type the provider
registry does not carry, and the registry — not this gate — is what is short.
The gate is deliberately the registry and not a literal list precisely so that
the repair in that case is to seed the missing service, in one place, where
`Security.getAlgorithms` will report it too.
