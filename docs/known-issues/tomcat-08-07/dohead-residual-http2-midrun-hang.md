# DoHead family — 2 classes hang mid-run partway through the HTTP/2 parameterizations

**Status:** OPEN, low confidence (possible host-load artifact). **Severity:**
medium. **HotSpot:** not yet checked. **Related:**
[dohead-jit-heap-corruption-register-invisibility-FIXED.md](../../internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md)
(`docs/internal/fixed-suite-bugs/`) — that doc's extensive multi-run
validation already documents a small residual-flake rate (6 of 64 classes
at 287/288, 1 sporadic failure each) that this session's own fresh rerun
reproduced consistently (6 of 8 non-passing DoHead classes showed exactly
that 1-7-failures-of-288 pattern with the same
`LifecycleException: Failed to start component` sporadic signature — not
worth a separate doc, matches the already-documented and accepted residual
rate). **This doc covers only the 2 classes that didn't just flake a few
parameterizations but hung entirely** — a different, more severe shape not
covered by that doc's characterization.

## Summary

`jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1025ValidWrite513`
and `jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite512ValidWrite1`
both hit a 300s timeout with no completion, no crash, and no diagnostic
warning — the log simply stops after starting a specific
`testDoHeadHttp2[N]` parameterization:
```
INFO [...] Starting test case [testDoHeadHttp2[45: 0 false true 16 true 1,025 BUFFER 513 true]]
INFO [org.apache.coyote.http11.Http11NioProtocol] The [...] connector has been configured to support HTTP upgrade to [h2c]
INFO [org.apache.coyote.http11.Http11NioProtocol] Initializing ProtocolHandler [...]
INFO [org.apache.catalina.core.StandardService] Starting service [Tomcat]
INFO [org.apache.catalina.core.StandardEngine] Starting Servlet engine: [Apache Tomcat/12.0.0-M1-dev]
(nothing further for 300s)
```
Both classes reached a substantial fraction of their 288 parameterizations
before hanging (parameter index 45 and 53 respectively, both inside the
`testDoHeadHttp2` block which runs after all plain `testDoHead`
parameterizations) — so this isn't an immediate/deterministic failure,
more consistent with an intermittent stall under specific runtime
conditions (possibly host load: this box has had heavy concurrent build/
test activity all session).

Found via a fresh Windows full-suite rerun (dev commit `f23a3f42a`,
2026-07-14, real JDK, JIT on, 300s timeout,
`CRATONVM_REAL_NET_SOCKETS=1` etc. set).

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName doheadhang `
  -Start <idx> -Count 1 -TimeoutSec 600 -Parallel 1
# jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1025ValidWrite513
# jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite512ValidWrite1
```

## Recommendation

Re-run both classes in isolation on an idle host before investing further
— given this session's established pattern of shared-host contention
producing false-positive hangs, and that the related family doc already
found genuine residual issues are rare (1-2%) at this point, a full 300s+
stall (vs. a few-second flake) on 2 of 8 residual classes could well be
load-induced rather than a distinct new bug. If it reproduces cleanly on
an idle host, get a thread dump at the hang point (both stall right after
`StandardEngine` start, before the HTTP/2 client even connects) to see
whether this is the same "STW cross-thread JIT takeover" mechanism
documented elsewhere in this doc set, or something specific to this
family's connector setup under HTTP/2 upgrade configuration.
