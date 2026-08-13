# PostgreSQL SCRAM-SHA-256 auth fails — `PBKDF2WithHmacSHA384 SecretKeyFactory not available` — FIXED

**Status:** FIXED (2026-08-12). Filed 2026-08-07 while setting up a
real-Postgres run of the Hibernate suite
(`apps/hib-suite-runner/runs/pgsql-fullsuite-20260807/`), worked around
operationally (see below) so the suite could run.

Closed in two parts, neither of which was visible from this filing alone:

* **The headline gap** — `SecretKeyFactory` not registering
  `PBKDF2WithHmacSHA384`/`SHA512` — was fixed by `e0598dc55`. The doc's open
  question ("is `PBKDF2WithHmacSHA256` *also* missing?") is answered: it was
  present all along; only the 384/512 probe entries were absent, which was
  enough to kill `ScramMechanism.<clinit>` outright.
* **What remained after that** was a different defect entirely, found on
  2026-08-12 while chasing the Vert.x reactive client's SCRAM failure:
  `javax/crypto/Mac.doFinal([BI)V` was never registered, so
  `com.ongres.scram`'s PBKDF2 loop died on its second iteration. pgjdbc shades
  that same library, so both SCRAM clients were blocked by it. See
  `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`.

Also fixed alongside: `SecretKeyFactory.getProvider()` threw
`NullPointerException: Cannot enter synchronized block because "this.lock" is
null` on a factory that derived keys correctly, and
`generateSecret(...).getAlgorithm()` answered the bare `PBKDF2` where HotSpot
answers the full `PBKDF2WithHmacSHA256`.

**Verified 2026-08-12** against a `postgres:18.4` container with
`--auth-host=scram-sha-256` (`pg_hba.conf` confirmed
`host all all all scram-sha-256`): pgjdbc's `DriverManager.getConnection`
completes the full
`SASLInitialResponse` → `SASLContinue` → `SASLResponse` → `SASLFinal` →
`AuthenticationOk` exchange and runs a query, 3/3 runs. The MD5 downgrade
below is no longer needed.

## Symptom

Any JDBC connection to a Postgres server using its default `scram-sha-256`
authentication method fails during the SCRAM mechanism-negotiation step,
before any query runs:

```
java.lang.SecurityException: PBKDF2WithHmacSHA384 SecretKeyFactory not available
	at org/postgresql/shaded/com/ongres/scram/common/ScramMechanism.isAlgorithmSupported(ScramMechanism.java:265)
	at org/postgresql/shaded/com/ongres/scram/common/ScramMechanism.<clinit>(ScramMechanism.java:89)
	at org/postgresql/core/v3/ScramAuthenticator.initializeScramClient(ScramAuthenticator.java:63)
	at org/postgresql/core/v3/ScramAuthenticator.<init>(ScramAuthenticator.java:49)
	at org/postgresql/core/v3/ConnectionFactoryImpl.lambda$doAuthentication$5(ConnectionFactoryImpl.java:1019)
	at org/postgresql/core/v3/AuthenticationPluginManager.withPassword(AuthenticationPluginManager.java:82)
	at org/postgresql/core/v3/ConnectionFactoryImpl.doAuthentication(ConnectionFactoryImpl.java:1006)
	...
	at org/postgresql/Driver.connect(Driver.java:298)
```

CratonVM's `SecretKeyFactory` provider does not register
`PBKDF2WithHmacSHA384` (SCRAM-SHA-256's underlying PBKDF2 uses SHA-256 by
default per RFC 5802, but the pgjdbc-shaded `ongres-scram` library also
probes SHA-384/SHA-512 variants at class-init time for its mechanism
registry, and `ScramMechanism.<clinit>` fails hard the first time
`isAlgorithmSupported` throws rather than catching per-algorithm — so this
blocks *all* SCRAM mechanisms, not just SHA-384 specifically).

Not yet checked: whether `PBKDF2WithHmacSHA256` (the one SCRAM-SHA-256
itself actually needs) is *also* missing, or whether only the SHA-384/512
probe entries are — the static initializer dies on the first unsupported
algorithm in whatever order it's probed, so seeing the SHA-384 error doesn't
by itself prove SHA-256 support is present. Worth checking directly:
`SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256")` on this VM.

## Workaround used to unblock the suite run

Not a fix — routes around the gap so real-Postgres testing could proceed.
Downgraded the test user's stored credential from a SCRAM verifier to a
plain MD5 hash, and the server's catch-all `pg_hba.conf` rule from
`scram-sha-256` to `md5` (both required — changing only the hba rule is not
enough; the *stored* credential is still SCRAM-format from container init
and Postgres cannot downgrade a stored SCRAM verifier to MD5 auth on the
fly):

```sql
SET password_encryption = 'md5';
ALTER USER hibernate_orm_test WITH PASSWORD 'hibernate_orm_test';
```

then edited `$PGDATA/pg_hba.conf`'s `host all all all scram-sha-256` line to
`... md5` and `SELECT pg_reload_conf();`. MySQL 8.0/9.x's default
`caching_sha2_password` plugin has a similar RSA-key-exchange-over-a-brief-
secure-channel requirement and hit an analogous (but distinct — TLS-layer,
not JCA-layer) gap; see the MySQL-side doc from the same investigation
session if filed.

Confirmed after the workaround: `EntityManagerTest` passes cleanly
(`found=16 ok=16 failed=0`) against real Postgres 18.

## Impact if unfixed

Any future real-Postgres testing needs this same manual downgrade —
MD5 password auth is itself deprecated in current Postgres and the upstream
`docker_db.sh`-provisioned container defaults to `scram-sha-256`, so this
isn't a one-time setup quirk, it recurs on every fresh container. More
importantly, it means CratonVM cannot authenticate to *any* Postgres
instance still requiring SCRAM auth in more security-conscious deployments
(scram-sha-256 is the Postgres default since v10 specifically because MD5 is
considered weak) — this is a real production-relevant gap, not just a local
test-setup inconvenience.

## Repro

```
cd apps/hib-suite-runner
# with a Postgres server using its default scram-sha-256 catch-all auth rule:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "<jdk25>" --Xmx 1500m \
  -Dhibernate.connection.url="jdbc:postgresql://localhost/hibernate_orm_test_1?preparedStatementCacheQueries=0" \
  @<pgsql-flavored-common.args> -Dcraton.batch=1 CratonRunner org.hibernate.orm.test.jpa.EntityManagerTest
```
Deterministic — every connection attempt against a scram-only server hits
this before any query runs, confirmed across the smoke test and the initial
failed 4-shard launch attempt.

## Answers to the two open questions

- **Was `PBKDF2WithHmacSHA256` also missing?** No. Measured 2026-08-12: all
  five of `PBKDF2WithHmacSHA1/224/256/384/512` resolve and derive the RFC 7677
  vector correctly (`pencil`, salt `W22ZaJ0SNY7soEsUEjb6gQ==`, i=4096). Only
  the 384/512 registrations had been absent — but because
  `ScramMechanism.<clinit>` fails hard on the FIRST unsupported probe rather
  than per-algorithm, two missing names blocked every mechanism, SHA-256
  included. That is the shape worth remembering: the algorithm named in the
  exception is the one that was probed first, not necessarily the one the
  caller needs.
- **Where is the provider implemented?** `phases_early::pbkdf2_get_instance` /
  `pbkdf2_generate_secret`, with the PRF table in
  `phases_early::pbkdf2_prf_code`. The guess of `native-builtins/src/jca/` was
  half right: `jca::cipher` carries the *registration* that is live in
  real-JDK mode, while the phase-53 copy beside the implementation is reachable
  only from `register_synthetic_overrides` and so is `#[cfg(feature =
  "synthetic-jdk")]`. Registering a `SecretKeyFactory` method in the phase-53
  block ALONE leaves it inert on a default run — that is how
  `getProvider()` stayed broken through a first attempt at fixing it.

## Related

- `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` — the second half of
  this defect, and the reason SCRAM stayed broken after the SHA-384/512
  registrations landed.
