# ES FAIL - RestClientBuilder SSL UnknownIssuer reopened

Status: OPEN

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Affected class:
- `client/rest org.elasticsearch.client.RestClientBuilderIntegTests`

CratonVM result:
- FAIL, 27.310s, 2 tests, 4 failures.

Primary signal:
```text
javax.net.ssl.SSLHandshakeException: rustls: invalid peer certificate: UnknownIssuer
```

Additional fallout:
```text
java.lang.AssertionError
com.carrotsearch.randomizedtesting.ThreadLeakError: 1 thread leaked from SUITE scope
Thread[id=3, name=idle-timeout-task, state=RUNNABLE, group=TGRP-RestClientBuilderIntegTests]
```

HotSpot control:
- Run: `esprobe-hotspot-restbuilder-20260710`
- Same class: PASS, 1.5s.

Evidence:
- Craton stdout: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.out.log`
- Craton stderr: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\client_rest.org.elasticsearch.client.RestClientBuilderIntegTests.err.log`
- HotSpot result: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-restbuilder-20260710\hotspot-restbuilder\results.tsv`

Relationship to older fixed note:
- `docs/internal/fixed-suite-bugs/elasticsearch-restclient-builder-ssl-handshake-residual.md` recorded an older RestClientBuilder TLS residual as fixed on 2026-07-04.
- This current-dev result reopens the class as an active known issue, but do not assume the old root cause. The older failure expected an SSL handshake exception and got connection closure; the current failure surfaces `SSLHandshakeException: UnknownIssuer` directly plus thread-leak fallout.

Interpretation:
- The current failing path is CratonVM-only and TLS/JSSE related.
- The first next step is a focused wire or `SSLEngine` probe to determine whether this is client trust validation, server identity propagation, or exception/catch shaping.
