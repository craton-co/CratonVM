# `java.util.Properties.clone()` NPEs — breaks Testcontainers' Docker client init, and likely anything else that clones a `Properties`/`Hashtable`

## Status
New finding, 2026-08-29, confirmed CratonVM-only via HotSpot A/B on the
identical class/method, same host, same moment (Docker healthy throughout —
`docker ps` clean, a plain `postgres:16` container up 12h+). Not yet
root-caused inside CratonVM's own `Hashtable`/`Properties` implementation.

## Symptom

Surfaced while rerunning `apps/hibernate-reactive-suite-runner`'s suite: 138
of 191 classes in one batch failed identically. Root exception:

```
Caused by: java.lang.NullPointerException
	at java.util.Properties.clone(Properties.java:1526)
	at org.testcontainers.shaded.com.github.dockerjava.core.DefaultDockerClientConfig.createDefaultConfigBuilder(DefaultDockerClientConfig.java:222)
	at org.testcontainers.dockerclient.TestcontainersHostPropertyClientProviderStrategy.<init>(TestcontainersHostPropertyClientProviderStrategy.java:23)
	at java.util.ServiceLoader$ProviderImpl.newInstance(ServiceLoader.java:707)
```

Testcontainers' `TestcontainersHostPropertyClientProviderStrategy` constructor
clones `System.getProperties()` (standard Java pattern to snapshot properties
before mutating a local copy) — `((Properties) System.getProperties().clone())`
-shaped — and the clone itself throws `NullPointerException` inside
`Properties.clone` → `Hashtable.clone`. `ServiceLoader` wraps that in a
`ServiceConfigurationError`, which callers were not expecting from a provider
that should simply fail over to the next strategy, so it propagates as a hard
test failure instead of a graceful fallback.

## Confirmed CratonVM-only

Two-class smoke batch (`CompletionStagesTest`, `HQLQueryTest`), same binary
family, same moment:

- **CratonVM** (`target-gpu/release/cratonvm.exe`): `HQLQueryTest` FAILs with
  the above trace, both test methods, reproduced twice (a 191-class batch and
  an isolated 2-class rerun).
- **HotSpot** (JDK 25.0.3.9): `PASS=2` — both classes clean, including
  `HQLQueryTest`'s live-Postgres-via-Testcontainers path.

## Why this matters beyond Testcontainers

`Properties`/`Hashtable.clone()` is an extremely common pattern (snapshotting
config, defensive-copying system properties before mutation, cache
invalidation). Testcontainers happened to be the app that tripped it here
because it clones `System.getProperties()` on every Docker-client
provider-strategy attempt, but nothing about the failure is
Testcontainers-specific — it's in `java.util.Properties.clone`/
`java.util.Hashtable.clone` itself.

## Not yet done
- Root cause inside CratonVM's `Hashtable`/`Properties` clone implementation
  — which field is null that HotSpot's isn't. Given this session's repeated
  pattern of primitive/reference slot confusion surfacing as unexplained NPEs
  (`cratonvm::gc::guard` W7-84/G30 warnings seen constantly in raw logs across
  unrelated suites this session), that family is a plausible starting
  hypothesis, not a confirmed cause.
- A minimal standalone repro (`new Properties(); p.clone()` vs
  `(Properties) System.getProperties().clone()`) to isolate whether this needs
  a populated/large properties table, specific key/value types, or reproduces
  on an empty one.
- Whether this is a regression (worked before some recent `dev` change) or
  has been broken for a while — the earlier 57-class hibernate-reactive batch
  from earlier the same day did not hit this signature, but that batch may
  simply not have exercised any class needing Testcontainers' Docker-client
  init path yet.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
cratonvm.exe --java-home <jdk25> -cp <hibernate-reactive classpath> \
  CratonRunner org.hibernate.reactive.HQLQueryTest
# HotSpot passes the identical class/classpath cleanly.
```
