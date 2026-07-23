# HANG classification unconfirmed — possible host-contention artifacts, 9 classes

Not confirmed as fixture gaps, CratonVM bugs, or anything else — this doc
exists to flag that the evidence behind these 9 classes' current
classification is weak, not to claim a root cause.

## Why these are flagged

The 2026-07-24 HotSpot control pass (see
`docs/internal/fixed-suite-bugs/tomcat/16-full-suite-6shard-rerun-20260721.md`'s
"CORRECTED addendum") ran on the shared Azure host while `uptime`'s load
average spiked from ~70 to **148** from *other concurrent sessions* — not
this run. See `[[feedback_shared_host_multitenant_confound]]` (Claude
session memory) for the general pattern: at that level of contention, a
merely-slow-but-passing test can easily blow past a 300-second per-class
timeout with zero real defect involved, and this has produced confirmed
false-positive "HANG"/known-issues docs on this exact host before.

These 9 classes hit the 300s timeout under HotSpot during that overloaded
run. They may be genuine fixture gaps (some legitimately slow embedded-
server test), genuine CratonVM regressions (if CratonVM is faster or
happened to run at a quieter moment), or just victims of the same
contention — **not distinguishable from the data collected so far.**

## Affected classes

- `jakarta.el.TestCompositeELResolver`
- `jakarta.el.TestOptionalELResolverInJsp`
- `jakarta.servlet.TestSessionCookieConfig`
- `jakarta.servlet.jsp.TestPageContext`
- `jakarta.servlet.jsp.el.TestImportELResolver`
- `org.apache.catalina.authenticator.TestFormAuthenticatorA`
- `org.apache.catalina.authenticator.TestFormAuthenticatorB`
- `org.apache.catalina.authenticator.TestFormAuthenticatorC`
- `org.apache.tomcat.util.net.TestCustomSsl`

## What to do

Rerun exactly these 9 classes under HotSpot on a quiet host (`uptime` load
average comfortably under core count, no other concurrent
`cargo`/`cratonvm`/test sessions):

```sh
TC_ROOT=/data/data/apps/tomcat \
  bash apps/tomcat-suite-runner/run-tomcat-suite.sh hotspot 0 1 quiet-recheck \
  <(printf '%s\n' \
    jakarta.el.TestCompositeELResolver \
    jakarta.el.TestOptionalELResolverInJsp \
    jakarta.servlet.TestSessionCookieConfig \
    jakarta.servlet.jsp.TestPageContext \
    jakarta.servlet.jsp.el.TestImportELResolver \
    org.apache.catalina.authenticator.TestFormAuthenticatorA \
    org.apache.catalina.authenticator.TestFormAuthenticatorB \
    org.apache.catalina.authenticator.TestFormAuthenticatorC \
    org.apache.tomcat.util.net.TestCustomSsl)
```

If they PASS cleanly under HotSpot on a quiet host, move them into the
confirmed-regressions list (compare against the corresponding CratonVM
result in `apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md` — several of
these already show a CratonVM `HANG` too, e.g.
`TestFormAuthenticatorA`/`B`, which would then need their own individual
triage). If they still HANG on a quiet host, move them to a "confirmed slow
test, needs a longer per-class timeout" doc instead — 300s may simply be too
tight for these specific classes even under normal load.
