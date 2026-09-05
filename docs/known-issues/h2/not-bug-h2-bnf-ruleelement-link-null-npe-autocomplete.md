# H2 console autocomplete (`autoCompleteList.do`) returns empty body — root-caused, NOT a discrete bug (same family as `TestBnf`)

## Status
**OPEN, partially fixed (follow-up session, 2026-07-23)** — the
performance-margin characterization below holds, and three real, distinct
interpreter throughput gaps that materially contribute to it have now been
found, fixed, and merged to `dev` (measured ~1.5-1.7x+ wall-clock
improvement on the isolated repro). **`TestWeb.testWebApp()` and
`TestBnf.testProcedures()` still fail** against H2's real 100ms
`Sentence.MAX_PROCESSING_TIME` budget — confirmed via direct test runs
against the fixed binary, not just the widened-budget repro. See "Follow-up
(2026-07-23): three real interpreter gaps found and fixed, partial
improvement" at the end of this doc for the full writeup, root-cause
precision, and what's left.

Originally: **CLOSED — root-caused as a performance-margin issue, not a discrete
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
`org.h2.test.unit.TestBnf` in `bug-h2-suite-residual-fail-triage-FIXED.md`**.
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
- The `HttpURLConnection` keep-alive-pooling fix (2026-07-xx, since archived) is what first let this NPE get exposed at all — it unblocked the suite run far enough to reach `TestWeb.testWebApp()`.
- `apps/h2database/h2/src/main/org/h2/bnf/RuleElement.java`, `RuleList.java`, `Bnf.java` — where the NPE originates.
- `apps/h2database/h2/src/main/org/h2/server/web/WebSession.java:119` (`loadBnf`) — where it's silently swallowed.

## Follow-up (2026-07-23): independent reconfirmation via new `apps/h2database-suite-runner`

A new Linux suite runner (`apps/h2database-suite-runner`) ran the full
218-class H2 suite against a `dev` binary built well after this doc's
CLOSED/reclassified finding, independently re-hitting `TestWeb.testWebApp()`
at the exact same assertion:

```
org.h2.test.server.TestWeb.testWebApp (TestWeb.java:388)
  AssertionError:  does not contain: '
```

Confirms this is still live on current `dev` (i.e. still the same
Sentence.MAX_PROCESSING_TIME budget-exhaustion mechanism this doc already
root-caused, not a regression or a different bug). No new investigation
done here — see `apps/h2database-suite-runner/RESULTS-20260723.md` for the
broader run this reconfirmation came from.

## Follow-up (2026-07-23): three real interpreter gaps found and fixed, partial improvement

Picked this up with the explicit goal of actually closing it, not just
re-confirming the characterization. Worktree `/data/wt-h2-bnf-perf-20260723`
on the Azure host, branch `fix/h2-bnf-perf-20260723`. Landed on `dev` as
commit `0227864d5` (merged forward via `6d0f865a0`).

### Precise root-cause, via instrumentation not guesswork

Built a faithful isolated repro (`BnfProbe5`/`BnfProbe6`, mirroring
`WebSession.loadBnf()` exactly) and temporarily widened
`Sentence.MAX_PROCESSING_TIME` to measure the *actual* time the
`"select 'abc"` query takes to complete correctly, rather than just
observing the 100ms timeout firing. Baseline: **~900ms** (a ~9x gap to the
100ms budget, not the "300-1000x" figure an earlier informal observation in
this doc's history had suggested — that earlier number was very likely
measured under heavy concurrent host load, not a property of the query
itself).

Added a temporary `Sentence.DBG_CALL_COUNT` counter (incremented in
`stopIfRequired()`, called once per `Rule.autoComplete()` invocation) and
found the query makes only **981 total calls** across the whole
`org.h2.bnf.Rule` family (`RuleList`/`RuleElement`/`RuleFixed`/
`RuleOptional`/`RuleRepeat`) — far too few for any single (class, method)
pair to cross the JIT's 500-invocation tier-up threshold (confirmed:
`CRATONVM_JIT_THRESHOLD=10` made no measurable difference). This is a
**cold-interpreter-throughput** problem, not a JIT-warmup problem.

Used the interpreter's existing `CRATONVM_DBG_INVOKESTATS` diagnostic
(`vm/src/runtime/interpreter.rs`'s `dbg_invoke_stats_record`) to isolate the
query's own contribution (by diffing `BnfProbe5`, setup+query, against
`BnfProbe6`, setup-only): the query's invoke-cache **miss rate was ~33%**
(vs ~11% ambient for ordinary setup/JDBC code), with the large majority of
misses cascading all the way to the most expensive full-slow-path
resolution (`execute_invoke_kind`), not the cheaper `vtable_fast` fallback.
Root cause: `RuleList.autoComplete()`'s `for (Rule r : list)
r.autoComplete(sentence)` loop (and the equivalent single-child call in
`RuleOptional`/`RuleRepeat`) hits the *same bytecode call site* with a
rotating sequence of the 5 different concrete `Rule` implementations — a
textbook megamorphic call site — against an `InvokeCache`
(`classloading/src/resolution.rs`) that was purely monomorphic (one slot
per call site, overwritten on every class change).

Separately, grepping `force_native_over_real_jdk_bytecode`
(`vm/src/runtime/interpreter.rs`) against `vm_exec.rs`'s `check_override`
found a second, independent gap: `check_override` has listed
`java/lang/String`'s `charAt`/`length`/`isEmpty`/`startsWith`/`substring`
(among others) as forced-native since a prior "RKC16N.6 RECON" boot-time
fix, but `force_native_over_real_jdk_bytecode` (the *cached* vtable-fast
decision point, consulted once per call site and then memoized) only had
`substring(II)` — the exact same "gate mismatch" bug class already
root-caused and fixed there for that one overload, just never completed for
the rest of the list. `RuleFixed`'s character-by-character grammar scanning
(`while (s.length() > 0 && ...) s = s.substring(1)`, `s.charAt(0)`,
`up.startsWith(name)`) hammers precisely these methods in tight loops.

### Three fixes landed

1. **`lookup_loader_initiated`** (`vm/src/runtime/interpreter.rs`) took the
   `class_manager` `RwLock` unconditionally on every call — unlike its
   sibling `should_use_loader_initiated_resolution`, which already has a
   lock-free `ANY_DEFINING_LOADER_REGISTERED`-gated fast bailout via
   `defining_loader_for` (`native-builtins/src/classloader.rs`). Confirmed
   via call-count instrumentation (`CRATONVM_DBG_HOTPATH_COUNTS`) this one
   function accounted for ~90% of ALL executed bytecode instructions on
   this workload — it's called from `resolve_class_loader_aware` on every
   single `new`/`checkcast`/`instanceof`/`anewarray`. Added the same fast
   bailout (new `any_defining_loader_registered()` accessor). Provably
   behavior-preserving (identical result in every case, just reached
   without the lock when no user-defined classloader has ever registered a
   class process-wide) and broadly beneficial well beyond this workload.

2. **`InvokeCache`** (`classloading/src/resolution.rs`) gained a small,
   *additive* polymorphic overflow cache — `put_poly`/`get_poly`, capped at
   8 distinct receiver classes per call site — consulted from
   `execute_invokevirtual_cached`'s `VirtualBytecode`-arm receiver-mismatch
   guard (`vm/src/runtime/interpreter.rs`) right before it would otherwise
   give up and fall through to the expensive slow paths. The existing
   monomorphic `entries` map, its `get`/`put`/`evict` semantics, and every
   other call site are completely unchanged — this is a pure second-chance
   lookup, not a rework of the primary cache. Confirmed working via direct
   instrumentation: 2.5M poly hits vs 226 misses process-wide on this
   workload. Verified the existing `invoke_cache_evicts_stale_entry_after_
   redefine_bump` unit test (and its sibling) still pass — `evict()` now
   also clears the poly-cache entries for that call site, preserving the
   redefine-staleness invariant.

3. **`force_native_over_real_jdk_bytecode`** gate completion: added
   `charAt`/`length`/`isEmpty`/`startsWith(String)`/`substring(int)` for
   `java/lang/String`, matching `check_override`'s already-vetted intent.
   Deliberately scoped to the locale/Unicode-independent subset of
   `check_override`'s String list — `trim`/`toLowerCase`/`toUpperCase`/
   `replace`/`compareTo`/`compareToIgnoreCase` were left alone pending
   their own correctness review (in particular, `native_string_trim`
   already uses Rust's `str::trim()`, which trims full Unicode whitespace —
   different from Java's `String.trim()` spec, which only strips
   codepoints `<= U+0020`; that's a **separate, pre-existing** native-vs-
   bytecode correctness gap, not something this session introduced or
   fixed, flagged here for whoever picks it up next).

### Net effect and what's still open

Measured on the isolated repro: **~900ms → ~530-590ms** (best clean
readings; the Azure host was under severe, fluctuating contention for much
of this session — including one stretch peaking at load average 134 on a
16-core box — which prevented a single fully-clean combined measurement of
all three fixes together, but the trend and the per-fix mechanisms are each
independently confirmed).

**This is not sufficient to close the bug.** Re-ran `TestWeb.testWebApp()`
and `TestBnf.testProcedures()` directly against the fixed binary with H2's
real, unwidened 100ms budget — both still fail at the exact same
assertions. The remaining ~5-6x gap is **not** another discrete dispatch
bug: profiling showed the query's ~3.7M interpreted bytecode instructions
are overwhelmingly *ordinary* instruction execution (String/HashMap
operations' own bodies), not resolution/dispatch overhead — dispatch-path
fixes like the three above close some of the gap but can't close all of it.
Fully closing this needs genuine, broad raw-interpreter-throughput work
(the same category this doc's original 2026-07-22 investigation already
concluded was required), which is out of scope for further scoped patches;
it would need its own dedicated investigation with room for a much larger,
carefully-regression-tested change.

**Regression verification performed:** `cratonvm-classloading`'s
`invoke_cache` unit tests (2/2 pass, via `cargo test -p
cratonvm-classloading --release invoke_cache`); live runs of
`org.h2.test.unit.TestStringUtils`, `org.h2.test.db.TestAlter`,
`org.h2.test.unit.TestShell`, and `org.h2.test.db.TestLinkedTable` against
the fixed binary (all pass cleanly); `org.h2.test.unit.TestUpgrade` fails
only at its own already-tracked, unrelated `RootReference` residual (see
`bug-h2-suite-residual-fail-triage-FIXED.md`). A `cargo test -p cratonvm-vm
force_native_over_real_jdk_bytecode` run was attempted but got OOM-killed
by the host's memory pressure before completing — not evaluated, worth
re-running by whoever picks this up next on a less contended host.

## Follow-up (2026-08-10): the gap is now MEASURED per query, and CratonVM straddles the 100 ms budget

Found while triaging the H2 three-GC-variant sweep's GC-independent FAILs.
`TestBnf` and `TestWeb` are two of exactly three classes that FAIL under all
three collectors and PASS on an **idle** HotSpot 25 control — so this page owns
2/3 of that residue. (The third is `TestTransaction`, a different budget; see
`internal/fixed-suite-bugs/h2-suite-bugs/gc-variant-fullsuite-crashes-hangs-fails-20260810-FIXED.md` §4.)

A probe replicating `testProcedures`' completion half verbatim, timing each
`getNextTokenList` (`org.h2.test.unit.TestBnfTiming`), idle host:

| query | HotSpot | CratonVM | verdict |
|---|---:|---:|---|
| `Bnf.getInstance` + `updateTopic`×2 + `linkStatements` (setup) | 69 ms | **1282 ms** | 18.6x |
| `SELECT CUSTOM_PR` (first query) | 57 | **230** | **MISS** — n=3, `INT` absent |
| `create table "test" as (sel` | 0 | 64 | ok |
| `create table test as (sel` | 0 | 57 | ok |
| `select 1\|\|f` | 4 | 99 | ok |
| `select 1 \|\| 2 ` | 2 | 50 | ok |
| `SELECT LEAS` | 3 | 91 | ok |
| `SELECT CUSTOM_PRINT(` | 4 | 107 | ok |
| `select 'abc` | 2 | 96 | ok |

Three things this changes about the page's framing:

1. **Only the FIRST query fails, and it fails because it is cold.** 230 ms
   against a 100 ms budget; every later query runs 50-107 ms on the same warmed
   code. The page's earlier ~900 ms figure for `select 'abc` was measuring a
   cold path; warm, that same query is now 96 ms and **passes**.
2. **CratonVM straddles the budget.** 89-125 ms across repeats against a
   hardcoded 100 ms means `TestBnf`/`TestWeb` are coin-flips, not steady
   failures — which is exactly why an earlier HotSpot control taken on a
   *loaded* host showed HotSpot failing `TestWeb` too, and an idle one shows it
   passing. Any measurement of these two classes has to state host load.
3. **The target is ~1.3-2x, not "broad throughput work".** The page's 2026-07-23
   follow-up landed three real fixes for ~1.5-1.7x and concluded the rest needed
   an open-ended programme. Per-query numbers say the remaining distance is
   small and concentrated: clear the cold first query (230 → <100) and pull the
   warm ones off the 100 ms line.

And one lever result, so nobody re-runs it: **the JIT tier-up threshold is inert
here.** First query 222 ms (threshold 500), 206 ms (500 000), 252 ms
(`--nojit`); warm queries 48-125 ms on all three arms. So unlike
`TestTransaction` — which IS an instance of
`../vm/jit-net-negative-on-call-dense-classes-20260810.md` and moves 20% on that
lever — this class is raw interpreter/runtime throughput and the admission
policy is not implicated.

Probe: `org.h2.test.unit.TestBnfTiming`, ~8 s per run, deterministic on the
MISS. It must call `linkStatements()` — see this page's own 2026-07-22 warning
about the repro that skipped it.
