# H2 console autocomplete (`autoCompleteList.do`) returns empty body — root-caused, NOT a discrete bug (same family as `TestBnf`)

## Status
**CLOSED — root-caused as a performance-margin issue, not a discrete
CratonVM defect** (follow-up session, 2026-07-22, same day as the original
finding). The original `RuleElement.link` NPE hypothesis below was a **red
herring from an incomplete isolated repro** (`docs-known-issue-doc-
hypothesis-can-be-wrong-not-just-stale` applies exactly here) — the isolated
repro called `Bnf.getInstance(null)` then `getNextTokenList(...)` directly,
skipping the required `linkStatements()` call that every real caller
(`WebSession.loadBnf()` included) makes first. With `linkStatements()`
called (verified via a faithful probe replicating `WebSession.loadBnf()`
exactly, including all 7 `updateTopic()` calls and a real, `readContents()`-
populated `DbContents` against a live H2 connection — `BnfProbe4.java`,
worktree `/data/wt-h2-testupgrade-20260722`), the `RuleElement.link` NPE
**does not reproduce at all**.

The REAL mechanism behind `TestWeb.testWebApp()`'s empty-body symptom:
`getNextTokenList("select 'abc")` (an autocomplete query with an
**unclosed string literal**) returns an empty result (`size=0`) instead of
suggesting the closing `'`. Root cause: `org.h2.bnf.Sentence`'s hardcoded
`MAX_PROCESSING_TIME = 100` (milliseconds) wall-clock budget for the BNF
grammar-tree walk — **the exact same mechanism already characterized for
`org.h2.test.unit.TestBnf` in `bug-h2-suite-residual-fail-triage.md`**.
Confirmed directly: temporarily widening the budget to 30000ms in a scratch
rebuild makes `getNextTokenList("select 'abc")` correctly return
`{1#anything=Hello World, 1#'='}` (the expected closing-quote suggestion) —
i.e. CratonVM's interpreter is slow enough that the 100ms budget expires
before the grammar walk reaches the string-literal (`RuleFixed
.ANY_EXCEPT_SINGLE_QUOTE`) branch for this specific query shape. Not a
dispatch/NPE/loader bug of any kind — a genuine interpreter-throughput gap,
same category as `TestFileLock`/`TestTransaction`/`TestBnf`. No targeted fix
attempted (would require broader interpreter throughput work, matching
those items' existing characterization).

`WebApp.autoCompleteList()` swallows the resulting empty-result case
silently by design (`try { ... session.put("autoCompleteList", result); }
catch (Throwable e) { server.traceError(e); }` — an empty `result` string is
not even an exception here, just the correct-per-input output of a grammar
walk that ran out of budget), which is why the HTTP response comes back as
a well-formed `200 OK` with `Content-Length: 0` rather than a visible
error — exactly the originally-reported symptom, now correctly attributed.

**Original (incorrect) hypothesis kept below for context, per this
project's convention of preserving investigation history rather than
deleting a superseded hypothesis.**

## Severity
**LOW-MEDIUM** — cosmetic/feature gap in the H2 Console's SQL-autocomplete
UI, not a data-correctness or crash bug. Confirmed server-side and
independent of networking/HTTP client behavior (see "Confirmed unrelated to
HTTP connection pooling" below).

## Symptom
`org.h2.test.server.TestWeb.testWebApp()`:
```java
result = client.get(url, "autoCompleteList.do?query=select 'abc");
assertContains(StringUtils.urlDecode(result), "'"); // FAILS: result is ""
```
`TestWeb.java:388`. The HTTP response itself is well-formed
(`HTTP/1.1 200 OK`, `Content-Length: 0`) — the body is genuinely empty, not
a transport-level truncation.

## Root cause
`WebApp.autoCompleteList()` (`src/main/org/h2/server/web/WebApp.java:308`)
calls `session.getBnf()`; if that returns `null`, it returns
`"autoCompleteList.jsp"` without ever calling
`session.put("autoCompleteList", result)` — so the JSP template
(`${autoCompleteList}`) interpolates to an empty string.

`session.getBnf()` returns `null` because `WebSession.loadBnf()`
(`src/main/org/h2/server/web/WebSession.java:119`) throws and the exception
is silently swallowed:
```java
void loadBnf() {
    try {
        Bnf newBnf = Bnf.getInstance(null);
        ...
        bnf = newBnf;
    } catch (Exception e) {
        // ok we don't have the bnf
        server.traceError(e);   // no-op unless server tracing is enabled
    }
}
```

Isolated repro (bypasses the whole H2 Console/WebSession machinery):
```java
Bnf bnf = Bnf.getInstance(null); // throws under CratonVM
```
Actual exception:
```
java.lang.NullPointerException: Cannot invoke
"org.h2.bnf.Rule.autoComplete(org.h2.bnf.Sentence)" because "this.link" is null
	at org.h2.bnf.RuleElement.autoComplete(RuleElement.java:77)
	at org.h2.bnf.RuleList.autoComplete(RuleList.java:66)
	at org.h2.bnf.Bnf.getNextTokenList(Bnf.java:363)
```
(Hit via `bnf.getNextTokenList("select ")` right after construction, in the
isolated repro — `Bnf.getInstance` itself returns without throwing; the NPE
surfaces on first use of the grammar, which is also what
`WebSession.loadBnf()`'s try block ultimately does via
`newBnf.linkStatements()` and friends before assigning `bnf = newBnf`, so
the exception is caught there instead of surfacing to the caller.)

**The `help.csv` grammar resource itself loads fine** — confirmed via
`Utils.getResource("/org/h2/res/help.csv")` returning 270598 bytes, not
`null`. This rules out a classpath/resource-loading gap as the cause. The
NPE is specifically a `RuleElement.link` field that ends up `null` when
`RuleElement.autoComplete`/`RuleList.autoComplete` expects it populated —
i.e. something in the BNF grammar's rule-linking pass
(`Bnf`/`RuleList`/`RuleElement` construction, likely triggered from
`Bnf.getInstance`'s internal parse-and-link step, or from
`WebSession.loadBnf()`'s own `newBnf.linkStatements()` /
`newBnf.updateTopic(...)` calls layered on top) isn't wiring up a rule
reference under CratonVM the way it does on real JDK. Not yet
investigated: which specific `updateTopic`/`linkStatements` call leaves a
`RuleElement.link` unset, and whether that's a CratonVM field-write/dispatch
gap (e.g. a putfield through an interface-typed reference, a HashMap
iteration-order dependency, or a reflection-based construction path H2's
BNF loader uses) versus a genuine upstream H2 bug that real JDK happens to
tolerate differently. `docs-known-issue-doc-hypothesis-can-be-wrong-not-just-stale`
applies — the above is the confirmed symptom and stack trace, not yet a
confirmed CratonVM-vs-H2 attribution.

## Confirmed unrelated to HTTP connection pooling
Reproduced with a direct `curl` request against a running `WebServer`
instance — no `HttpURLConnection`/connection-pooling code involved at all —
on the very first `autoCompleteList.do` request for a brand-new session
(only `GET /` and `GET /login.jsp` preceded it). Same empty
`Content-Length: 0` response. This is a server-side (in-JVM) bug reachable
regardless of how the HTTP request arrives.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.server.TestWeb   # testWebApp() fails at TestWeb.java:388
```
Isolated Bnf-only repro (no HTTP/server involved):
```java
package org.h2.test.server; // or anywhere Bnf/RuleElement are visible

public class BnfProbe {
    public static void main(String[] args) throws Exception {
        org.h2.bnf.Bnf bnf = org.h2.bnf.Bnf.getInstance(null);
        System.out.println(bnf.getNextTokenList("select ").size()); // throws NPE
    }
}
```

## Related
- `docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-httpurlconnection-no-keepalive-pooling-FIXED.md` — the fix that exposed this.
- `apps/h2database/h2/src/main/org/h2/bnf/RuleElement.java`, `RuleList.java`, `Bnf.java` — where the NPE originates.
- `apps/h2database/h2/src/main/org/h2/server/web/WebSession.java:119` (`loadBnf`) — where it's silently swallowed.
