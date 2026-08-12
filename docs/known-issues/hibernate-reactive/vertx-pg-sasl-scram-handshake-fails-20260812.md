# Vert.x reactive Postgres client SASL/SCRAM handshake fails — `expected SASL response, got message type 88 (08P01)`

**Status:** OPEN (2026-08-12). Found on Azure host `azureuser@20.80.105.49`
while standing up a real-Postgres, 3-GC-variant (default/G1/ZGC) run of the
hibernate-reactive suite (206 classes, `-Ddb=PostgreSQL`, Testcontainers
`postgres:18.4`). This is the dominant blocker for that suite: it fails
essentially every DB-required class regardless of GC variant, and the run
was stopped early specifically because of it (see "Impact" below).

## Symptom

Any hibernate-reactive test that opens a session against the
Testcontainers-provisioned Postgres fails during connection setup:

```
io.vertx.pgclient.PgException: FATAL: expected SASL response, got message type 88 (08P01)
```

This is thrown from Vert.x's own reactive Postgres client
(`io.vertx.pgclient`), **not** JDBC/pgjdbc — hibernate-reactive bypasses
JDBC entirely in favor of Vert.x's non-blocking SQL client, which implements
its own SASL/SCRAM-SHA-256 exchange rather than delegating to pgjdbc's
shaded `ongres-scram`. Postgres's `08P01` (`protocol_violation`) response
means the server received something it couldn't parse as the expected next
SASL message — consistent with CratonVM producing a malformed
`SASLInitialResponse`/`SASLResponse` byte sequence during the handshake
(e.g. via a broken HMAC/PBKDF2/SecureRandom primitive corrupting the
computed proof or a length-prefix field), though the exact byte-level cause
has not yet been isolated.

## Root cause: not yet isolated, but likely related to a known JCA gap

`docs/known-issues/hibernate/postgres-scram-sha256-pbkdf2-hmacsha384-missing-20260807.md`
— a related, already-documented CratonVM gap — is CratonVM's `SecretKeyFactory` provider
not registering `PBKDF2WithHmacSHA384`, which breaks pgjdbc's SCRAM client
(a *different* library, `org.postgresql.shaded.com.ongres.scram`) with a
hard `SecurityException` at class-init. This failure is different in
shape (Postgres-side protocol violation, not a client-side JCA exception)
and comes from a different SCRAM implementation (Vert.x's own, not
ongres-scram), so it is **not confirmed to be the same bug** — but both are
"Postgres SCRAM auth broken on CratonVM," and if the two SCRAM
implementations end up calling the same underlying CratonVM crypto
primitive (HMAC-SHA256 / PBKDF2WithHmacSHA256 / SecureRandom), a single fix
there could resolve both. Worth checking whether Vert.x's SCRAM client hits
the same `PBKDF2WithHmacSHA256`/`Mac.getInstance("HmacSHA256")` code paths
before assuming they're independent.

## HotSpot-clean confirmation

Ran the identical class (`BatchFetchTest`), classpath, and args (with
`DOCKER_HOST=unix:///var/run/docker.sock` set, see Defect A below) under
stock HotSpot (JDK 25) against a freshly-provisioned Postgres 18
Testcontainer: **passed 3/3 (`ok=3 failed=0`)**. Under CratonVM, the same
class fails deterministically every time with the SASL error above. Genuine
CratonVM-specific defect, not an environment/harness issue.

## Impact

Blocks essentially the entire DB-required corpus (199 of 206 total classes
in `apps/hibernate-reactive-suite-runner/testlist.txt`) across all 3 GC
variants — this is a coverage-blocking defect, not a GC-variant-specific
one. Partial 3-way run before it was stopped (same wall-clock window, 1
shard each):

| variant | attempted | PASS | HANG | FAIL | NOTESTS |
|---|---|---|---|---|---|
| default (`-XX:+UseGenerationalGC`) | 28 | 3 | 9 | 14 | 1 |
| g1 (`-XX:+UseG1GC`) | 77 | 4 | 8 | 63 | 1 |
| zgc (`-XX:+UseZGC`) | 29 | 3 | 9 | 15 | 1 |

Every sampled FAIL carried the identical SASL signature above. (The G1
variant processing ~2.7x more classes in the same window is a real timing
divergence worth separate investigation, but is swamped here by defect B's
near-universal failure rate and shouldn't be read as a GC-driven
pass/fail-rate difference.)

The 3-way run was stopped deliberately after ~15-28 minutes rather than let
it continue for hours: the harness disables Testcontainers' Ryuk reaper
(`TESTCONTAINERS_RYUK_DISABLED=true`), and because this SASL failure
prevents clean session teardown, every failing class-fork leaked its
Postgres container — 120 orphaned containers observed after ~19 minutes
and climbing. Continuing would have mostly reproduced this same defect
~600 more times while pushing the Docker daemon/host toward instability.
All orphaned containers were cleaned up (`docker rm -f`) and host memory
confirmed fully recovered (14GB → 1.4GB used) before stopping.

**`HIB-CV-32`/`gen_heap::read_slot: corrupt Value cell` did NOT reproduce**
anywhere in the partial run's logs (checked explicitly) — the heap-
corruption regression fixed by the 08-10 `dev` merge remains fixed.

## Repro

```bash
# On azureuser@20.80.105.49, /data/cratonvm:
cd apps/hibernate-reactive-suite-runner
export DOCKER_HOST=unix:///var/run/docker.sock   # see Defect A below — required to reach this failure at all
./cratonvm-hibreactive-default-wrapper.sh --list <(echo BatchFetchTest) ...  # (exact CratonRunner invocation per common.args)
```
Deterministic — every DB-required class hits this on first session open.

## Related

- `docs/known-issues/hibernate/postgres-scram-sha256-pbkdf2-hmacsha384-missing-20260807.md`
  — a different SCRAM client (pgjdbc/ongres-scram), different symptom
  (client-side `SecurityException`, not a server-side protocol violation),
  possibly the same underlying crypto-primitive gap. Check before treating
  as unrelated.
- `docs/known-issues/hibernate-reactive/jna-native-clinit-nativeversion-npe-20260812.md`
  — a separate CratonVM defect hit earlier in the same investigation (JNA's
  `RootlessDockerClientProviderStrategy` probe), with a workaround
  (`DOCKER_HOST=unix:///var/run/docker.sock`) that was required just to
  reach this SASL failure in the first place.
