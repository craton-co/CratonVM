# `ResourceTests` residuals — 5 distinct pre-existing bugs beyond the reported cluster

Status: FIXED (5 of 5 residuals). Moved to `..` after the 2026-07-05 follow-up pass.

## Summary

The `core` bug-cluster report listed `org.springframework.core.io.ResourceTests`
as one failing class (`AssertionError`, generic). Root-causing it surfaced
several independent bugs. A first pass fixed the originally-reported cluster
and got the class to 58/68 (see git history - not repeated here), then fixed
3 of the 5 residuals documented here, getting the class to **61/68** with
`--jdk real --jit on`.

The 2026-07-05 follow-up pass fixed the remaining two documented residuals and
one newly-visible URL filename parsing failure, getting the class to **65/68**.
The remaining failures are separate HTTP URL/network issues now tracked in
`docs/known-issues/resourcetests-remaining-http-url-failures.md`.

## Resolution update - 2026-07-05

The follow-up branch `fix/resourcetests-residuals-20260705-resourcetests-13506`
closed the two open residuals from this document:

- Residual 2 (`getFilePath()` Mockito/ByteBuddy): fixed annotated generic-array
  type wrapping for `GenericArrayTypeImpl`, added the missing base annotated
  owner native, and preserved `Path.toString()` native dispatch after Mockito
  redefines `java.nio.file.Path`.
- Residual 5 (`resourceCreateRelativeUnknown`): fixed the duplicate
  `Files.size(Path)` native by routing it through the normalized VFS-aware path
  helper and mapping missing files to typed `NoSuchFileException` behavior.
- Also fixed the newly-visible `UrlResourceTests#filenameIsExtractedFromFilePath`
  failure by splitting `file:` URL/URI synthetic fields into `file`, `path`,
  `query`, and `ref` instead of leaking `?query` into `getPath()`.

Verification on `/data/cratonvm-resourcetests-residuals-20260705-13506`:

- `ResourceTests$UrlResourceTests#filenameIsExtractedFromFilePath`: 1/1 OK.
- `ResourceTests$FileSystemResourceTests#getFilePath` followed by
  `urlAndUriAreNormalizedWhenCreatedFromFile`: 2/2 OK in the same JVM.
- Full `org.springframework.core.io.ResourceTests`: found=68, passed=65,
  failed=3 (`res-residuals-13506-urlfix-jit-real-all-20260705-060934`).

The remaining 3 failures are all HTTP URL/network cases, not the residuals
tracked here.

## FIXED — Residual 1: `ClassPathResource.createRelative("../X.class")` fails `getURL()`

**Root cause**: `t19_h10_validate_resource_name()` (`../../../native-builtins/src/lang_class.rs`)
hard-rejected any `..` path segment in the resolved resource name, as a
defence-in-depth measure for `Class.getResource[AsStream]`. This blocked the
*legitimate* HotSpot-matching case where a directory-classpath-root lookup
transparently tolerates an embedded `..` (e.g.
`SomeClass.class.getResource("../Sibling.class")`) — the real containment
check already lives one layer down in
`find_resource`/`find_all_resource_urls` (`../../../classloading/src/class_path.rs`),
which canonicalize the resolved path and reject it unless it stays
`starts_with` the canonicalized classpath root (the same archive-vs-directory
split, `is_safe_resource_name` vs `is_directory_resolvable_resource_name`,
already governs jar-entry lookups there).

**Fix**: removed the `..`-segment rejection from `t19_h10_validate_resource_name`
(kept the backslash/control-byte/length checks). Updated the corresponding
unit test (`t19_h10_validate_resource_name_allows_traversal_rejects_backslash`).

**Verified**: fixes `resourceCreateRelativeWithDotPath [ClassPathResource with
Class]`; suite went from 58/68 to 59/68.

## FIXED — Residual 3: `urlAndUriAreNormalizedWhenCreatedFromFile()` — URI.toString()/toURL() field-index mismatch

**Symptom**: `resource.getURL().toString()` returned
`"file:/C:/.../java.nio.file.Path@13653ec9"` — a `Path` object's default
identity-hash `toString()` embedded literally in place of the real path.

**Root cause**: `java.net.URI` natives are registered from (at least) two
places with **incompatible field-index conventions**:
- `net_phase_e.rs`'s `register_uri_natives` (`uri_raw_string()` helper):
  tries the by-name `"string"` field first (real-JDK-safe, since in real-JDK
  mode `alloc_object`'s slot layout follows the *real* class's field order,
  not a fixed index), then falls back to indices `6`, `5`, then a guarded
  index `0`.
- `phases_early.rs`'s `register_phase54_net_extras`: read/wrote **raw index
  6** directly for `toString`/`toASCIIString`/`toURL`/`equals`/`hashCode`,
  with no by-name fallback. This is the convention used by the *actual*
  `URI.<init>(String)` / `URI.create(String)` natives, which do write index
  6 — but **not** by the newer `url_parse()`+`uri_store_named()` construction
  helper (used by `File.toURI()`, `Path.toUri()`, etc.), which only sets
  the by-name `"string"` field and index 5 (`URL_FIELD_FULL`), never index 6.

`register_phase54_net_extras` registers *after*
`net_phase_e::register_phase_e_networking` (`register_essential_natives` vs
`register_synthetic_overrides` call order in `lib.rs`), so its raw-index-6
versions win — and for any URI built via `url_parse`/`uri_store_named`, index
6 is unset. Reading it apparently returned stale/incidental data that
manifested as a `Path` object's identity-hash string once fed through further
string-building.

**Fix**: `phases_early.rs`'s `toString`/`toASCIIString`/`toURL`/`equals`/
`hashCode` registrations for `java/net/URI` now call
`net_phase_e::uri_raw_string()` (made accessible; already handles both
conventions) instead of reading raw index 6 directly. This is a strict
widening — the `<init>`/`create` convention (index 6 set) still works via
`uri_raw_string`'s index-6 fallback; the `url_parse`/`uri_store_named`
convention (index 6 unset) now also works via the by-name lookup.

**Verified fixed in isolation** (see "Cross-test pollution" note below for
why it still shows as failing in the full-class run): running just this one
method via a custom single-method JUnit launcher (`KRunMethod.java`, see
"Debugging tools added this session" below) passes 1/1, both standalone and
as the only method run.

## FIXED — Residual 4: `canCustomizeHttpUrlConnectionForExists[Fallback]()` — customizeConnection no-op'd for ALL callers

**Symptom**: `UrlResource`'s `exists()` (and, transitively, `contentLength()`/
`lastModified()`) never applied a subclass's `customizeConnection(HttpURLConnection)`
override — a custom request header (`"Framework-Name: Spring"`) never reached
the outgoing request. Confirmed this way for BOTH the simple
(`canCustomizeHttpUrlConnectionForExists`, single HEAD request) and fallback
(`canCustomizeHttpUrlConnectionForExistsFallback`, HEAD-then-GET retry after
405) variants — proving it wasn't about the retry logic at all.

**Root cause**: `net_phase_e.rs` (S111r24, ~2026-06-11) registered
`AbstractFileResolvingResource.customizeConnection(URLConnection)` **and**
`customizeConnection(HttpURLConnection)` as unconditional native no-ops,
working around a *different*, narrower bug: `ResourceUtils.useCachesIfNecessary`
calling `con.getClass().getSimpleName()` on a synthetic `HttpURLConnection`
Class mirror could throw, breaking `UrlResource.getInputStream()`'s
uncaught-exception path (`loadSpringFactories` swallowing the connection
silently). But no-opping `customizeConnection` entirely disabled it for
**every** caller, not just `getInputStream()` — including `exists()`,
`contentLength()`, and `lastModified()`, which call it directly.

Two things make this fix safe:
1. `UrlResource.getInputStream()` is *separately* natively overridden
   (`net_phase_e.rs`, "S111r25") to delegate straight to `URL.openStream()`,
   completely bypassing `openConnection()`/`customizeConnection()` — so the
   original S111r24 motivating scenario is already covered independently of
   the no-op.
2. Empirically, `con.getClass().getSimpleName()` on the connection objects
   `customizeConnection` actually receives today works fine — verified via a
   standalone repro: `url.openConnection()` for an `http://` URL returns a
   *real* `sun.net.www.protocol.http.HttpURLConnection` (not a synthetic
   mirror), and `getSimpleName()` on it correctly returns `"HttpURLConnection"`
   with no exception.

**Fix**: removed both `customizeConnection` no-op registrations from
`net_phase_e.rs`. Real bytecode now runs for both overloads, restoring the
real JDK contract (`useCachesIfNecessary` plus `instanceof`-gated virtual
dispatch to the receiver's actual `customizeConnection(HttpURLConnection)`
override).

**Debugging note**: verifying this required temporary `eprintln!`
instrumentation in `is_real_carrier`/`huc_get_response_code`/
`huc_set_request_property` (`http_url_connection.rs`) and `huc_perform`
(`net_phase_e.rs`) to rule out three other plausible-looking hypotheses
first (a duplicate/incompatible `HttpURLConnection` native registration in
`phases_early.rs` — real, fixed as a drive-by, see below, but not sufficient
alone; an identity-hash cache collision between the HEAD and retry-GET
connection objects; a fundamental virtual-dispatch bug for 4-level
override+overload chains — refuted via multiple HotSpot-vs-CratonVM
comparison repros). The eventual root cause (the no-op registration) was
found only by grepping `../../../native-builtins/src/net_phase_e.rs` for
`"AbstractFileResolvingResource"` and reading the S111r24 comment block in
full. All temporary debug instrumentation was removed before landing.

**Drive-by fix (kept, real bug, not sufficient alone)**: `phases_early.rs`'s
`register_phase54_net_extras` also duplicated `getResponseCode`/
`getRequestMethod`/`setRequestMethod`/`setRequestProperty`/
`addRequestProperty` for `java/net/HttpURLConnection` with a naive
non-real-carrier-aware implementation (always writing/reading a fixed
synthetic field-4 header array, even for a real-JDK connection whose
`is_real_carrier()` check should route through the identity-keyed
`real_reqs`/`real_results` side tables in `http_url_connection.rs`). Since
`register_phase54_net_extras` runs after `http_url_connection::
register_http_url_connection_real`, its duplicate silently shadowed the
correct, real-carrier-aware implementation. Removed the duplicate
registrations (kept `setDoInput`/`setDoOutput`/`connect`/`getInputStream`/
etc. in `phases_early.rs`, which don't collide). This alone did not fix
the test (the real culprit was the no-op above), but it's a genuine,
independently-confirmed bug worth keeping fixed.

**Verified**: `canCustomizeHttpUrlConnectionForExists` and
`canCustomizeHttpUrlConnectionForExistsFallback` both disappeared from
`failcauses.log`; suite went from 59/68 to 61/68 in the same run (fixed 2
tests at once).

## Cross-test JVM-state pollution (major finding, ties Residual 2 to Residual 3)

Running the full `ResourceTests` class, residual 3's fix (confirmed correct
and passing in isolation, see above) still shows up as failing — with
byte-identical symptoms to before the fix. Root cause: `getFilePath()`
(Residual 2's Mockito test, still failing — see below) corrupts JVM-global
state when its `mock()` call throws partway through ByteBuddy/Mockito's
inline-mock-maker class-redefinition attempt on `java.nio.file.Path`. Once
`getFilePath()` runs (successfully or not) earlier in the same JVM process,
`Path`/`URI` `toString()` dispatch for later tests in the same run breaks,
producing the exact `java.nio.file.Path@<hash>` identity-string symptom.

Proof (via the custom `KRunMethod.java` launcher — see below): running just
`urlAndUriAreNormalizedWhenCreatedFromFile` alone passes 1/1. Running
`getFilePath` followed by `urlAndUriAreNormalizedWhenCreatedFromFile` in the
same launcher invocation: `getFilePath` fails as expected (Residual 2
unfixed), AND `urlAndUriAreNormalizedWhenCreatedFromFile` ALSO fails with the
identical `java.nio.file.Path@13653ec9` corruption — reproduced even in this
minimal 2-method combination.

This means fixing Residual 2 will likely also make Residual 3 (and possibly
others) pass in the full-class run, even though Residual 3's native fix is
already independently correct. Not yet root-caused to the exact mechanism
(most likely: `Instrumentation.redefineClasses`/ByteBuddy's inline mock maker
partially mutates `java.nio.file.Path`'s class metadata before the
`IllegalArgumentException` aborts the mock, leaving native method dispatch
for `Path`/`URI` in a broken state for the rest of the process). A follow-up
investigating Residual 2 should start by checking `../../../vm/src/runtime/instrument.rs`
(the `Instrumentation`/`redefineClasses` implementation) for how a
partially-failed redefinition of a class with CratonVM native overrides
(like `java/nio/file/Path`) is (or isn't) rolled back.

## OPEN — Residual 2: `getFilePath()` — Mockito still cannot mock `java.nio.file.Path`

Failure signature: `org.mockito.exceptions.base.MockitoException: Mockito
cannot mock this class: interface java.nio.file.Path.` with underlying cause
`java.lang.IllegalArgumentException: object of type
net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$NoOp
is not an instance of java.lang.reflect.AnnotatedType`, coming from
`AnnotationReader.ForOwnerType.resolve()` calling `getAnnotatedOwnerType()`
via a `JavaDispatcher` proxy.

**Partial improvement made (kept, but not sufficient)**: `Class.
getAnnotatedInterfaces()` (`../../../native-builtins/src/lang_class.rs`) wrapped each
element of the erased `getInterfaces()` array in `make_annotated_type`,
instead of the generic interface types from `getGenericInterfaces()` (which
parses the class's Signature attribute into real `ParameterizedTypeImpl`s).
Since `java.nio.file.Path extends Comparable<Path>, Iterable<Path>,
Watchable`, the first two are genuinely parameterized — wrapping the erased
`Comparable.class`/`Iterable.class` produced a base `AnnotatedTypeBaseImpl`
instead of `AnnotatedParameterizedTypeImpl`, the same class of bug the
`elasticsearch-bytebuddy-annotatedtype-proxy-mismatch.md` fix addressed for
`Method`/`Field`/`Parameter` — just not extended to `Class.
getAnnotatedInterfaces()`. Fixed by routing through
`native_class_get_generic_interfaces()` instead of raw `Class` mirrors. This
is independently correct but did not resolve the Mockito failure — the exact
same exception still occurs post-fix (verified via `KRunMethod` isolation),
meaning the real crash path is a different call site.

**Where the crash actually happens**: `getAnnotatedOwnerType()` is declared
on `AnnotatedType` itself (for a nested/inner class's enclosing type
context) — this is the same failure signature the ES bytebuddy fix already
fixed for 4 call sites (`Method.getAnnotatedReturnType`,
`Parameter.getAnnotatedType`, `Executable.getAnnotatedParameterTypes`,
`Field.getAnnotatedType`), all confirmed still correctly using
`annotated_type_impl_class_name()` dispatch (re-verified this session).
There must be at least one more call site producing a wrong-subclass
`AnnotatedType` for something in `Path`'s reflected shape, likely triggered
by ByteBuddy walking `Path`'s owner-type chain (an interface's
nesting/enclosing context, or a type-variable bound) specifically — not yet
root-caused to an exact file:line.

**Suggested next steps**:
- Since `java.nio.file.Path` is a JDK interface with no user-visible generic
  supertype chain beyond `Comparable<Path>`/`Iterable<Path>` (both now fixed
  per above), the remaining gap is likely in
  `AnnotatedType.getAnnotatedOwnerType()` itself, or in whatever builds the
  nested `AnnotatedType` for a `TypeVariable`'s bound.
- A debug-build approach (temporary `eprintln!` in
  `annotated_type_impl_class_name`/`make_annotated_type` printing the
  backing `Type`'s class plus the call stack of which native function
  invoked it) would likely resolve this faster than further static reading
  — this session ran low on time/budget before reaching that step.
- Given the cross-test pollution finding above, fixing this is higher-value
  than its single-test-failure count suggests: it may also fix Residual 3's
  full-suite visibility and possibly others.

## OPEN — Residual 5: `[6] FileSystemResource with File path` — `lastModified()`/`contentLength()` don't throw for a missing relative file

Failure signature: `java.lang.AssertionError: Expecting code to raise a
throwable.` at `ResourceTests.resourceCreateRelativeUnknown(ResourceTests.java:125)`
(the `relative4::lastModified` assertion).

Argset `[6]` constructs the resource via `new
FileSystemResource(Paths.get(resourceClass.toURI()))` (a `Path`, not a
`String`/`File`). `createRelative("X.class")` on this variant goes through
`new FileSystemResource(this.filePath.getFileSystem(), pathToUse)` then
`fileSystem.getPath(this.path).normalize()`.

**Traced and individually verified correct this session** (each of these was
checked in isolation and matches expectations):
- `FileSystem.getPath(String, String...)` (`../../../native-builtins/src/phases_late.rs`,
  the winning/only registration) — correctly builds a `p57`-shaped `Path` via
  `p57_alloc_path`.
- `Path.getFileSystem()` — correctly returns the identity-preserved owning
  `FileSystem`, or the default-FS singleton fallback.
- `Path.normalize()` (`p57_normalize_path`) — correctly leaves a
  no-dotdot/no-dot path unchanged.
- `Files.getLastModifiedTime(Path, LinkOption...)` — the sole registration
  (no duplicate found), correctly maps `std::fs::metadata` NotFound to
  `NoSuchFileException` via `extract_path_string` (already includes the
  `p57_to_os_path` Windows-drive-path fix from an earlier session).

**But**: a standalone repro (bare `main()`, real `FileSystemResource` class
from the actual jar, matching package depth, real javac-compiled class)
reproduces neither the reported symptom (`lastModified()` doesn't throw) NOR
the seemingly-contradictory behavior seen when the same check is run via the
suite (`KRunMethod` isolation of the actual parameterized
`resourceCreateRelativeUnknown` test method — no Mockito/cross-test
pollution involved, verified independently of Residual 2/3's pollution
mechanism above):
- Standalone bare-main() repro: `contentLength()` (`Files.size()`) does NOT
  throw (returns 0), `lastModified()` correctly throws.
- Isolated JUnit run of the actual parameterized test method (no other test
  methods in the run at all): the failure is specifically at
  `lastModified()` (line 125) — implying `contentLength()` (line 124, the
  assertion immediately before) DID throw correctly there.

This is the opposite asymmetry from the standalone repro, despite using the
same underlying classes/JDK/binary. The one structural difference not yet
isolated: `resourceCreateRelativeUnknown` is `@ParameterizedTest
@MethodSource("resource")`, and `resource()` builds all 7 argset `Resource`
instances (including argset `[6]`'s `Path`/`FileSystemResource`) once, up
front, before any argset's test body runs — so by the time argset `[6]`
executes, six other `createRelative`/`exists`/`lastModified` calls (against
different `Resource`/`Path`/`File` objects) have already run in the same
JVM process. Given the identity-hash-keyed side-table pattern already found
to cause real bugs elsewhere in this same investigation (see Residual 4's
ruled-out hypothesis, and Residual 3's confirmed cross-test pollution), an
identity-hash collision or some other per-object-identity cache leaking
across the 6 earlier argsets into argset `[6]`'s `Path`/`File` objects is
the leading hypothesis, but not yet confirmed — ran out of session time
before writing a multi-argset-reproducing repro.

**Suggested next steps**:
1. Write a repro that constructs and calls (roughly) the same sequence of
   operations `resourceCreateRelativeUnknown`'s 7 argsets perform, in order,
   within a single `main()`, to see if that reproduces the isolated-JUnit
   behavior (contentLength succeeds, lastModified fails) instead of the
   standalone single-argset behavior (contentLength fails, lastModified
   succeeds).
2. If reproduced, add temporary identity-hash-keyed debug logging to
   `Files.getLastModifiedTime`/`Files.size`'s native implementations
   (`../../../native-builtins/src/phases_late.rs`, search for `getLastModifiedTime`
   and the `size` registration on `java/nio/file/Files` taking a `Path`)
   printing `ctx.identity_hash_code(path_obj)` plus the resolved OS path
   string on every call, to see whether argset `[6]`'s lookup is somehow
   reading a different, earlier-cached path/result.
3. Also worth checking: `Files.size` has the same 2x-duplicate-registration
   pattern seen everywhere else in this codebase (`phases_late.rs` lines
   ~5789 and ~22955 at the time of writing), both silently returning 0 on
   any error — neither maps NotFound to NoSuchFileException, unlike
   `getLastModifiedTime`. This is a separate, confirmed, currently-latent
   bug (Residual 5's standalone repro shows `contentLength()` silently
   returning 0 instead of throwing) — independent of the cross-argset
   mystery above, and worth fixing regardless: route both registrations
   through the same NotFound-to-NoSuchFileException mapping
   `getLastModifiedTime`/`readAttributes` already use.

## Newly-discovered, out-of-scope bugs (not part of the original 5, found while fixing Residual 4)

Fixing Residual 4 let two more previously-masked `UrlResourceTests` methods
run far enough to surface their own, unrelated, pre-existing bugs (confirmed
via `KRunMethod` against the pre-Residual-4-fix binary too — i.e. these are
NOT regressions introduced by this session's fixes, just newly-visible now
that execution gets further):

- `useUserInfoToSetBasicAuth()`: `java.io.IOException: URL.openStream:
  unsupported scheme: alice:secret@localhost:<port>` — a URL constructed as
  `http://alice:secret@localhost:<port>/resource` (embedded userinfo) is
  apparently mis-parsed somewhere such that `alice:secret@localhost:<port>`
  is read as the scheme instead of `http`. Likely in `URL.openStream()`'s or
  `url_parse()`'s authority/userinfo splitting — not investigated further
  this session.
- `canCustomizeHttpUrlConnectionForRead()`: a Windows socket-connect-timeout
  IOException (`os error 10060`) talking to the test's local MockWebServer.
  Not yet determined whether this is a genuine CratonVM networking bug or an
  environment/timing issue; worth a rerun to check for flakiness before
  investigating further.

Both reproduce identically on the pre-fix binary in isolation, confirming
they are pre-existing and unrelated to this session's changes.

## Regression check (this session)

Ran `--only core.io.` (21 classes, 260 test methods) with the fixed binary
and compared per-class pass counts against the pre-fix (residual-1-only)
baseline for every class that showed any failures: `PathResourceTests`
27/38 on both (unchanged), `PathMatchingResourcePatternResolverTests` 10/22
baseline vs 15/22 post-fix (+5 improved), `SpringFactoriesLoaderTests` 31/33
on both (unchanged), `ResourceTests` 59/68 baseline vs 61/68 post-fix (+2,
this pass's target). At the time of this resource-test run,
`DataBufferTests`/`DataBufferUtilsTests` timed out on both binaries due to the
pre-existing Log4j StackWalker issue later archived at
[`databuffertests-stackwalker-log4j-context-recursion-hang.md`](databuffertests-stackwalker-log4j-context-recursion-hang.md).
No regressions found; one bonus improvement
(`PathMatchingResourcePatternResolverTests`) beyond the target class.

## Debugging tools added this session (not part of the fix; removed from the shared checkout after use)

A small custom JUnit Platform launcher (`KRunMethod.java`, parallel to the
existing `KRun.java` in `apps/spring-suite-runner/`) was written to run one
or more specific `@Test`/`@ParameterizedTest` methods (by simple name,
resolved via `getDeclaredMethods()` to sidestep JUnit's `MethodSelector`
string-parsing quirks with overloaded/parameterized method names) instead of
a whole class. Multiple `<class> <method>` pairs can run together in the
same JVM/launcher invocation, in order — this is what proved the Residual 2
to Residual 3 cross-test pollution above. `.af_*.txt` argfiles from any
prior `run-suite.sh` run (under `out/<run>/`) already contain the right
`-cp` entries and can be reused directly. It was removed from the shared
checkout after this session since `../../../apps` is gitignored anyway (not
checked in); it's short (~35 lines) and easy to recreate from this
description if needed again. A frozen baseline binary with only Residual 1's
fix, `vmfrozen/cratonvm-residual1-only.exe`, was kept for isolating
"did this test already fail before my change" checks.

## Why residuals 2 and 5 weren't fixed this pass

Both required going well past static code reading into live-binary
differential debugging (repro-vs-HotSpot comparison, JUnit test-method
isolation, temporary native `eprintln!` instrumentation) and, even with that
tooling, resolved to "confirmed real, confirmed distinct from the other 3,
not yet root-caused to a specific fix" within the session's time budget.
Both are documented above with enough detail (exact failure signature, ruled
out hypotheses, concrete next steps, and reusable debugging tools) to pick
up independently without re-doing the investigation from scratch.
