# Spring Boot suite — known issues

Found triaging `FAIL`s from the first full Spring Boot 4.1.0-SNAPSHOT test
suite run against CratonVM (1975 classes; see
[[project_spring_boot_suite_runner_20260711]] and
`apps/spring-boot-suite-runner/RESULTS-20260711.md`). 508 classes FAILed;
these five clusters account for ~146 of them directly (many more indirectly,
via the "Unstarted application context"/"Failed to parse configuration
class" wrapper noise these root causes produce) — the rest is an
uncharacterized long tail of smaller/individual differences not yet
clustered.

| Doc | Classes | Severity | Status |
|---|---:|---|---|
| [`onclasscondition-npe-cast-to-string-array-cluster.md`](onclasscondition-npe-cast-to-string-array-cluster.md) | 75 (348 occurrences) | CRITICAL | OPEN, precisely pinned to `OnClassCondition.addAll` |
| [`disposablebeanadapter-invalid-destruction-signature-cluster.md`](disposablebeanadapter-invalid-destruction-signature-cluster.md) | 34 | HIGH | OPEN, Spring call site pinned |
| [`unsafe-memoryaccessoption-npe-cluster.md`](unsafe-memoryaccessoption-npe-cluster.md) | 26 | HIGH | OPEN, likely same family as an already-fixed bug |
| [`httpclient-builder-dead-registration-abstractmethoderror.md`](httpclient-builder-dead-registration-abstractmethoderror.md) | 13 | HIGH | OPEN, fully root-caused (dead `synthetic-jdk` registration) |
| [`zip-filedatablock-bulk-bytebuffer-put-aioobe.md`](zip-filedatablock-bulk-bytebuffer-put-aioobe.md) | 9 (whole `spring-boot-loader` module) | HIGH | OPEN, characterized |

## Methodology note (runner fix, not a bug)

The first run (`full-20260711b`) did **not** set `CRATONVM_REAL_NET_SOCKETS`,
so any test doing a real socket bind (e.g. `HazelcastAutoConfigurationClientTests`)
hit the already-known, already-fixed-behind-a-flag
`java.net.ServerSocket.socketLock` null bug (see `reference_server_socket_gap`
in memory) rather than a new issue. `run-spring-boot-suite.ps1` now sets
`CRATONVM_REAL_NET_SOCKETS`/`CRATONVM_REAL_AQS`/
`CRATONVM_DISABLE_DEFAULT_WATCHDOG`/`CRATONVM_ROOTSNAP_CACHE` for craton runs,
matching `tomcat-suite-runner`'s convention — a rerun with the fixed runner
would likely shift some FAIL/HANG counts.
