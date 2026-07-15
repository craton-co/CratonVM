# Keycloak WelcomePageTest residuals follow-up (2026-07-15) — one confirmed fixed, one root-caused (not fixed, architectural), one FIXED same-day in a follow-up session, one partially re-verified

Status: item 3 (zipfs `Files.copy`) is now FIXED — see the 2026-07-15 (later) update section immediately below.
Item 4 (teardown hang) has been re-verified: it did NOT reproduce once item 3 was fixed. Original investigation
(3 of 4 items, no code changes) follows unchanged below for history.
Follow-up to `docs/internal/fixed-suite-bugs/pom-xml-declaration-char-corruption-breaks-quarkus-maven-bootstrap-FIXED.md`,
which fixed the pom.xml bootstrap path and left three further residuals for dedicated investigation. This doc covers
all three, plus a fourth issue discovered along the way.

## 2026-07-15 (later same day) update: item 3 FIXED, item 4 re-verified NOT reproducing

Item 3 (`Files.copy()` from a non-default `FileSystemProvider` path) turned out to be **two** stacked
path-layout bugs, not one: `Files.copy` itself didn't classify a jarfs-encoded *source* (only the
destination was handled), and — the more severe bug, only reachable through Quarkus's *real*
`ZipUtils.unzip()` code path (not the hand-written repro in item 3 below, which is why it wasn't found
sooner) — `p57_read_path()` silently mis-read a Quarkus `PathWrapper` decorator Path as an empty
string, which made the zip mount silently fall back to the **real host filesystem root**, causing
`Files.walkFileTree` to try to copy the entire host disk into the extraction target. Full root-cause,
fix, and verification detail: `docs/internal/fixed-suite-bugs/zipfs-files-copy-wrapped-path-FIXED.md`.
Fixed and merged to `dev`: `e38d6f60`/`90cc7e73` (bug 1, `Files.copy` source), `a43436fc`/`882395cd`
(bug 2, `p57_read_path`).

With item 3 fixed, item 4's originally-reported ~27-minute post-test-completion hang was re-run
end-to-end (`org.keycloak.tests.welcomepage.WelcomePageTest` via the existing `TimedKcRunner` harness,
`--stack-dump-on-timeout 1800`): the Keycloak 26.6.1 test server now boots successfully, all 6 test
methods run, and the process exits cleanly **~183 seconds** after starting (well under a second after
the last test method finishes) — no hang, no orphaned server process left behind. The originally-reported
hang did not reproduce; it's plausible the hang was somehow related to the runaway host-filesystem-copy
condition in bug 2 (a walk of the entire host disk under heavy shared-host I/O contention could plausibly
manifest as an apparent multi-minute-to-tens-of-minutes stall depending on exactly where/when it was
observed), though this was not proven and the original report predates this fix, so no conclusive causal
link is claimed — only that the specific symptom no longer reproduces after this fix.

All 6 `WelcomePageTest` methods still individually FAIL (server boots, but the tests themselves don't
pass yet): the WebDriver-backed methods hit the pre-existing, separately root-caused (not fixed) item 2
Stream/Spliterator Selenium-JSON bug below; the non-WebDriver methods now fail later, on
`Failed to resolve artifact: org.keycloak.testframework:keycloak-test-framework-remote-providers`
(a Maven/Sisu artifact-resolution failure, not obviously a CratonVM defect) — not investigated further,
flagged as the next blocker in this chain.

## 1. resteasy `ApacheHttpClient43Engine` NoClassDefFoundError — CONFIRMED RESOLVED

Was a downstream symptom of the (separately, independently fixed) missing `sun.management.VMManagementImpl
.getVersion0()` native (`native-builtins/src/jmx.rs`, landed on `dev` the same day this investigation started).
Verified with a minimal, server-free repro — just constructing the Keycloak admin REST client directly:

```java
Keycloak kc = KeycloakBuilder.builder()
    .serverUrl("http://localhost:9999").realm("master")
    .clientId("temp-admin").clientSecret("mysecret")
    .grantType("client_credentials").build();
```

Under current `dev`, this now builds successfully under CratonVM (`BUILD_OK`) with no `ServiceConfigurationError`
and no `NoClassDefFoundError`. No further action needed.

## 2. Selenium/HtmlUnit JSON parsing failure — ROOT-CAUSED, NOT FIXED (architectural)

### Root cause

CratonVM's synthetic `java.util.stream.Stream` pipeline (`native-collections/src/lib.rs`) materialises a stream's
elements **eagerly and in two disconnected passes** whenever a terminal operation (`.collect()`, `.map()`, etc.)
runs against a lazily-sourced stream (one built via `StreamSupport.stream(spliterator, false)`, e.g. every
`Spliterators.spliteratorUnknownSize(iterator, 0)` call):

1. `stream_elements()` -> `materialize_lazy_stream()` -> `drain_spliterator_to_array()` drains the **raw** source
   Spliterator via `tryAdvance(collector)` in a loop, where `collector` just appends whatever `next()` yields —
   with NO downstream operation (map/filter/etc.) applied yet.
2. Only afterward does `.map()`'s own loop apply the user's mapper function to each already-drained element.

This is semantically different from real JDK Streams, which are pull-based: each element flows through the
**entire** downstream pipeline (map -> filter -> collect-accumulate) in one interleaved step per spliterator
advance. The two-pass CratonVM model is invisible for the common case (an Iterator whose `next()` returns an
independent, fully-realized value per call) but breaks for the "cursor" pattern where:

- `next()` returns `this` (the same object every call, not a new value), and
- the real per-element consumption/advancement is deferred to a *separate* call the downstream processing function
  makes (not `next()` itself).

`hasNext()` for such a cursor legitimately depends on state that only changes once the downstream processing runs
— but during CratonVM's pure drain phase (step 1 above), that processing hasn't run for even one element yet, so
`hasNext()` can never observe a state change and never returns `false`. The drain loop runs until it hits the
hard-coded safety cap in `drain_spliterator_to_array` (**1,000,000** iterations), collecting 1,000,000 *identical*
references to the same cursor object. `.map()`'s subsequent loop then applies the real function to all 1,000,000
of them — the first few calls succeed (since the cursor's state legitimately advances once actually processed),
and the rest fail once the cursor is genuinely exhausted.

This is **exactly** the pattern Selenium's own JSON parser (`org.openqa.selenium.json.JsonInputIterator`, used by
`org.openqa.selenium.json.MapCoercer` to decode a JSON object into a `Map`) uses: `JsonInputIterator.next()`
returns `this` (the `JsonInput`), and the real key/value consumption happens inside
`JsonTypeCoercer.coerce(...)`, invoked from a `.map()` lambda over the iterator-turned-stream. Hence: any
`Json.toType(str, Map.class)` call (used by `RelativeLocator.asAtomLocatorParameter`, hence every
`HtmlUnitWebDriverSupplier`-based test) throws `JsonException: Unable to determine type from: ','` once the
runaway drain corrupts its element count.

### Minimal, Selenium-free repro

No Selenium/WebDriver/network involved — pure `java.util.stream` + a hand-rolled cursor Iterator:

```java
class Cursor implements Iterator<Cursor> {
    int idx = 0;
    final List<String> entries;
    Cursor(List<String> entries) { this.entries = entries; }
    public boolean hasNext() { return idx < entries.size(); }
    public Cursor next() { return this; }               // <-- returns `this`, not a fresh value
    String readEntry() { String v = entries.get(idx); idx++; return v; }  // <-- real consumption, deferred
}
...
Cursor cursor = new Cursor(List.of("a", "b", "c"));
Spliterator<Cursor> sp = Spliterators.spliteratorUnknownSize(cursor, 0);
List<String> result = StreamSupport.stream(sp, false)
    .map(c -> c.readEntry())
    .collect(Collectors.toList());
```

- **Real HotSpot (JDK 25)**: terminates correctly after exactly 3 elements; `result = [a, b, c]`.
- **CratonVM (current `dev`, confirmed with `--nojit` too — not a JIT bug)**: runs the drain-then-map two-pass
  described above; `result` ends up `[a, b, c, <3rd-element-repeated-or-overflow>, ...]` depending on the exact
  probe variant, always terminating only once `drain_spliterator_to_array`'s **1,000,000**-iteration safety cap
  is hit (confirmed via `eprintln!` instrumentation added-then-reverted in `native-collections/src/lib.rs`: zero
  Spliterator-native-registration hits — `tryAdvance`/`forEachRemaining`/`characteristics`/`estimateSize` are
  never invoked for this path, ruling out a dispatch-routing bug and confirming the eager
  `drain_spliterator_to_array` mechanism itself is the site).
- A plain `while (it.hasNext()) it.next();` loop over the *same* `Cursor` (no Stream/Spliterator involved) works
  correctly and identically on both JVMs — this is specific to the Stream/Spliterator materialisation path, not
  general `Iterator` dispatch.

### Why not fixed here

A correct fix requires restructuring how CratonVM's Stream intermediate operations (`.map()`, `.filter()`,
`.peek()`, `.flatMap()`, ...) consume their upstream source — applying each op's function *inside* the same
per-element callback used to drain the source spliterator, rather than draining raw elements first and mapping
them in a disconnected second pass. This is foundational, shared machinery used by essentially every Stream
pipeline in the VM; every one of those call sites (`native_stream_map`, `_filter`, `_flat_map`, `_collect`, and
the `stream_elements`/`materialize_lazy_stream`/`drain_spliterator_to_array` helpers they all funnel through)
would need re-auditing to make the change safely, against a very large existing passing-test surface. Scoping and
implementing that safely is a substantial, dedicated effort — flagged for its own investigation rather than a
hasty patch attempted within this session.

## 3. NEW: `Files.copy()` from a non-default `FileSystemProvider` path fails — REPRODUCED, NOT ROOT-CAUSED

Discovered while re-verifying issue 2/3: `DistributionKeycloakServer.createInstallation()` extracts the built
Keycloak distribution zip via Quarkus's own `io.quarkus.fs.util.ZipUtils.unzip()`, which opens the zip as a
`java.nio.file.FileSystem` (the JDK's built-in zip filesystem provider) and walks it with
`Files.walkFileTree(..., new SimpleFileVisitor() { preVisitDirectory() { Files.copy(zipDirEntry, realTargetPath); } })`.
Under CratonVM this now fails on the very first directory:

```
java.lang.IllegalStateException: IOException: the source path is neither a regular file nor a symlink to a regular file
    io.quarkus.fs.util.ZipUtils$1.preVisitDirectory(ZipUtils.java:97)
```

### Minimal repro

```java
try (FileSystem zipfs = FileSystems.newFileSystem(zipPath, (ClassLoader) null)) {
    Path srcDir = zipfs.getPath("/keycloak-26.6.1");
    Files.isDirectory(srcDir);      // true  (correct, matches HotSpot)
    Files.isRegularFile(srcDir);    // false (correct)
    Files.isSymbolicLink(srcDir);   // false (correct)
    Files.copy(srcDir, realTargetPath.resolve("keycloak-26.6.1"));  // <-- FAILS
}
```

- **Real HotSpot**: `Files.copy` succeeds, creates the target directory.
- **CratonVM**: throws `IOException: No such file or directory (os error 2)` — note the exact phrasing
  ("os error 2") is Rust's `std::io::Error` `Display` format, confirming a CratonVM-native code path is involved
  somewhere in the chain, not a pure-bytecode JDK exception.
- Also confirmed: plain `Files.walkFileTree` over the same zip (reading attributes only, no `Files.copy`) works
  correctly on both JVMs — the divergence is specific to the copy operation.

### Investigation notes (why this isn't root-caused yet)

`native_files_copy` (`native-io/src/lib.rs`) was the obvious suspect — it resolves both `Path` arguments to plain
strings via `files_path_str`/`read_path_str` and always operates on them with `std::fs`, with no branch at all for
a `Path` belonging to a different `FileSystemProvider` (e.g. a zip-filesystem entry). This looked like an exact
match for the symptom. A fix was written and built (detect `std::fs::symlink_metadata` returning `NotFound` as a
signal that the source isn't a real OS path, then fall back to draining the source through its own
`FileSystemProvider` via genuine virtual dispatch — `Path.getFileSystem()` -> `FileSystem.provider()` ->
`provider.readAttributes(...)`/`provider.newInputStream(...)`), but **`eprintln!` tracing added directly inside
`native_files_copy` never fired** when re-running the repro — conclusively ruling out that function as the actual
call site for this specific `Files.copy(Path, Path)` invocation. (The speculative fix and its tracing were reverted
before finishing this session — nothing was left half-applied.)

This means real JDK bytecode is handling the top-level `Files.copy` dispatch here (consistent with the
"prefer real bytecode when the receiver/argument isn't the expected default-provider shape" pattern already
established elsewhere in this codebase for `Spliterator`/`Process` — see the pom.xml-followup doc above), and the
actual failure is somewhere **inside** that real bytecode's own cross-provider fallback path
(`java.nio.file.CopyMoveHelper.copyToForeignTarget`, which itself calls `Files.isDirectory`/`Files.createDirectory`
/`Files.newInputStream`/the 3-arg `Files.copy(InputStream, Path, CopyOption...)` — one of *those* natives, or the
zip provider's own `copy()`/attribute-reading bytecode, is the real culprit). Not pinned down further within this
session's time budget. Needs a fresh investigation starting from tracing `CopyMoveHelper.copyToForeignTarget`'s
actual bytecode-level call sequence (a stack-dump-on-timeout style approach, or a targeted `RUST_LOG`/`eprintln!`
sweep across every other `Files.*` native, would likely find it quickly given how narrow the previous trace
already made the search space).

## 4. Test-teardown hang — STILL OPEN, blocked on #3

The originally-reported hang (`TimedKcRunner` never reaching its final summary print after all 6
`WelcomePageTest` methods finish, ~27 min wall-clock vs ~3 min CPU — genuinely blocked, not slow-under-load) could
not be independently re-confirmed or diagnosed via stack dump in this session: server bootstrap now fails earlier,
during zip extraction (#3 above), before ever reaching the point in the test lifecycle where the hang was
originally observed. Re-attempt once #3 is fixed.

## What changed

Nothing merged from this follow-up session — all code edits made during investigation (a `native_files_copy`
fallback for issue 3, and `eprintln!` tracing added to both `native-io/src/process.rs`-adjacent
`native-collections/src/lib.rs` Spliterator natives and `native-io/src/lib.rs`'s `native_files_copy`) were
reverted (`git checkout --`) once shown not to address the actual mechanism. Worktree
`/data/wt-kc-webdriver-jmx-teardown-20260715` (branch `fix/kc-webdriver-jmx-teardown-20260715`, based on `dev`) is
clean and left in place with the built Keycloak 26.6.1 distribution + probes for a follow-up session to reuse.
