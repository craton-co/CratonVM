# H2 — CratonVM's rustls-based TLS server socket rejects a legacy DSA private key

## Status
**OPEN** — new finding, 2026-07-21. Related to, but a different symptom
from, a previously-documented (now-deleted, and since apparently partially
superseded by real JCA/PBES2 work — see below) H2 `TestNetUtils` gap.

## Severity
**LOW-MEDIUM** — legacy DSA TLS certificates are obsolete and disallowed by
modern TLS policy anyway; this only matters for old keystores/tests (like
H2's own bundled test keystore) that still carry one.

## Affected test class
`org.h2.test.unit.TestNetUtils` (`testFrequentConnections`) — PASSes on the
HotSpot JDK25 baseline. Also called out in the original
task background as one of the original 3 known FAILs, but the underlying
symptom is now different — see "Relationship to the prior doc" below.

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: IO Exception:
  "java.io.IOException: ServerConfig with_single_cert failed: unexpected error:
   failed to parse private key as RSA, ECDSA, or EdDSA;
   legacy DSA TLS fallback: key is not DSA"; "port: 9111 ssl: true"
	at org/h2/util/NetUtils.createServerSocket(NetUtils.java:172)
	at org/h2/security/CipherFactory.createServerSocket(CipherFactory.java:118)
```
CratonVM's TLS server-socket setup (backed by `rustls`, per the error
message's own wording) fails to parse the private key backing H2's bundled
test keystore/self-signed certificate when starting an SSL server socket.

## Root cause
CratonVM's TLS layer parses private keys as RSA, ECDSA, or EdDSA, with a
"legacy DSA" fallback path — and that fallback itself fails ("key is not
DSA") against the actual key material H2's test keystore uses. Not traced
further to the exact key format/algorithm H2's keystore actually contains
in this session (would need to `keytool -list -v` the specific `.p12`/`.jks`
file `CipherFactory`/`NetUtils` load) — but the error text makes clear this
is a `rustls`-side key-parsing/algorithm-support gap, not an H2 or
generic-JDK-API problem: real JDK's SunJSSE provider (used by the HotSpot
baseline) accepts whatever this keystore's key actually is.

## Relationship to the prior (deleted) doc
An earlier H2 investigation (`docs/known-issues/h2-suite-bugs/bug-h2-netutils-missing-pbe-algparams.md`,
deleted by the `b71e7402f` docs-cleanup commit) found `TestNetUtils` crashing
with a *different* symptom: `no AlgorithmParameters PBEWithHmacSHA256AndAES_256
implementation in any provider` (a PKCS#12/PBES2 keystore-loading gap). A
later commit (`dda6cb60c`, "fix(jca,keystore): PEM XDH/EdDSA/RSASSA-PSS
KeyFactory, PBES2, PKCS12 SHA-256 MAC + content decrypt") looks like it
addressed that specific gap — `TestNetUtils` no longer aborts with a VM
`runtime error` (unrecoverable crash) as the old doc described; it now fails
with a normal, catchable `IOException` further along in TLS setup, at
certificate/key parsing rather than keystore loading. Net effect: real
progress since the old doc, but a new (or previously-masked) gap is now the
first thing blocking this class.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestNetUtils
```
