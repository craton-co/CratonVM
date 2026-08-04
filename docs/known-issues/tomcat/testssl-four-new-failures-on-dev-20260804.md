# `TestSsl` went 1/21 → 5/21 failing somewhere in 149 `dev` commits

| | |
|---|---|
| **Status** | OPEN — not bisected, reported on sight |
| **Severity** | high — four TLS tests that were green went red, including a webapp start failure |
| **HotSpot** | PASS (`OK (21 tests)`) |
| **CratonVM** | FAIL ×5 at `dev` `12b8cbdea`; FAIL ×1 at `dev` `48fba3a31` |
| **Discovered** | 2026-08-04, running the standing TLS regression batch after merging `dev` into `fix/tomcat-final-five-20260803` |

## Not mine, and here is why that is not an assumption

The batch runs two arms interleaved, alternating order. **Both arms are
post-merge** — one with the `date_format_fast` / field-descriptor-memo changes
and one without — and both report `Tests run: 21, Failures: 5` with the same
five names. The pre-merge binary (`dev` `48fba3a31`, built 2026-08-03) run on
the same fixture minutes later reports `Tests run: 21, Failures: 1`.

So the delta is `48fba3a31..12b8cbdea`, 149 commits, none of them mine.

## The five

| test | at `48fba3a31` | at `12b8cbdea` |
|---|---|---|
| `testSni[JSSE]` | PASS | **FAIL** |
| `testKeyPass[JSSE]` | PASS | **FAIL** |
| `testKeyPassFile[JSSE]` | PASS | **FAIL** |
| `testSSLSessionTracking[JSSE]` | PASS | **FAIL** |
| `testClientInitiatedRenegotiation[JSSE]` | FAIL | FAIL (pre-existing, by design — see `testssl-client-initiated-renegotiation-FIXED.md`) |

`testSni` is the loudest:

```
org.apache.catalina.LifecycleException: A child container failed during start
    at org.apache.catalina.core.ContainerBase.startInternal(ContainerBase.java:751)
    at org.apache.catalina.core.StandardEngine.startInternal(StandardEngine.java:201)
    at org.apache.catalina.core.StandardService.startInternal(StandardService.java:433)
```

i.e. the embedded server does not start at all, which is a different class of
failure from the other three (`testKeyPass`/`testKeyPassFile` are bare
`assertTrue` at `TestSsl.java:270`, `testSSLSessionTracking` likewise). A
single cause covering all four is plausible but not established.

## Reproduction

```bash
/data/tcone.sh craton org.apache.tomcat.util.net.TestSsl <log> <cratonvm>
```

~113 s per run on the Azure Linux fixture. The class is otherwise stable —
both arms produced byte-identical failure sets across the batch.

## Suggested next step

Bisect `48fba3a31..12b8cbdea` on `TestSsl` alone. The range is large but the
class runs in under two minutes and the signal is deterministic (5/5 in every
run so far), so a `git bisect run` over ~8 builds is the cheap path. Start with
the commits touching TLS, keystores and `SSLHostConfig`; `testKeyPass` /
`testKeyPassFile` both concern the key password, which narrows the search a
long way if the same commit owns all four.

Do NOT read the surviving `testClientInitiatedRenegotiation` failure as part of
this — it is the by-design TLS 1.2 renegotiation gap and predates the range.
