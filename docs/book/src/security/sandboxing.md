# Sandboxing & Hardening

CratonVM exposes an **opt-in hardening surface** for running less-trusted or
multi-tenant workloads: filesystem confinement, an egress/SSRF policy,
request-body and decompression-bomb caps, and TLS-trust validation. Everything
here is **off by default** (the default posture is JDK-faithful — see [Security
Overview](overview.md)), except the always-on cloud-metadata egress block and
the numeric caps noted below.

> These reduce blast radius; they are **not** a certified trust boundary. For
> genuinely adversarial code, also use OS-level isolation.

## Convention

Unless noted, each switch follows the same convention: **presence enables**, and
the values `0` / `false` / `off` / `no` (case-insensitive) disable. Each is read
**once at startup** and cached, so it cannot meaningfully change mid-run.

## Hardening environment variables

| Variable | Effect | Default |
|----------|--------|---------|
| `CRATONVM_CONFINE_IO` | **Confinement profile.** Turns on working-directory path confinement and registers the process CWD as a sandbox root. **Fails closed:** if confinement cannot actually be enabled at startup, the VM aborts rather than running unconfined. | off |
| `CRATONVM_UNTRUSTED_CODE` | **Strict defence-in-depth profile.** Enables the same fail-closed filesystem confinement (it aborts identically — it does *not* warn and continue), implies `CRATONVM_REQUIRE_POLICY`, and rejects JNI/Panama downcalls and host-library loading even if a Java policy would otherwise grant them. It still requires an OS/container boundary. | off |
| `CRATONVM_REQUIRE_POLICY` | With a `SecurityManager` installed but no policy loaded, **deny** (fail-closed) instead of the JDK-default allow-all. | off |
| `CRATONVM_BLOCK_PRIVATE_NETS` | Extend the egress policy to also deny loopback (`127.0.0.0/8`, `::1`) and the RFC 1918 private ranges (and IPv6 equivalents), so a confined workload can't reach internal services by SSRF. | off |
| `CRATONVM_RESOLVE_OUTBOUND_HOST` | Resolve outbound **hostnames** and apply the per-IP egress policy to every resolved address (closes the DNS-alias / DNS-rebind bypass). | off |
| `CRATONVM_HARDEN_MANIFEST_CLASSPATH` | Treat a JAR's `Class-Path:` manifest attribute as untrusted: drop entries that resolve outside the JAR's own directory. | off |
| `CRATONVM_HTTP_MAX_BODY` | Max accepted inbound request-body size (bytes) for the embedded HTTP server; larger bodies get a `413`. | `8388608` (8 MiB) |
| `CRATONVM_ZIP_MAX_ENTRY_BYTES` | Max declared uncompressed size (bytes) of a single zip/JAR entry that will be inflated — a decompression-bomb guard, paired with an always-on 1000:1 ratio cap. | `536870912` (512 MiB) |
| `CRATONVM_TRUST_PEM` | Path to a PEM bundle of additional trust anchors for JAR-signature verification. | unset |

Both hardening profiles only *add* restrictions; they never relax the default,
and both fail closed rather than continuing unconfined. The same confinement
machinery is also drivable programmatically by an embedder (see
[Embedding](../embedding/overview.md)).

## Filesystem confinement

When confinement is on, paths are validated against the CWD root plus any
registered sandbox roots. Independently of confinement, an **always-on traversal
guard** rejects a *leading* `..` on a relative path that would climb out of its
anchor — but it deliberately allows interior `..` that merely cancels a
preceding component (e.g. `app/sub/../config/x.properties`), because a standard
JDK does, and CratonVM stays faithful.

## Network hardening

### Egress / SSRF policy

The native blocking-connect path runs every outbound target through an outbound
policy before connecting:

- **Default policy:** deny well-known cloud-metadata link-local addresses — the
  whole IPv4 link-local block (`169.254.0.0/16`, which catches the instance
  metadata and task-role endpoints), IPv6 link-local (`fe80::/10`), and the
  known IPv6 metadata address. This is **always on**.
- **Resolution-time re-check:** the active policy is re-evaluated against each
  resolved IP, closing the DNS-rebind / hostname-alias escape (e.g. a name that
  resolves to a metadata IP).
- **IPv4-mapped/compatible IPv6 literals** are unwrapped and run through the v4
  metadata/link-local check, so they can't bypass the block as a v6 address.
- **Optional private-net block** via `CRATONVM_BLOCK_PRIVATE_NETS`.
- **Optional active hostname resolution** via `CRATONVM_RESOLVE_OUTBOUND_HOST`.
- **Bounded connect timeout** (30 s default) so a connect to a blackholed
  metadata IP can't hang a VM thread for the OS-default TCP timeout.
- **Embeddable:** an embedder can install a custom policy function.

A denial maps to a Java `IOException`. The literal-IP check is intentionally not
preceded by its own DNS lookup (that would double connect latency and open a
time-of-check/time-of-use window); the resolution-time re-check is what catches
hostname-based SSRF.

### Request-smuggling defense (embedded HTTP server)

The embedded HTTP server rejects classic request-smuggling vectors with a `400`:

- Conflicting / duplicate `Content-Length` headers.
- Unparseable `Content-Length`.
- `Transfer-Encoding` desync: `Transfer-Encoding: chunked` is honored (the body
  is decoded by chunked framing, capped by `CRATONVM_HTTP_MAX_BODY`), but a
  request carrying **both** `Transfer-Encoding` and `Content-Length`, or an
  unknown transfer coding, is rejected.

The advertised body length is checked against the cap **before** any allocation,
and the body buffer is allocated with bounded capacity. Oversized requests get a
`413`.

### Outbound HTTP client

The built-in HTTP client **strips credential headers** (`Authorization`,
`Proxy-Authorization`, `Cookie`) when a redirect crosses to a different origin,
so a redirect to an attacker host can't exfiltrate the caller's credentials. It
also bounds the chunked reader's accumulation (anti-DoS).

### Decompression-bomb guard

JAR/zip inflation enforces both an absolute per-entry uncompressed cap
(`CRATONVM_ZIP_MAX_ENTRY_BYTES`, default 512 MiB) and an always-on 1000:1
declared-uncompressed-to-compressed ratio cap, so a "gigabytes of zeros in a few
KiB" entry is refused rather than inflated.

### TLS trust validation

CratonVM performs real RFC 5280 §6 X.509 chain validation on trust-manager
checks: validity-period checks, signature verification against each issuer,
issuer↔subject continuity, intermediate `BasicConstraints.cA = TRUE`, **name
constraints** (permitted and excluded subtrees, per the §6.1.4 state machine),
and chain-end matching against the system trust anchors. Validation failure
raises a `CertificateException`. Residual limits: a few `GeneralName` types are
not modeled, directory-name matching is byte-exact, and CRL revocation checking
is off by default.

## Limitations

- **No formal audit** — treat the profiles as defense-in-depth.
- **The `SecurityManager` is partial** — see [Security Overview](overview.md).
- **The egress policy guards the native connect paths**, not arbitrary
  user-supplied socket code that bypasses them; embedders wanting a stricter
  posture should install their own policy.
- **RSA is not side-channel-hardened** — see [Cryptography](cryptography.md).
