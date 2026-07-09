# TestVirtualContext — virtual classloader resource returns 404 instead of 200

**Status:** OPEN. **Severity:** medium. **HotSpot:** PASS.

## Summary

`org.apache.catalina.loader.TestVirtualContext.testVirtualClassLoader` fails:
```
java.lang.AssertionError: expected:<200> but was:<404>
```
This test exercises Tomcat's "virtual" webapp loader/context mechanism
(serving classes/resources from a location outside the normal webapp
docBase, via a virtual classloader mapping). A request that should resolve
to `200 OK` instead comes back `404 Not Found`, meaning the virtual
classloader either isn't finding the resource CratonVM's webapp resource
resolution expects, or the virtual-path mapping itself isn't being applied
correctly.

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName virtctx `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.loader.TestVirtualContext
```

## Recommendation

Check `org.apache.catalina.loader.VirtualWebappLoader`/`VirtualDirContext`
(or whatever mechanism `TestVirtualContext` sets up — read the test source
for exact setup) against CratonVM's `WebResourceRoot`/classloader resource
resolution. Likely candidates: a path-normalization difference (Windows
path separators leaking into a resource lookup key), or CratonVM's
classloader-real-mode resource resolution not consulting the virtual
mapping the same way HotSpot's does. Compare against
`reference_par_classpath_extension_uri_decode`-style prior findings in this
codebase (URI/path decode edge cases have bitten webapp resource resolution
before).

## 2026-07-09 worker update

Candidate VM-side root cause found and patched in
`native-builtins/src/classloader.rs`: real-JDK mode force-routes
`java.net.URLClassLoader.findResource/findResources` through CratonVM natives
because the JDK `URLClassPath` object is shimmed. The old native consulted the
flattened global dynamic classpath before consulting the receiver loader's own
constructor URLs, so a webapp/virtual loader could miss or be shadowed by
global resources instead of resolving through its own resource root.

Patch summary:

- `ucl_find_resource` now searches the receiver's stashed/constructor URLs with
  a temporary `ClassPath` before falling back to the legacy global scan.
- `ucl_find_resources` uses the same receiver-local scan and pins the temporary
  enumeration across the existing custom-handler probing path.
- Added a focused regression named
  `test_urlclassloader_find_resource_prefers_receiver_urls`, using a temporary
  resource named `tomcat0807_webapp.txt`.

Validation available in this worktree:

- `cargo test -p cratonvm-native-builtins classloader_tests::test_urlclassloader_find_resource_prefers_receiver_urls`
  passes.
- `cargo test -p cratonvm-native-builtins classloader_tests::` passes: 91/91.
- `cargo test -p cratonvm-native-builtins` passes: 2876 passed, 6 ignored, plus
  integration tests 5/5 and 2/2.

The actual Tomcat runner (`apps/tomcat-suite-runner` / `apps/tomcat`) is not
present in this worktree, so this note remains OPEN until
`org.apache.catalina.loader.TestVirtualContext` is rerun against the suite
fixture and confirmed green.

## 2026-07-09 verification pass (still OPEN — two new bugs found+fixed, one new blocker found)

Reran `org.apache.catalina.loader.TestVirtualContext` (both `@Test` methods:
`testVirtualClassLoader` and `testAdditionalWebInfClassesPaths`) against the
real Tomcat suite fixture on the Azure Linux host
(`/data/data/apps/tomcat`, JDK `/home/victor/jdk25`), in a fresh worktree off
dev tip `7e382917`. Confirmed the 2026-07-09 `ucl_find_resource`/
`ucl_find_resources` fix (commit `cb016825`) is merged into dev and its unit
test (`classloader_tests::test_urlclassloader_find_resource_prefers_receiver_urls`)
still passes. However, actually running the Tomcat regression test end-to-end
surfaced several separate, unrelated problems that had to be worked through
before the original 404 symptom could even be re-exercised:

**1. Fixture-completeness gap (host/environment issue, not a VM bug).**
`/data/data/apps/tomcat/test/webapp-virtual-webapp/target/classes` does not
exist, and `/data/data/apps/tomcat/test/webapp-virtual-library` is entirely
empty (no `src`, no `target`) on this host. Both are legitimate, *git-tracked*
upstream Tomcat fixture files (`git ls-files` against a clean reference
checkout at `/tmp/tomcat-src-ref` confirms `test/webapp-virtual-library/target/...`
and `test/webapp-virtual-webapp/target/classes/rsrc/resourceB.properties` are
committed source, not Maven/Ant build output that happens to live under a
`target/` directory name) — most likely stripped by whatever copy/rsync
staged this fixture onto `/data/data/apps/tomcat`, if it used a
`target`-directory exclusion rule (standard Java-project hygiene, but wrong
here since these particular `target/` dirs are checked-in fixtures, not build
output). A parallel fixture copy at `/mnt/nvme1-live/data/apps/tomcat` has
these files, but its own `.suite/cp-linux-fixed.txt` bakes in a
`/data/data/data/apps/tomcat/...` prefix from a different mount context and
isn't directly usable either. Per the task's instructions this fixture is
shared/read-only, so it was **not** modified; verification here used a
throwaway `/tmp/vcloader_sandbox/test/` overlay (symlinks to the real fixture
for every subdirectory except the two broken ones, which were populated from
the clean `/tmp/tomcat-src-ref` checkout). Worth fixing at the fixture-staging
level so future suite runs on this host don't need this workaround.

**2. `java.io.File.FS` stays null after a swallowed `<clinit>` failure (real
VM bug, FIXED).** `File.<clinit>`'s real bytecode (checked via `javap` against
jdk25) assigns `FS = DefaultFileSystem.getFileSystem()` **first**, before the
four fields (`separatorChar`/`separator`/`pathSeparatorChar`/`pathSeparator`)
that `vm_util.rs`'s existing `post_clinit_fixup` for `java/io/File` already
backfills. `DefaultFileSystem.getFileSystem()` is just `new
UnixFileSystem()`, and `UnixFileSystem::<init>` *is* natively overridden
(`native_platform_filesystem_init`, registered in `register_essential_natives`)
specifically to avoid the synthetic-`Properties`-NPE that motivated the
existing fixup — but `File.<clinit>` runs during very early VM bootstrap,
before that native override is wired up for this call site, so the
constructor's real, unshimmed bytecode runs instead, NPEs on the synthetic
`System.getProperties()` singleton's null `map` field, and `FS` is left null
(confirmed via a small reflection probe: a `new UnixFileSystem()` performed
*after* bootstrap completes works fine, matching the "constructor override
works, timing is the problem" theory). Any later real-JDK code that calls
`File.isInvalid()` (e.g. `FileOutputStream`'s constructor —
`TestVirtualContext.testAdditionalWebInfClassesPaths`) dereferences the null
static `FS` and throws `NullPointerException: Cannot invoke
"java.io.FileSystem.isInvalid(java.io.File)" because "java.io.File.FS" is
null`.

Fixed in `vm/src/vm/vm_util.rs`'s `post_clinit_fixup` for `"java/io/File"`:
after the existing four-field backfill, if `FS` is still null, force-load
`java/io/UnixFileSystem` (or `WinNTFileSystem` on Windows) via
`shared.load_class_concurrent` (needed — the class is not yet registered
under its simple name via a find-only lookup at this point, even though `new
UnixFileSystem()` was already attempted), allocate a synthetic instance,
populate its `slash`/`colon-or-semicolon`/`altSlash`/`userDir` fields with the
same platform-correct values `native_platform_filesystem_init` would set, and
backfill the static. Guarded so an already-valid `FS` (real object identity)
is never clobbered.

**3. `UnixFileSystem`'s `colon` field was never set (real VM bug, FIXED).**
While probing bug 2, found `native_platform_filesystem_init` (`native-builtins/src/lib.rs`)
only ever wrote a field named `"semicolon"` (correct for `WinNTFileSystem`)
and never `"colon"` (the real field name on `UnixFileSystem` — confirmed via
`javap`), so on Linux the write silently no-ops (per the field-by-name
setter's documented "absent field ignored" behavior) and `colon` stays at its
zero-init default (`'\0'`) instead of `':'`. Fixed by also writing
`"colon"` with the same computed path-separator value; each of the two field
names is a no-op on the class that doesn't declare it, so this is safe for
both platforms.

Both fixes validated via a direct reflection probe (`FS` now resolves to a
real `java.io.UnixFileSystem` instance with `slash=/`, `colon=:`, correct
`userDir`) and the full existing test suite: `cargo test -p
cratonvm-native-builtins classloader_tests::` still 92/92 (including
`test_urlclassloader_find_resource_prefers_receiver_urls`), and `cargo test -p
cratonvm-native-builtins` 2937 passed / 1 failed / 6 ignored — the one
failure (`security_manager::policy::tests::wp68_substitution_dollar_escape_preserves_literal`)
is in a file this change never touches and is unrelated (a `$`-escape parsing
issue in security-policy substitution, pre-existing).

**4. New blocker found (NOT fixed, root cause narrowed).** With both of the
above fixed, `TestVirtualContext` still fails both methods, but now inside
`FileOutputStream`/`FileInputStream` construction itself, deeper than before:

```
java.lang.ClassCastException: java.lang.String cannot be cast to java.lang.ThreadGroup
	at jdk.internal.misc.InnocuousThread.<clinit>(InnocuousThread.java:170)
	at jdk.internal.ref.CleanerFactory$1.newThread(CleanerFactory.java:43)
	at jdk.internal.ref.CleanerImpl.start(CleanerImpl.java:110)
	at java.lang.ref.Cleaner.create(Cleaner.java:200)
	at jdk.internal.ref.CleanerFactory.<clinit>(CleanerFactory.java:40)
	at java.io.FileCleanable.register(FileCleanable.java:77)
	at java.io.FileOutputStream.<init>(FileOutputStream.java:211)
```

`javap -c` of `jdk.internal.misc.InnocuousThread.<clinit>` (jdk25) shows it
walks `Thread.currentThread().getThreadGroup()`'s `parent` chain via
`Unsafe.objectFieldOffset(ThreadGroup.class, "parent")` +
`Unsafe.getReference(...)`, then `checkcast ThreadGroup` on the result — the
cast fails because the value read back is a `String`. That strongly suggests
the computed field offset for `ThreadGroup.parent` doesn't match the actual
in-VM layout of the root `ThreadGroup` object reachable from
`Thread.currentThread()` at this point in bootstrap (`Unsafe.getReference` at
the wrong offset is landing on some other field — plausibly `name`, which
*is* a `String` and is commonly adjacent to `parent` in the real field
layout) — i.e. this looks like the same family of "Unsafe field offset /
object layout mismatch" bug already fixed elsewhere in this codebase for
other classes (see the `Timestamp` nanos field-slot-layout fix), just not yet
diagnosed/fixed for `ThreadGroup`. This is very likely **pre-existing**, not
introduced by fixes 2/3 above — it was simply never reached before, since
`TestVirtualContext` always hit the `File.FS` NPE (or the fixture
`IllegalArgumentException`) earlier in both test methods on every previous
run. It blocks the actual classloader-resource-resolution assertions this doc
is tracking from ever being exercised, so the original 404-vs-200 symptom
remains **unverified either way** — the test cannot get that far yet.

**Net status:** cb016825 (the original classloader fix) is merged and its
unit test passes; two newly-found, distinct, real VM bugs blocking this
specific regression test were fixed and merged today; a third, deeper,
apparently pre-existing bug (`ThreadGroup`/`Unsafe` field-offset mismatch
surfacing via `InnocuousThread.<clinit>`) now blocks further progress and
needs its own investigation. **Doc stays OPEN** — do not retire until
`TestVirtualContext` actually runs clean.
