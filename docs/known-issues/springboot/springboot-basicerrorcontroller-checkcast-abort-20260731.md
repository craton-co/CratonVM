# `BasicErrorControllerIntegrationTests` aborts with `checkcast: not an object reference`

**Status: OPEN — REGRESSED 2026-07-31 (same day as the fix below).** Two
independent GC bugs were fixed (`3211b8c74`): the moving young collector never
rooted collection-overlay side-table references, and several TreeSet/TreeMap
backing-array allocations published a pointer through a pre-allocation
(possibly relocated) owner. Filed as OPEN earlier the same day while retiring
the Spring/javac JIT bans; the ban removal was correctly exonerated then and
is unrelated to the fix. The fix was validated 20/20 clean in isolated
repeated runs of this one class — but the identical abort recurred hours
later, in the same-day 49-class residual rerun, on a binary built from a
commit (`9fcd1b63f`) that has `3211b8c74` as an ancestor. See "Regression
note (2026-07-31)" below. This is at least the **third** occurrence of this
exact class aborting/failing on this exact signature: 2026-07-28 (original,
see
[`basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md`](basicerrorcontrollerintegrationtests-caseinsensitivecomparator-crash-20260728.md)),
"fixed" 2026-07-29, regressed and refiled 2026-07-31 (this doc, GC root
cause), "fixed" 2026-07-31, regressed again 2026-07-31 (below).

## Symptom

A hard VM abort — the process dies mid-run, after the Spring Boot banner:

```
[cratonvm] main-vm run() returned Err: Error in thread "main"
internal error: checkcast: not an object reference
```

Intermittent but frequent: **5 hard aborts and 3 partial failures in 12 runs**
of `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
(`module/spring-boot-webmvc`, Spring Boot 4.1.0-SNAPSHOT, real JDK 25). Some
runs died with SIGSEGV instead, and a second face — a flaky
`BeanDefinitionStoreException: Error processing condition on
HttpMessageConvertersAutoConfiguration` — turned out to be the same corruption
landing elsewhere.

## Root cause

The abort site is precise once the error message names it (this session
widened it from the bare `checkcast: not an object reference`):

```
checkcast: not an object reference (got Int(0))
  at java/lang/String$CaseInsensitiveComparator.compare(Ljava/lang/Object;Ljava/lang/Object;)I pc=6
```

Spring's `JdkClientHttpRequest.lambda$buildRequest$0` calls
`DISALLOWED_HEADERS.contains(name)`. `DISALLOWED_HEADERS` is a
`TreeSet<>(String.CASE_INSENSITIVE_ORDER)`, and CratonVM implements
`TreeSet.contains` natively: `native_ts_contains` → `ts_binary_search` →
`tree_compare` → `comparator_compare` → the real
`CaseInsensitiveComparator.compare`. The element handed to the comparator was
`Int(0)`, not a String.

Instrumenting the backing store showed why:

```
[DBG_TSSLOT] non-ref at mid=2 size=5 arr_len=0 ... data_cid=ClassId(0)
             data_cls="java/lang/Object" owner_cls="java/util/TreeSet"
```

`size=5` but the backing array has `array_length == 0` and `ClassId(0)` — the
documented signature of a **freed, zeroed** object. The array had been
collected out from under a live TreeSet, and reading past its (now zero)
length yields `Int(0)`.

CratonVM keeps TreeSet/TreeMap/LinkedList/LinkedHashMap backing stores in
process-global Rust **side tables**, not in Java heap fields, so no root slot
and no card can describe the collection→array edge. Two independent holes let
that edge be missed:

### 1. The moving young collector had no overlay rooting at all

`native_roots::scan_collection_overlays` deliberately skips the unconditional
overlay root scan on the Generational collector while JIT quiescence is
engaged, an unregistered JIT frame is on the stack, or a major GC is pending —
relying instead on the marker walking each owner and pulling in
`external_roots_for_owner`. `sweep_young_non_moving` and `old_gen_gc`
implement that owner walk. The **moving Cheney young path never did**: an
audit of every `external_roots::` use in `gc/src/gen_heap.rs` finds them in
`sweep_young_non_moving`, `scan_young_object` and `old_gen_gc`, and nowhere in
the moving path. When both conditions met — overlay scan skipped, moving young
chosen — every overlay-held young array was silently reclaimed.

That combination became common exactly when moving-young became the default
(`codex/moving-young-default-20260730`), which matches the observed window:
0 aborts in 12 runs on dev `9ac1feffe`, 5 in 12 on `376114f635`.

**Fix** (`gc/src/gen_heap.rs`, new "Phase 1a"): seed the Cheney evacuation from
`external_roots_for_matching_owners(&|_| true)` — every current owner's refs,
exactly as the non-moving young path does. Only forwarding is needed; the side
tables are repointed afterwards by the existing `remap_external_roots` pass.

### 2. Backing arrays were published through a stale owner

`alloc_ref_array` is a Java-heap allocation and can trigger a moving young
collection that relocates the collection object. Ten TreeSet/TreeMap sites
allocated a backing array and then stored it through the **pre-allocation**
`this`, registering the overlay under a stale owner address in
`overlay_owner_keys` — the reverse index the collector consults for
"which side-table refs does this collection own?". The collection still read
its own state fine (the side-table key is the relocation-invariant identity
hash), but the GC could not associate the live object with its overlay.

`native_tm_put` already had this fix, commented "gcstress face-1"; every other
site had been missed.

**Fix** (`native-collections/src/lib.rs`): new `ts_install_backing_array` /
`tm_install_backing_array` helpers that pin BOTH the owner and the new array
across the allocation and the store and return the refreshed pair; all ten
sites routed through them, plus the unpinned `new_arr` window in
`ts_ensure_capacity`.

## Results

`BasicErrorControllerIntegrationTests`, real JDK 25:

| build | runs | aborts | partial failures | clean |
|---|---|---|---|---|
| dev `2f138f04e3` (before) | 12 | 5 | 3 | 4 |
| + side-table pin fix only | 14 | 1 | 2 | 11 |
| + moving-young overlay rooting | 20 | **0** | **0** | **20** |
| final (diagnostics removed) | 20 | **0** | **0** | **20** |

The `TreeSet` backing-store probe fired 0 times across the last 40 runs
(previously once per failing run), and the second face — the flaky
`Error processing condition on HttpMessageConvertersAutoConfiguration` — is
gone as well.

Unit tests: `cargo test --release` — `cratonvm-gc --lib` 873/0,
`cratonvm-native-collections` all suites pass including the overlay GC harness
(14 tests, one new: `every_overlay_value_is_reachable_through_an_always_true_owner_predicate`,
which pins the seed source the moving path now depends on),
`cratonvm-vm --lib` 2301/0, `cratonvm-jit --lib` 1060/0.
`JavacConsolidationProbe 200` still OK 200/200.

## Why this was worth chasing past the obvious

The abort's shape is identical to `SPRINGBOOT-HTTP-HEADER-COMPARATOR.1`, one
of the JIT bans removed earlier the same day (its doc comment: "a call to
`CaseInsensitiveComparator.apply(Object)`, followed by a fatal invalid-
reference checkcast"). The `NoSuchMethodError:
CaseInsensitiveComparator.apply(Object)Object` warning even appears in the log
immediately before the abort. Both are red herrings: the `apply` call is
`comparator_compare`'s documented key-extractor fallback, which runs only
*after* the real `compare` has already failed, and a pristine-dev control with
every ban still in place reproduced the abort at the same rate. The ban
removal was not involved.

## Regression note (2026-07-31)

Recurred in the same-day 49-class residual rerun
(`craton-rerun-20260731`/`all-jit`), on
`cratonvm-spring-boot-residual0728.exe` built from `dev` merged to
`9fcd1b63f` — a commit that has this doc's fix commit `3211b8c74` ("fix(gc):
root collection-overlay refs in the moving young collector") as an ancestor
(`git merge-base --is-ancestor 3211b8c74 9fcd1b63f` succeeds). The rerun's own
notes (`apps/spring-boot-suite-runner/RESULTS-20260731-residual49.md`) list
this class under "3 CRASH (same as the full-suite round, not re-diagnosed)" —
i.e. it was assumed to be the pre-fix abort and not actually re-checked
against the new signature until this investigation.

Log evidence
(`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorCo-957a1d0f4289.{out,err}.log`),
2026-07-31T19:04-19:05Z:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String$CaseInsensitiveComparator.apply(Ljava/lang/Object;)Ljava/lang/Object;" caller="org/springframework/http/client/JdkClientHttpRequest.lambda$buildRequest$0(Ljava/net/http/HttpRequest$Builder;Ljava/lang/String;Ljava/util/List;)V @pc=15"
[cratonvm] main-vm run() returned Err: Error in thread "main" internal error: checkcast: not an object reference
```

This is byte-for-byte the same `NoSuchMethodError`-then-`checkcast` signature
as both the original 2026-07-28 report and this doc's own "Root cause"
section above (`CaseInsensitiveComparator.compare` reading `Int(0)` off a
reclaimed TreeSet backing array). The crash happened on the class's 4th
Tomcat boot cycle within the run (three prior boots in the same process
completed and returned `500`s for unrelated `IllegalStateException` test
fixtures — normal `BasicErrorControllerIntegrationTests` behavior), consistent
with the original bug's dependence on cumulative GC pressure across many
per-test context boots rather than firing on the first request.

Not re-diagnosed further this session (no source changes made). Plausible
explanations, in rough order of likelihood: (1) the `3211b8c74` fix closed the
specific reproduction the 20-run isolated-class validation exercised, but the
full 49-class residual run's different GC pressure/promotion pattern (many
more classes and allocations sharing the same process's generational spaces
before this class runs) still reaches a code path the fix didn't cover; (2) a
third, still-undiscovered overlay/owner site in `native-collections/src/lib.rs`
was missed by the "ten sites" audit that produced `ts_install_backing_array`;
(3) a new regression landed between `3211b8c74` and `9fcd1b63f` that
reopens the same class of bug. Whoever picks this up should first try to
reproduce with the exact residual-round harness conditions (49-class
sequential run, not an isolated repeated single-class run), since that is the
one variable that differs from this doc's "20/20 clean" validation.

## Second corroboration: `JettyServletWebServerFactoryTests` (2026-07-31 hang-reverify)

The same-day 1500s hang-reverify run (`craton-hangverify-20260731`/`all-jit`,
same worktree, `HEAD` = `a9ead67a1`, which has this doc's fix commit
`3211b8c74` as an ancestor) hit the identical
`NoSuchMethodError: ...CaseInsensitiveComparator.apply(...)` warning
immediately preceding a fatal error, this time on a Jetty worker thread
rather than a hard process abort:

```
2026-07-31T20:12:50.496062Z  WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="java/lang/String$CaseInsensitiveComparator.apply(Ljava/lang/Object;)Ljava/lang/Object;" caller="org/eclipse/jetty/http/HttpCookie.from(Ljava/lang/String;Ljava/lang/String;ILjava/util/Map;)Lorg/eclipse/jetty/http/HttpCookie; @pc=47"
Thread Thread-890 terminated with error: InternalError(Internal { message: "checkcast: not an object reference" })
```

Log:
`apps/spring-boot-suite-runner/.suite/results/craton-hangverify-20260731/all-jit/logs/module_spring-boot-jetty.org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests.{out,err}.log`

Per this doc's own "Why this was worth chasing past the obvious" section,
the `NoSuchMethodError: apply(Object)` warning is the `comparator_compare`
key-extractor-fallback red herring, not the real defect — the real defect is
the GC one above (a TreeSet/collection-overlay backing array reclaimed while
still live). `HttpCookie.from`'s use of `String.CASE_INSENSITIVE_ORDER`
(building a case-insensitive cookie-attribute map/set) is a new call site
hitting the same class of corruption, not a new bug. Killing a Jetty worker
thread mid-request instead of aborting the whole process explains a
downstream symptom in this run: the client's HTTP request timed out reading
a response
(`JettyServletWebServerFactoryTests.sessionCookieSameSiteAttributeCanBeConfiguredAndOnlyAffectsSessionCookies[2]`
— `java.net.SocketTimeoutException: Read timed out` — the server thread that
would have answered died first).

This is at minimum a fourth occurrence of this exact
`NoSuchMethodError`-then-`checkcast` signature (see the count at the top of
this doc), now confirmed at a call site outside `JdkClientHttpRequest`,
consistent with root cause #1 above (moving-young collector root-map gap)
still not being fully closed for every TreeSet/TreeMap-overlay owner and
allocation shape. Not re-diagnosed at the source level this session (no
source changes made, no rebuild/run performed) — whoever picks this up
should extend the "ten sites" `ts_install_backing_array`/
`tm_install_backing_array` audit in `native-collections/src/lib.rs` to check
whether `HttpCookie.from`'s map/set construction pattern (or whatever
TreeSet/TreeMap Jetty builds keyed on `String.CASE_INSENSITIVE_ORDER` here)
routes through one of the still-unpinned allocation sites, per explanation
(2) in the "Regression note" above.

## Residual: `whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`

Independent of the crash above (no correlated fatal event in the `.err.log`
around this test, and it does not involve `CaseInsensitiveComparator` at
all): a new connection attempt made after Jetty's graceful shutdown starts
gets routed to a handler and receives `404 Not Found` instead of being
refused at the TCP level (`HttpHostConnectException` expected):

```
java.lang.AssertionError:
Expecting actual:
  404 Not Found HTTP/1.1
to be an instance of:
  org.apache.hc.client5.http.HttpHostConnectException
but was instance of:
  org.apache.hc.client5.http.impl.classic.CloseableHttpResponse
     org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests.whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade(JettyServletWebServerFactoryTests.java:337)
```

This suggests the server's listening socket/connector is not being closed
at the point graceful shutdown begins, so new connections are still
accepted and dispatched. Not investigated further this session; no existing
doc covers this specific assertion shape (checked `jetty-webserver-factory-
poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`,
`tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md`, and
`jetty-private-lambda-wrong-receiver-startcontext-recursion-cluster-FIXED.md`
— none mention this shutdown-connection-refusal assertion). Filed here as a
separate, unrelated residual of the same class rather than a new doc, since
it doesn't stand alone as a full investigation.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests`
- `module/spring-boot-jetty` — `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` (2026-07-31 hang-reverify corroboration, new call site `HttpCookie.from`, plus an unrelated graceful-shutdown residual — see sections above)
