# HANG classification unconfirmed — possible host-contention artifacts, 9 classes — ✅ RESOLVED (all 9 fixed and verified passing)

**Status: CLOSED.** Reran all 9 classes under both HotSpot and CratonVM on
the Azure host per this doc's own "What to do" section. The HANG
classification was indeed a pure host-contention artifact (confirmed below) —
but once contention was ruled out, all 9 classes turned out to be genuine
CratonVM regressions (not fixture gaps, not slow-but-passing tests). Both
underlying bugs are now root-caused and fixed on branch
`fix/hang-classification-quiet-recheck-20260723`; a final verification run
on a genuinely quiet host (`uptime` load average 6-13 on 16 cores) with the
standard 300s per-class timeout shows all 9 classes passing cleanly on
CratonVM, matching HotSpot.

## Original flag (kept for history)

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
run:

- `jakarta.el.TestCompositeELResolver`
- `jakarta.el.TestOptionalELResolverInJsp`
- `jakarta.servlet.TestSessionCookieConfig`
- `jakarta.servlet.jsp.TestPageContext`
- `jakarta.servlet.jsp.el.TestImportELResolver`
- `org.apache.catalina.authenticator.TestFormAuthenticatorA`
- `org.apache.catalina.authenticator.TestFormAuthenticatorB`
- `org.apache.catalina.authenticator.TestFormAuthenticatorC`
- `org.apache.tomcat.util.net.TestCustomSsl`

## What the rerun found

**First pass** (worktree `/data/wt-hangclass-quiet-recheck-20260723`,
branch `fix/hang-classification-quiet-recheck-20260723`, based on
`origin/dev` @ `621eeb15e`): ran the exact 9-class list under HotSpot (host
load ~15-27) and CratonVM (unfortunately during a fresh, unrelated load
spike to 60-134 from other concurrent sessions on this shared host — an
important caveat, see below) with a generously inflated `TIMEOUT_SEC=1200`
(vs. the usual 300s) specifically so that contention at that level couldn't
itself produce a false HANG and mask the real signal.

**Result: HotSpot PASSED ALL 9 classes cleanly** (2-236s each, no HANGs at
all) — confirming the original HANG classification was indeed a pure
contention artifact, exactly as this doc suspected. (A second, independent
rerun of the CratonVM side while the load spike was still ongoing,
`quiet-recheck-craton-v2-20260723`, reproduced the identical FAIL pattern
for all 9 classes at different individual timings, which further confirmed
these were deterministic FAILs, not contention-induced flakes — see next
section.)

**CratonVM FAILED all 9 classes** — but as real, fast, deterministic FAILs
(11-193s each), never anywhere near the inflated 1200s timeout. Two
completely independent root causes, both now fixed:

### Root cause 1 (8 of 9 classes): `Hashtable.size()` field-index collision breaking ecj JSP compilation

`jakarta.el.TestCompositeELResolver`, `TestOptionalELResolverInJsp`,
`jakarta.servlet.TestSessionCookieConfig`, `jakarta.servlet.jsp.TestPageContext`,
`jakarta.servlet.jsp.el.TestImportELResolver`, and
`TestFormAuthenticatorA`/`B`/`C` all failed with the identical signature:

```
ERROR [...jsp] Servlet.service() ... threw exception [org.apache.jasper.JasperException: Unable to compile class for JSP]
with root cause (java/lang/NullPointerException: Cannot assign field "referenceBinding" because "classFile" is null)
```

This is the exact ecj (`CompilationResult.getClassFiles()`) NPE already
root-caused and fixed by a concurrent session's investigation into
`../../../known-issues/tomcat/regressions-revealed-by-fixture-completion-20260723.md`
— see `[[reference_hashtable_size_field_collision_modcount]]` and commits
`744401af4` ("fix(vm): force native Hashtable/HashMap put/get/size on ALL
dispatch paths") and `b854dc01f` ("fix(collections): resolve map size field
against receiver's own class, not hardcoded HashMap") on branch
`fix/tomcat-fixture-regressions-20260723`. Cherry-picked both commits onto
this branch (commit `a517d31b4`) — confirmed this fixes all 8 of these
classes, exactly matching that investigation's own prediction ("very likely
the root cause of MULTIPLE of the 9 doc regressions at once").

### Root cause 2 (1 of 9 classes, new): `HttpsURLConnection.setDefaultSSLSocketFactory` didn't unwrap a delegating `SSLSocketFactory`

`org.apache.tomcat.util.net.TestCustomSsl` failed with
`SSLHandshakeException: ... UnknownIssuer` — the in-process client rejected
the embedded server's self-signed test certificate even though
`TesterSupport.configureClientSsl()` explicitly installs a `TrustManager`
that trusts it.

Root-caused via `CRATONVM_DBG_TLS_AUTH=1` tracing:
`HttpsURLConnection.setDefaultSSLSocketFactory`'s native handler
(`native-builtins/src/t27_tls.rs`) read field 0 of the passed-in factory and
assumed it was always our own synthetic `SSLSocketFactory` carrier (field 0
= the `SSLContext`, true for the direct return of `SSLContext
.getSocketFactory()`). Tomcat's own `TesterSupport.ClientSSLSocketFactory`
test helper wraps that carrier in a **real bytecode subclass** whose own
field 0 is its `delegate` field — itself another `SSLSocketFactory`, one hop
short of the actual `SSLContext`. Every `ctx_obj_key`-keyed lookup (trust
roots, key managers, identity) then silently missed against this unrelated
object's identity, so the client silently fell back to the platform default
trust store.

Two follow-on findings while fixing this:
- `class_name_of_id` misreports the abstract `SSLSocketFactory` carrier
  class as `java/lang/Object` (a name-string check based on it silently
  found nothing at every recursion level) — matching the exact caveat
  `alloc_concurrent_synthetic` documents about "interface-like synthetic
  classes." Fixed by comparing `ClassId` equality (one `class_id_by_name`
  lookup) instead.
- `class_num_total_fields` also misreports the SAME carrier's field count as
  0, even though it was allocated with (and safely holds, per `get_field`'s
  own bounds-checking contract) 1 real slot — the identical metadata gap on
  the *read* side that `alloc_concurrent_synthetic` already works around on
  the *allocation* side via `num_fields.max(real)`. Fixed by scanning a
  small fixed field-index range (0..8) instead of trusting the reported
  field count — safe because `get_field` is required to bounds-check and
  fail safe on any truly out-of-range index.

Fix: `resolve_sslcontext_from_factory` (new function in
`register_https_url_connection`, `native-builtins/src/t27_tls.rs`) does a
small bounded breadth-first search over reachable Object-typed fields,
matching by `ClassId`, to find the actual `SSLContext` through any number of
wrapper layers — not just our own direct carrier.

## Final verification (quiet host, standard 300s timeout)

With both fixes applied (branch `fix/hang-classification-quiet-recheck-20260723`,
binary `cratonvm-hangclass-quiet-recheck-20260723`), reran the same 9-class
list on the SAME host once it settled to a genuinely quiet state (`uptime`
load average 6.06, 8.96, 13.01 — comfortably under the 16-core count) with
the standard `TIMEOUT_SEC=300`:

| Class | HotSpot | CratonVM (before fix) | CratonVM (after fix) |
|---|---|---|---|
| `jakarta.el.TestCompositeELResolver` | PASS | FAIL | **PASS** |
| `jakarta.el.TestOptionalELResolverInJsp` | PASS | FAIL | **PASS** |
| `jakarta.servlet.TestSessionCookieConfig` | PASS | FAIL | **PASS** |
| `jakarta.servlet.jsp.TestPageContext` | PASS | FAIL | **PASS** |
| `jakarta.servlet.jsp.el.TestImportELResolver` | PASS | FAIL | **PASS** |
| `org.apache.catalina.authenticator.TestFormAuthenticatorA` | PASS | FAIL | **PASS** |
| `org.apache.catalina.authenticator.TestFormAuthenticatorB` | PASS | FAIL | **PASS** |
| `org.apache.catalina.authenticator.TestFormAuthenticatorC` | PASS | FAIL | **PASS** |
| `org.apache.tomcat.util.net.TestCustomSsl` | PASS | FAIL | **PASS** |

**9/9 clean PASS on CratonVM**, matching HotSpot exactly. No HANGs
observed anywhere in this investigation once the CWD bug (see below) and
the two root causes above were addressed — the original 300s-timeout HANG
classification was 100% a host-contention/harness artifact, not a real
slow-test or genuine hang.

## Incidental fixture-runner bug found and fixed along the way

The *very first* rerun attempt (before discovering the two real bugs above)
was run from the shared main worktree's stale copy of
`apps/tomcat-suite-runner/run-tomcat-suite.sh`, which predates the
`cd "$TC_ROOT"` CWD fix (see group 16's "CORRECTED addendum, 2026-07-24").
This produced a completely different, misleading failure signature (fast
`NoSuchFileException`s resolving `test/webapp` against the wrong directory)
for all 9 classes under HotSpot — a pure harness-invocation mistake, not a
new bug. Re-running from an up-to-date worktree's copy of the script
(confirmed via `grep 'cd "\$TC_ROOT"'`) immediately resolved it. Recorded
here as a reminder: **always verify which copy of the suite runner script
you're invoking** on this heavily-forked shared host — see
`[[project_tomcat_fixture_regressions_20260723]]`'s own identical warning.

## Commits (branch `fix/hang-classification-quiet-recheck-20260723`)

- `a517d31b4` — cherry-picked Hashtable/HashMap dispatch + size-field-collision fixes (root cause 1)
- `03c03c610`, `7c8df9379`, `857c4af10`, `644cc0fe0` — `resolve_sslcontext_from_factory` (root cause 2), iterated through 3 revisions as tracing revealed the `class_name_of_id`/`class_num_total_fields` metadata gaps above
