# `SslConnectorCustomizerTests.sslEnabledMultipleProtocolsConfiguration` — TLS 1.1 unavailable (rustls backend limitation)

**Status: OPEN — found 2026-07-20**, while validating the fix for
`rustls-cbc-cipher-suites-not-supported.md`.

## Symptom

`module/spring-boot-tomcat`'s `SslConnectorCustomizerTests.sslEnabledMultipleProtocolsConfiguration`
sets `ssl.setEnabledProtocols(new String[] { "TLSv1.1", "TLSv1.2" })` and
asserts `sslHostConfig.getEnabledProtocols()` contains exactly `["TLSv1.1",
"TLSv1.2"]`. After the CBC cipher-suite fix landed (see the sibling doc),
this test still fails, now with a different assertion:

```
Expecting actual:
  ["TLSv1.2"]
to contain exactly in any order:
  ["TLSv1.1", "TLSv1.2"]
but could not find the following elements:
  ["TLSv1.1"]
```

`sslEnabledProtocolsConfiguration` (the other test the CBC doc tracked,
requesting only `TLSv1.2`) now passes cleanly — this is a distinct,
previously-masked gap, not a residual of the CBC fix.

## Root cause

rustls has never implemented TLS 1.0 or TLS 1.1 in any version, in any
crypto provider — only TLS 1.2 and TLS 1.3 (`native-builtins/vendor/
rustls-cbc/src/versions.rs`'s `ALL_VERSIONS` only lists those two). This is
the same category of permanent, deliberate upstream limitation as the CBC
cipher-suite gap and the classic-DHE gap already tracked in this
directory: TLS 1.0/1.1 are actively deprecated and disabled by every major
TLS library (rustls, and increasingly OpenSSL/BoringSSL/JSSE too) because
they're cryptographically broken (BEAST, POODLE-adjacent CBC padding
issues, weak/removed MD5-SHA1 PRF). rustls's maintainers made an explicit,
permanent choice never to implement them.

Since CratonVM's cipher-suite reporting (`t27_tls.rs`,
`net_phase_e.rs`, `phases_late.rs`, `tls.rs`) can only ever report
`getEnabledProtocols()` as a subset of what the underlying rustls
`ServerConfig`/`ClientConfig` can actually negotiate, and TLS 1.1 can never
be one of those versions, `sslHostConfig.setProtocols("TLSv1.1+TLSv1.2")`
can only ever result in the effectively-negotiable subset (`TLSv1.2`) being
reported back, not the full requested set.

## Why this isn't trivially fixable

Implementing TLS 1.1 would mean reviving a deprecated, insecure protocol
version that the wider TLS ecosystem is actively removing, not adding.
Unlike CBC-mode cipher suites (still occasionally required for legacy
interop even though modern), TLS 1.0/1.1 have no comparable modern
justification — every major browser and TLS library has already disabled
them by default or removed them outright. Implementing them would be a
strictly negative security tradeoff with no offsetting interop benefit,
and is a much larger undertaking than the CBC fix (a full second protocol
version's handshake state machine and legacy PRF, not just a record-layer
cipher). Filed as a known, permanent, environment-level limitation rather
than a bug to fix, same as the CBC and DHE gaps.

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | `sslEnabledMultipleProtocolsConfiguration` |

Not pursued further without explicit user sign-off given the security
tradeoff involved (reviving a deprecated, actively-being-removed protocol
version); flagged back rather than silently attempted, same posture as the
CBC and DHE docs.
