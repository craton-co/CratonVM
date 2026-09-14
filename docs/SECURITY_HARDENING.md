# CratonVM — Security Hardening & Sandboxing

This document describes CratonVM's **opt-in defence-in-depth surface**:
egress/SSRF policy, filesystem confinement, decompression-bomb and request-body
caps, the cryptographic posture, and what is *not* covered. These controls are
useful inside an OS sandbox; they do not make an in-process trust boundary.

See also: [`SECURITY.md`](../SECURITY.md) (overall security policy and
disclosure process), [`CONFIG.md`](CONFIG.md) (full flag/env reference), and
[`CRYPTO_STATUS.md`](CRYPTO_STATUS.md) (per-algorithm crypto status).

## Threat model & posture

CratonVM aims to be a **JDK-faithful general-purpose JVM by default**. That
means the env-less default behaviour matches HotSpot: file I/O is unconfined,
outbound sockets are permitted, and a missing `java.policy` means
"allow everything" (the no-`SecurityManager` compatibility path). Faithfulness
is a correctness requirement for the app-compat work, so hardening is layered
on top as **opt-in profiles** rather than baked into the default.

What CratonVM **does** give you, opt-in:

- Defence-in-depth knobs to confine a workload's filesystem, bound its memory
  use against malicious archives / requests, and restrict where it can connect.
- One always-on egress default: well-known cloud-metadata link-local addresses
  (e.g. AWS IMDS `169.254.169.254`) are blocked on the native connect path.

What CratonVM is **not**:

- **Not a certified sandbox.** None of this has had a formal third-party
  security audit. The crypto cores use audited RustCrypto crates where noted,
  but the VM as a whole is unaudited.
- **Not a substitute for OS-level isolation.** For genuinely adversarial code,
  run CratonVM inside a container / VM / seccomp profile / separate UID. The
  in-process knobs reduce blast radius; they do not replace kernel boundaries.
- The Java `SecurityManager` path is **partial** (see Limitations).

## Hardening environment variables

All flags follow the same convention unless noted: **presence enables**, and
the values `0` / `false` / `off` / `no` (case-insensitive, after trimming)
disable. They are read **once at startup** and cached for the process lifetime,
so they cannot meaningfully change mid-run. All default **off** (permissive,
JDK-faithful) except where a numeric default is listed.

| Env var | Effect | Default |
|---------|--------|---------|
| `CRATONVM_CONFINE_IO` | **Confinement profile.** Turns on CWD path confinement and registers the process CWD as a sandbox root. **Fails closed**: if confinement cannot actually be enabled at startup, the VM aborts rather than running unconfined. (`apply_certified_deployment_profile` in `native-io/src/lib.rs`.) | off |
| `CRATONVM_UNTRUSTED_CODE` | **Strict defence-in-depth profile.** Enables the same fail-closed filesystem confinement, implies `CRATONVM_REQUIRE_POLICY`, and rejects JNI/Panama downcalls and host-library loading even if a Java policy would otherwise grant them. It still requires an OS/container boundary. | off |
| `CRATONVM_REQUIRE_POLICY` | When set, a **missing** `java.policy` **denies** (fail-closed) instead of the JDK-default allow-all. The absence of an explicitly-loaded policy can then never be mistaken for a grant. Must be set before the first permission check. (`security_manager.rs`.) | off (allow-all on no policy, JDK-parity) |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Extends the default egress policy to also deny loopback (`127.0.0.0/8`, `::1`) and the RFC1918 private ranges (`10/8`, `172.16/12`, `192.168/16`) plus their IPv6 equivalents (`fc00::/7` ULA and IPv4-mapped private v6), so a confined workload can't reach internal services by SSRF. The link-local metadata block is **always on** regardless. (`outbound_policy.rs`.) | off |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound **hostnames** and apply the per-IP egress policy to every resolved address (deny if any is blocked) — closes the DNS-alias/rebind bypass for direct `check_outbound`/`default_policy` callers. (`outbound_policy.rs`.) | off (no DNS in policy) |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | Treats a JAR's `Class-Path:` manifest attribute as **untrusted**: out-of-tree / escaping entries are dropped (with a debug log) instead of silently honoured. (`classloading/src/class_path.rs`.) | off (HotSpot parity) |
| `CRATONVM_HTTP_MAX_BODY` | Max accepted inbound request-body size (bytes) for the embedded `com.sun.net.httpserver`. Larger bodies are rejected with `413`. `0` / unparseable → default. (`net_phase_e.rs`.) | `8388608` (8 MiB) |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Max *declared* uncompressed size (bytes) of a single zip/JAR entry that will be inflated into memory — a decompression-bomb guard. Paired with an always-on `1000:1` compression-ratio cap. `0` / unparseable → default. (`native-io/src/zip_real_jar.rs`.) | `536870912` (512 MiB) |
| `CRATONVM_TRUST_PEM` | Path to a PEM bundle of additional trust anchors for JAR-signature verification, consulted alongside the system trust store. (`classloading/src/jar_signer.rs`.) | unset |

> Note: the confinement/untrusted profiles only *add* restrictions; they never
> relax the env-less default. The CWD-confinement machinery is the same opt-in
> code path you can also drive programmatically via `set_path_confine_to_cwd` /
> `add_sandbox_root`.

### Filesystem confinement details

When confinement is on, paths are validated against the CWD root plus any
registered sandbox roots (`is_within_sandbox` / `validate_path`). Independently
of confinement, an **always-on traversal guard** (`has_escaping_parent_segment`)
rejects a *leading* `..` on a relative path that would climb out of its anchor —
but it deliberately allows interior `..` that merely cancels a preceding
component (e.g. `apps/kafka/../config/x.properties`), because the JDK does, and
CratonVM must stay faithful.

## Cryptographic posture

Full per-algorithm detail lives in [`CRYPTO_STATUS.md`](CRYPTO_STATUS.md). The
hardening-relevant highlights:

- **SecureRandom is backed by the OS CSPRNG.** `os_random_bytes`
  (`crypto_impl.rs`) reads from `RtlGenRandom`/`BCryptGenRandom` on Windows and
  `/dev/urandom` on Unix; `SecureRandom::new` seeds from it and `next_bytes`
  draws directly from it (with one retry). Key/nonce/prime generation
  (`SecureRandom::new`) all draw from this source. If OS entropy is *completely*
  unavailable, the code logs a warning and falls back to a time/PID-mixed
  splitmix64 seed — a **best-effort degraded mode**, not a cryptographic source.
- **AES / AES-GCM are constant-time.** They are backed by the audited RustCrypto
  `aes` / `aes-gcm` crates. The previous hand-rolled FIPS-197 S-box core and
  bit-loop GHASH (both classic cache-timing oracles, Bernstein 2005) were
  deleted (`crypto_impl.rs`, comment "C18").

What is still **best-effort / not fully constant-time**:

- **RSA private-key operations use base blinding but are not fully
  constant-time.** Signing/decryption apply RSA **base blinding**
  (`rsa_private_modpow_blinded` / `rsa_random_coprime` in `crypto_impl.rs`): the
  secret-exponent `modpow` runs on a random, ciphertext-independent operand, so
  the **message-dependent** timing/branch channel is removed. The underlying
  `BigUint::modpow` is still variable-time (no Montgomery-ladder constant-time
  guarantee, no CRT), so the fixed exponent `d` still drives a non-constant-time
  modexp. This is adequate for functional JCA compatibility and removes the
  most practical timing oracle, but should **not** be relied on as fully
  side-channel-resistant. ECDSA scalar/nonce paths remain variable-time
  (documented in-code). For hardened RSA/ECDSA, terminate it in audited native
  crypto outside the VM. Keep this section and `CRYPTO_STATUS.md` in sync.

## Network hardening

### Egress / SSRF policy

The native blocking-connect path runs every outbound target through an
**outbound policy** before connecting (`native-io/src/outbound_policy.rs`):

- **Default policy:** deny well-known cloud-metadata link-local IPs. This is the
  whole IPv4 link-local block (`169.254.0.0/16` — catches IMDS `…169.254`, the
  ECS task-role endpoint `…170.2`, etc.), IPv6 link-local (`fe80::/10`), and the
  AWS IPv6 metadata address (`fd00:ec2::254`). Both bracketed and bare,
  unbracketed IPv6 literals are parsed correctly (`host_part` returns a multi-
  colon literal whole so it parses as an `IpAddr`).
- **Resolution-time re-check.** `policy_connect` re-evaluates the active policy
  against **each resolved IP**, closing the DNS-rebind / hostname-alias escape
  (e.g. `metadata.google.internal` → `169.254.169.254`). This mirrors the
  non-blocking path's `resolve_and_vet`.
- **Optional active hostname resolution** via `CRATONVM_RESOLVE_OUTBOUND_HOST`:
  when set, outbound **hostnames** are resolved and the per-IP policy is applied
  to **every** resolved address (deny if any is blocked), closing the alias
  bypass even for callers that hit `check_outbound`/`default_policy` directly
  rather than going through `policy_connect`. Off by default (the documented
  no-DNS, low-latency, TOCTOU-free default posture).
- **IPv4-mapped/compatible IPv6** literals (`::ffff:169.254.169.254`,
  `::a.b.c.d`) are unwrapped and run through the v4 metadata/link-local check, so
  they cannot bypass the metadata block as a v6 address.
- **Optional private-net block** via `CRATONVM_BLOCK_PRIVATE_NETS` (see table).
- **Embeddable.** `set_policy(fn)` installs a custom `PolicyFn`; `reset_policy()`
  restores the default. A denial maps to a Java `IOException`.
- **Bounded connect timeout.** Both connect paths cap at 30 s by default
  (`connect_timeout`), so a connect to a blackholed metadata IP can't hang a VM
  thread for the OS-default TCP timeout.

> The literal-IP check is intentionally **not** preceded by its own DNS lookup
> (that would double connect latency and open a TOCTOU window); the resolution-
> time re-check above is what catches hostname-based SSRF.

### Request-smuggling defence (embedded HTTP server)

`parse_http_request` in `net_phase_e.rs` rejects classic
HTTP-request-smuggling vectors with a `400`:

- **Conflicting / duplicate `Content-Length`** headers (and comma-lists with
  differing members) — instead of letting "the last one win".
- **Unparseable `Content-Length`.**
- **`Transfer-Encoding` desync.** `Transfer-Encoding: chunked` is now honored
  (the body is decoded by chunked framing, capped by `CRATONVM_HTTP_MAX_BODY`);
  a request carrying **both** `Transfer-Encoding` and `Content-Length`, or an
  unknown transfer coding, is rejected with `400` — closing the TE/CL desync that
  previously read zero body bytes and left the chunked payload unread on a
  keep-alive connection.

It also bounds memory: the advertised length is checked against
`CRATONVM_HTTP_MAX_BODY` **before** any allocation, bytes already buffered with
the header count toward the cap, and the body buffer is allocated with bounded
(never the raw client-advertised) capacity. Oversized → `413`.

### Outbound HTTP client

The built-in HTTP client (`http_client.rs`) **strips credential headers**
(`Authorization`, `Proxy-Authorization`, `Cookie`) when a redirect crosses to a
different origin (scheme/host/port), so a redirect to an attacker host cannot
exfiltrate the caller's credentials. It also bounds the chunked reader's
size-line/trailer accumulation (anti-DoS) and handles the HTTP/2 `PADDED` flag on
`HEADERS` frames.

### Decompression-bomb guard

JAR/zip inflation (`zip_real_jar.rs`) enforces both an absolute per-entry
uncompressed cap (`CRATONVM_ZIP_MAX_ENTRY_BYTES`, default 512 MiB) and an
always-on `1000:1` declared-uncompressed-to-compressed ratio cap, so a "4 GiB of
zeros in a few KiB" entry is refused rather than inflated.

### TLS trust validation

The `X509TrustManagerImpl` natives (`x509_manager.rs`) perform real RFC 5280 §6
chain validation on `checkServerTrusted` / `checkClientTrusted`: validity-period
(clock) checks, signature verification against each issuer's SPKI, issuer↔subject
DN continuity, intermediate `BasicConstraints.cA = TRUE`, **name constraints
(RFC 5280 §4.2.1.10)**, and chain-end matching against trust anchors from
`rustls_native_certs`. Failure raises a `CertificateException`.

Name constraints are now enforced per the §6.1.4 state machine: each CA's
`NameConstraints` extension (permitted *and* excluded subtrees) is checked
against the subject DN and SubjectAltName of every certificate beneath it in the
path — including a name-constrained root that is not shipped in the chain, and
honouring the §6.1.3(b) self-issued-intermediate exemption. The `GeneralName`
types evaluated are `dNSName`, `rfc822Name`, `uniformResourceIdentifier` (host),
`iPAddress` (CIDR), and `directoryName` (RDN prefix). Residual limits:
`GeneralName` types this verifier does not model (`otherName`, `x400Address`,
`ediPartyName`, `registeredID`) are not enforced; `directoryName` matching is
byte-exact per RDN (no attribute-value string normalisation); and CRL revocation
checking remains gated off by default.

### Resolved native-crash classes

The historical real-`RandomAccessFile` JIT crash was not evidence that real JDK
bytecode is inherently unsafe. It exposed two VM defect classes, both of which
now carry targeted regression coverage:

1. JIT/native transitions that did not consistently publish every live object
   reference to the collector.
2. Cached compiled call targets that could survive a class or redefinition
   state change which invalidated their assumptions.

Real `RandomAccessFile` is the default path; `CRATONVM_SYNTHETIC_RAF=1` exists
only as a diagnostic compatibility fallback. A new native crash belongs in
`docs/known-issues/` with a minimal reproducer — do not reopen this historical
umbrella without evidence that one of the two mechanisms above has regressed.

## Limitations / what's not done

- **No formal audit.** Nothing here has been independently security-reviewed.
  Treat the hardening profiles as defence-in-depth, not as a trust boundary.
- **SecurityManager is partial.** The default (no policy ⇒ allow-all) is
  JDK-faithful; `CRATONVM_REQUIRE_POLICY` flips it to fail-closed and
  `CRATONVM_UNTRUSTED_CODE` implies that setting. Sensitive native gates
  (`ProcessBuilder.start` / `Runtime.exec`, host-library loading, JNI, and
  Panama/FFM downcalls) fail closed in the strict profile. The permission
  surface is still not a complete `java.security.Policy` implementation.
- **Egress policy guards the native connect paths**, not arbitrary
  user-supplied socket code that bypasses them; the literal-string default does
  no DNS itself (the per-resolved-IP re-check in `policy_connect` is what closes
  rebind). Embedders wanting a stricter posture should install their own
  `set_policy`.
- **RSA is not side-channel-hardened** (see Crypto posture above).
- **One known moving-GC follow-up.** Under the moving collector there is a
  documented residual gap where a JIT worker's *published* root snapshot can be
  stale relative to its current JIT spill slots at an STW safepoint, leaving the
  peer collector blind to those roots. The complete fix is a cross-thread STW
  JIT root scan (precise oop maps / shadow-stack work); it is **not yet
  implemented** (`vm/src/jit/conservative_roots.rs`). In practice the JIT-active
  path forces the non-moving young sweep with selective promotion, so this has
  not been observed to corrupt the heap — but it is an open correctness item,
  noted here for honesty.

## Reporting

Security issues should be reported per the process in
[`SECURITY.md`](../SECURITY.md) — please do not file them as public issues.
