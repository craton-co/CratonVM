# An `SSLSocket` client captures only the LEAF certificate, so an application `TrustManager` cannot validate any real chain

## Status
**FIXED 2026-08-17** on Unix, on branch `fix/tls-client-chain-and-seclevel-20260817`.
Found 2026-08-16. Differential-verified against Temurin 25.0.3 on the Azure
host, against live public servers, before and after.

```
RealChainProbe, 20 live public sites, the VM's own default TrustManager
handed the chain the VM captured:

                      HotSpot 25.0.3      CratonVM before      CratonVM after
  handshake                20 ok               20 ok               20 ok
  validator ACCEPT         20                   0                  20
  peer chain length       2-4                   1                 2-4
```

Closing it took two defects, not one. The second was **invisible until the
first was fixed**, because a validator that is never handed a real chain is
never asked a real question. A third was diagnosed, fixed, measured to fix
nothing, and reverted — §3 keeps it, because how it came to look like a cause
is the most reusable thing on this page.

The residue page it blocked,
`tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816-FIXED-20260817.md`,
is closed by the same change.

**Not closed on Windows.** See "What is still open" at the bottom.

---

## 1. The reported defect: the chain was one certificate

An application that installs its own `TrustManager`s — the normal shape for
HttpClient5's `SSLContextBuilder`, Apache HC, a JDBC driver with a custom trust
store, anything that calls `SSLContext.init(null, tms, null)` — **could not
connect to any ordinary public HTTPS server**, because the VM handed that
TrustManager a one-certificate chain from which no path to a root can be built.

`CustomTmProbe`, same run, same hosts, TrustManagers from
`TrustManagerFactory.init(null)` (the JDK's own default managers — nothing
exotic):

```
                          HotSpot 25.0.3          CratonVM before
github.com
  default factory         OK  peerChainLen=3      OK    peerChainLen=1
  custom-TM context       OK  peerChainLen=3      FAIL  SSLHandshakeException:
                                                        no trust anchor
repo.maven.apache.org     OK  peerChainLen=4      OK 1 / FAIL
```

The default factory still connected because OpenSSL verified internally with
its own chain. The custom-TM context was the one that broke, and it broke
because CratonVM correctly stands the native verifier down there — making the
Java TrustManager the only verifier — and then gave it nothing to work with.

### Root cause, and why the reasoning was once correct

`servlet::s2_tls_connect_on` said so itself:

```rust
// Capture the peer's leaf certificate DER bytes. native-tls's public API
// only exposes the leaf via `peer_certificate()`; the full chain is
// validated internally by the backend (SChannel / SecureTransport /
// OpenSSL) before `connect` returns, which is why we can rely on a
// single-element chain here without weakening security.
```

That was true when written: the backend was the verifier, and the captured leaf
was only ever informational. It stopped being true when the `java_tm_key` path
was added — that path disables native verification precisely so the Java
TrustManager can decide, and it consumes the same `peer_cert_chain_der` vector.
**A premise was invalidated by a later change to a different function, and
nothing re-checked it.** The comment is now kept at that site with its own
history spelled out, because the shape recurs.

### The fix

native-tls 0.2 exposes neither `peer_cert_chain()` nor `set_security_level`, so
the default client path moved off `native_tls::TlsConnector` onto a raw
`openssl::SslConnector` — `servlet::s2_openssl_tls_connect_on`.
`s2_legacy_dsa_tls_connect_on` was the working in-tree template; its
`set_verify_hostname(false)` and security level 0 are its own legacy
compromises and were **not** copied. Everything native-tls configured is
restated: the TLS 1.2 floor, the optional TLS 1.2 ceiling, SNI, hostname
verification, the 30 s socket timeouts, additive vs replacing root sets, and
the stand-down when a Java TrustManager is the verifier.

Three things the swap bought, which is why they landed together:

1. the full peer chain, so an application TrustManager can validate;
2. `getPeerCertificates()` matching HotSpot (2–4 where it used to say 1);
3. the security level aligned with the JDK's floor — the residue page.

It also replaced two compile-time constants with the real negotiated protocol
and cipher suite, which OpenSSL can be asked for and native-tls could not.

`CRATONVM_TLS_OPENSSL_CLIENT=0` reverts the path to native-tls **in the same
binary**, so the two backends are A/B-able without building a second tree.
Every number on this page was taken that way.

---

## 2. Found by the fix: ECDSA verification was P-256 and only P-256

With real chains finally reaching `x509_manager::validate_chain`, 12 of 20
sites passed and **six failed with `BadSignature`**. Every one of the six had a
**P-384 issuer key**:

| site | failing index | issuer key |
|---|---|---|
| `en.wikipedia.org`, `repo.maven.apache.org`, `letsencrypt.org`, `stackoverflow.com` | 0 | Let's Encrypt YE1 / YE2 (P-384) |
| `openjdk.org` | 1 | DigiCert Global G3 TLS ECC (P-384) |
| `github.com` | 2 | Sectigo Server Authentication Root E46 (P-384) |

`crypto_impl::Ecdsa` is P-256 by construction: a fixed 4×64-bit field element,
a hard-coded generator, and a `public_key_from_bytes` that requires exactly 65
bytes. `parse_ecdsa_public_key` also discarded the SPKI's named-curve OID
entirely. So a P-384 key parsed to `None` and the caller reported a bad
signature — **a refusal indistinguishable from a forged certificate**.

The modern public ECDSA hierarchy is P-384 at the top almost everywhere, so
"P-256 only" was not a corner case; it was most of the internet's ECDSA chains.

Fixed with a named-curve verifier over the existing `BigUint` (P-256 / P-384 /
P-521, Jacobian point arithmetic, curve selected by the SPKI's own OID). The
OCSP response verifier and `X509Cert::verify_signature` were routed through it
too — same defect, same shape, both on trust paths.

Parameters were read off OpenSSL 3.0's explicit form rather than transcribed,
because **a mistyped `p` or `n` does not produce a wrong answer — it produces a
verifier that rejects everything**, which looks exactly like the bug being
fixed. They are checked *as parameters*: the generator is on the curve, `n·G`
is the identity and `(n−1)·G` is not.

## 3. Not a defect, and the more useful half of the story

Two of the remaining sites — `www.cloudflare.com` and `adoptium.net` — said
`no trust anchor found for chain` where HotSpot accepts. Both end their chain
with `CN=GTS Root R4` **as signed by `CN=GlobalSign Root CA`**: a
cross-certificate whose bytes differ from the self-signed GTS Root R4 that JDK
25's `cacerts` ships, and whose own issuer that `cacerts` no longer ships at
all.

`x509_manager::presented_cert_is_anchor` requires
`anchor.full_cert_der == presented_der`, where RFC 5280 §6.1.1 identifies a
trust anchor by (name, public key). That is exactly the error printed, so it
read as the cause. **It was not.** A relaxation to `(subject_der, spki_der)`
was written, landed, and then measured:

```
RealChainProbe, ECDSA fix in, anchor relaxation REVERTED:
  handshake ok=20  validator accept=20  reject=0
```

It fixed nothing. `validate_chain` retries through `rebuild_path`, and
`select_path` stops the moment the current certificate's ISSUER is a configured
anchor — so the cross-signed tail is dropped and the anchor is found by issuer
lookup instead. The presented-order `NoTrustAnchor` is simply the error
`validate_chain` reports when the rebuild ALSO fails, and at the time the
rebuild failed for an unrelated reason: the P-384 gap in §2. Both sites were
closed by that, and the relaxation was reverted.

Two things worth keeping from it:

* **An error message is a symptom, not an attribution.** The one printed came
  from the arm that ran first, not from the arm that decided. The retry was
  right there in the same function.
* **The test written for it was vacuous and said so under pressure.** It passed
  identically with the relaxation reverted — because it had never exercised it.
  It survives, renamed to what it actually proves
  (`a_cross_signed_tail_is_dropped_and_the_real_anchor_still_found`) and
  re-verified by breaking `rebuild_path` instead. Had the five break-runs not
  been done, a change that moved nothing would have shipped inside a trust
  decision, with a green test over it.

---

## Verified

Azure host, `--java-home /data/toolchain/jdk-25 --nojit`, branch off
`origin/dev` `2f4b2f82c`, against a pristine build of that same commit.

```
                                  HotSpot     before      after
RealChainProbe  handshake            20 ok      20 ok      20 ok
                validator ACCEPT     20          0         20
CustomTmProbe   github.com          OK 3       OK 1/FAIL   OK 3
                www.google.com      OK 3       OK 1/FAIL   OK 3
                repo.maven          OK 4       OK 1/FAIL   OK 4
EngineChainProbe (SSLEngine path)   3/3/4      3/3/4       3/3/4
WeakChainProbe  (the residue)       OK 321ms   REFUSED     OK 53ms
TrustSetProbe   default anchors      119        119        119
```

The **SSLEngine path was the open question on this page** ("NOT VERIFIED …
rustls may well be unaffected"). `EngineChainProbe` drives a raw `SSLEngine`
wrap/unwrap handshake against the same three hosts, in both a default-context
and a custom-TrustManager arm. It measured 3/3/4 on both VMs before and after:
rustls does hand its verifier the whole chain, so that path never had this
defect. May-well-be is now a measurement.

### Regression cover

* `servlet::openssl_client_tests` — three tests over a real two-certificate
  chain served by an in-process `SslAcceptor`. **This is the shape no TLS
  fixture in the tree had.** Every existing TLS fixture uses a *self-signed*
  certificate — H2's baked identity, the netty test certs, the regression-suite
  keystores — and for a self-signed peer the leaf IS the trust anchor, so a
  one-element chain validates perfectly. The defect was invisible to the entire
  corpus by construction. That is the generalisable part: **a fixture whose
  certificate is self-signed cannot exercise chain building at all, and no
  number of them adds up to one that can.**
* `x509_manager::real_chain_shape_tests` — the cross-signed tail and a P-384
  chain, with real OpenSSL keys and real signatures.
* `crypto_impl::named_curve_param_tests` / `named_curve_openssl_tests` — the
  curve table checked as a table, and the verifier round-tripped against
  signatures OpenSSL made.

Every acceptance is paired with the refusal that gives it meaning: a chain
reaching no configured anchor is still refused, the same subject with a
*different* key is still refused, and one flipped byte in a P-384 signature is
still refused.

**Each was verified by BREAKING what it guards**, one property at a time in a
single tree, and confirming the intended test fails:

```
CONTROL (unmodified)                    16 passed, 0 failed
leaf-only capture again                 client_captures_the_whole_chain FAILED
security level back to OpenSSL's 2      client_security_level…floor    FAILED
anchor identity back to exact-DER       nothing failed — see §3
`rebuild_path` returns None             a_cross_signed_tail_is_dropped FAILED
one nibble of P-384's b                 the_generator_is_on_the_curve  FAILED
assume P-256, ignore the SPKI OID       p384_sha384_round_trips        FAILED
```

### Gates

* `cargo test -p cratonvm-native-builtins`: 4070 passed / 23 failed. The same
  23 fail on the base commit `2f4b2f82c`, name for name — all pre-existing, none
  in `servlet`, `x509_manager`, `crypto_impl` or `tls`. The branch adds 16
  passing tests.
* Netty's 78 SSL/TLS/cert test classes, fork-per-class, this binary against a
  pristine build of the base commit: identical status on every class.

  Two classes first read as regressions (`SniClientTest`,
  `ParameterizedSslHandlerTest`) and **were not**. Both failed with
  `TimeoutException` against a wall-clock budget, and the fixed-arm run had been
  started while a `cargo test` was saturating the same 8 cores — an instrument
  on one arm is a variable of the comparison. Re-measured interleaved,
  base/fixed × 3 reps: 3/3 and 7/7 on every rep, 6–23 s against 30 s and 48 s
  budgets.

## Repro
```bash
$JDK25/bin/java -cp <probe-dir> RealChainProbe                            # 20 accept
<cratonvm-bin> --java-home $JDK25 --nojit -c <probe-dir> RealChainProbe    # 20 accept
CRATONVM_TLS_OPENSSL_CLIENT=0 <cratonvm-bin> … RealChainProbe             # 20 reject, chainLen=1
```
`probes/CustomTmProbe.java`, `probes/RealChainProbe.java`,
`probes/EngineChainProbe.java`, `probes/WeakChainProbe.java` +
`probes/mkweakca-witness.sh`.

## What is still open

**Windows keeps the leaf-only chain.** `openssl` is a deliberately Unix-scoped
dependency of `native-builtins` (see its `Cargo.toml`), and native-tls exposes
no chain accessor on the SChannel backend either — so closing it there needs a
different backend, not a different configuration. The `#[cfg(not(unix))]` arm
is unchanged and was not measured; it is filed as its own narrow page,
`tls-client-windows-schannel-leaf-only-chain-20260817.md` under
`docs/known-issues`. The Windows build was compile-checked on this branch.

## Related
* `tls-client-trust-is-openssl-seclevel-not-the-jdk-trustmanager-20260816-FIXED-20260817.md`
  — the residue this blocked; the same connector swap closes it.
