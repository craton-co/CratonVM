# ClassLoaderServiceImplTest (both classes) — two regressions of previously-FIXED HIB-CV-16/HIB-CV-24 bugs

**Status:** OPEN — both are regressions, not new discoveries in the "not a bug" sense.
**Run:** `apps/hib-suite-runner` "others" category, `run-20260721-152142-others/on-real`,
binary from worktree `C:/craton/CratonVM-hib-local-0712` (branch `test/hib-local-0712`,
merged with `origin/dev` HEAD `7aed580f0`), regenerated `common.args` classpath.
**Platform:** native Windows (win32) — Bug 1 below is Windows-path-specific.

Two *different* classes named `ClassLoaderServiceImplTest` both failed in this run:

| Class | found/ok/failed | Failing method | Assertion |
|---|---|---|---|
| `org.hibernate.orm.test.service.ClassLoaderServiceImplTest` | 2/1/1 | `testStoppableClassLoaderService` | `AssertionError: Expected size: 1 but was: 0 in: []` |
| `org.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderServiceImplTest` | 7/6/1 | `testLookupBefore` | `AssertionError: expected:<1> but was:<2>` |

Raw logs: `apps/hib-suite-runner/runs/run-20260721-152142-others/on-real/shard-3/raw.log`
(`service.ClassLoaderServiceImplTest`), `.../shard-4/raw.log` (`bootstrap.registry.classloading.ClassLoaderServiceImplTest`).
Both reproduced in isolation with `-Dcraton.trace=1` for full stack traces:

```
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.service.ClassLoaderServiceImplTest\norg.hibernate.orm.test.bootstrap.registry.classloading.ClassLoaderServiceImplTest\n" > /tmp/single-cls.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m @common.args -Dcraton.trace=1 -Dcraton.batch=2 CratonRunner /tmp/single-cls.txt 0
```

Both classes were previously investigated (HIB-CV-16, `docs/internal/hibernate-bugs/HIB-CV-16-custom-classloader-not-virtualized.md`)
and fixed (HIB-CV-24, `docs/internal/h2-suite-bugs/run-20260622/HIB-CV-24-classloader-isolation-delegation-FIXED.md`,
merged to dev at `579c4836b` / `67b07182a`, 2026-06-30, with verification claiming
`service.ClassLoaderServiceImplTest` **2/2 PASS** and sibling
`bootstrap.registry.classloading.ClassLoaderServiceImplTest` **7/7** unchanged). Both
have now regressed, each with a **different failure signature than the original bug**
(not a simple revert) — each was root-caused independently below.

---

## Bug 1 — `testStoppableClassLoaderService` (HHH-8363): Windows `file:` URL mishandled in `discover_providers`

### Symptom

`getTypeContributorServices()` (`ClassLoaderServiceImplTest.java:91`) asserts
`typeContributors.hasSize(1)`; got an **empty list**. This is the exact same
symptom the HIB-CV-24 fix (14b) closed on 2026-06-30 — a `TestClassLoader`
override of `findResources` hands back a `file:` URL (not `jar:...!/`) for the
`../../../../apps/META-INF/services/org.hibernate.boot.model.TypeContributor` descriptor, and
`ClassLoaderService.loadJavaServices` must find exactly 1 provider from it.

### Root cause

The 14b fix IS still present in `native-builtins/src/service_loader.rs`
(`discover_providers`, ~line 1020-1034): when a resource URL is a plain `file:`
URL (no `!/` jar separator), it reads the file directly from disk instead of
via the classpath-relative lookup:

```rust
if ext_str.starts_with("file:") && !ext_str.contains("!/") {
    let fs_path = percent_decode(
        ext_str.strip_prefix("file:").unwrap_or(&entry_path),
    );
    if let Ok(bytes) = std::fs::read(&fs_path) {
        parse_provider_lines(&bytes, &mut providers);
        got = true;
    }
}
```

On this Windows box the URL is
`file:/C:/craton/CratonVM/apps/hibernate-orm/hibernate-core/target/resources/test/org/hibernate/orm/test/service/org.hibernate.boot.model.TypeContributor`
(confirmed via `CRATONVM_DIAG_SERVICELOADER=1` tracing — `getResources` *does*
return this URL correctly). `ext_str.strip_prefix("file:")` yields
`/C:/craton/CratonVM/...` — a **malformed** Windows path (a leading `/` before
the drive letter). `std::fs::read` on that path fails with
`Os { code: 123, kind: InvalidFilename, message: "Синтаксическая ошибка в имени
файла..." }` (verified directly with a standalone `rustc` probe — real file, no
leading slash, reads fine; same path with the leading slash, `ERROR_INVALID_NAME`).
The `if let Ok(bytes) = ...` swallows the error, `got` stays `false`, and the
code falls through to `ctx.find_all_resource_bytes(&entry_path)` — a
classpath-*relative* lookup that cannot resolve an absolute filesystem path
either, so `providers` stays empty.

**The fix for this exact class of path already exists** two functions away,
unused by this call site: `file_url_path_to_fs_path` (`service_loader.rs:1270`,
used by `read_jar_url_entry` for `jar:file:/...` URLs) strips `"file:/"` (not
just `"file:"`) and special-cases the drive-letter form correctly:

```rust
fn file_url_path_to_fs_path(file_url: &str) -> Option<String> {
    let raw = if let Some(rest) = file_url.strip_prefix("file://") { rest }
              else if let Some(rest) = file_url.strip_prefix("file:/") { rest }
              else { return None; };
    let decoded = percent_decode(raw);
    if decoded.starts_with('/') || decoded.as_bytes().get(1).copied() == Some(b':') {
        Some(decoded)                    // already `/abs/path` or `C:/...`
    } else {
        Some(format!("/{decoded}"))
    }
}
```

For `file:/C:/craton/...`, this correctly yields `C:/craton/...` (no leading
slash) because `strip_prefix("file:/")` already consumes the single slash
before `C:`. The 14b fix's inline code reinvented this logic but stripped only
`"file:"` (keeping the slash), and doesn't special-case the drive-letter form.

The HIB-CV-24 verification table doesn't record the platform it ran on; this
inline logic is very likely only ever exercised/verified with POSIX-style
`file:/tmp/...` paths (where a single leading slash IS already correct), so
the Windows drive-letter case was never covered by that verification.

### Fix suggestion

Replace the inline `fs_path` computation at `service_loader.rs` ~line 1028-1030
with a call to the existing `file_url_path_to_fs_path(&ext_str)` helper (already
correct for both POSIX and Windows drive-letter forms), instead of the ad hoc
`percent_decode(ext_str.strip_prefix("file:")...)`.

### Verification of root cause (standalone, no Hibernate)

```
$ rustc probe.rs && ./probe
p1 ERR Os { code: 123, kind: InvalidFilename, ... }   # "/C:/craton/.../TypeContributor" (current behavior)
p2 OK len=77                                           # "C:/craton/.../TypeContributor" (file_url_path_to_fs_path's output)
```

---

## Bug 2 — `testLookupBefore`: user-loader `loadClass(String)` override invoked TWICE

### Symptom

`assertEquals(1, icl.getAccessCount())` (`ClassLoaderServiceImplTest.java:77`,
`bootstrap.registry.classloading` package) — expected `1`, got `2`. `icl` is an
`InternalClassLoader extends ClassLoader` (`parent=null`) whose overridden
`loadClass(String)` appends to a list before delegating to `super.loadClass(name)`;
`getAccessCount()` is that list's size.

This is the **same test** HIB-CV-16/HIB-CV-24 fixed on 2026-06-30 (was
`expected:<1> but was:<0>` — the loader was never consulted at all; verified
`ok=7 failed=0` after the fix). It has now regressed to a **different**
symptom — over-counted (2), not under-counted (0) — meaning the original fix
(`defer_to_find_class` in `cl_real_load_class_base`, still present and still
working — `AggregatedClassLoader.findClass` IS invoked exactly once) is intact,
but a **later, unrelated** change introduced a new double-dispatch defect.

### Isolated repro (no Hibernate)

`InternalClassLoader`(icl, `parent=null`, overrides `loadClass(String)`, prints+counts)
wrapped by a minimal `AggregatedClassLoader`-shaped loader (`parent=null`,
overrides `findClass` to try `tccl.loadClass(name)` then `app.loadClass(name)`),
`Class.forName(name, true, agg)`:

```
HotSpot:   [ICL] loadClass(TcclProbe) call #1                     -> icl.getAccessCount() = 1 (expected 1)
CratonVM:  [ICL] loadClass(TcclProbe) call #1
           [ICL] loadClass(TcclProbe) call #2                     -> icl.getAccessCount() = 2 (expected 1)
```

(Probe source used: a `TcclProbe.java` with static nested `InternalClassLoader`
and `AggregatedClassLoader` classes reproducing the exact shape; compiled with
the real JDK's `javac`, run once under `java` and once under
`cratonvm.exe --java-home <jdk25> -cp . TcclProbe`.)

### Root cause

`AggregatedClassLoader.findClass(name)` (real bytecode) calls `tccl.loadClass(name)`
— an **ordinary bytecode `invokevirtual`**. Since `icl`'s actual class overrides
`loadClass(String)` with real bytecode, this dispatches directly to that
override (call #1) *without* ever going through any CratonVM native. Inside the
override, `super.loadClass(name)` is an `invokespecial` targeting
`java.lang.ClassLoader.loadClass(String)`, which CratonVM has no bytecode for —
this reaches the native `cl_real_load_class`
(`native-builtins/src/classloader_real.rs:827`).

`cl_real_load_class` first calls
`invoke_single_load_class_override(ctx, this, name_obj)`
(`native-builtins/src/classloader.rs:1166`), which exists specifically to
redirect a *native-first* entry (e.g. `Class.forName`'s own
`ctx.invoke_virtual(loader, "loadClass", ...)` landing on the registered native
before any bytecode call has "warmed" dispatch toward the receiver's override —
see `reference_invoke_virtual_native_dispatch_cache_quirk` for the general
mechanism) to the receiver's real override, guarded by a thread-local
"in-flight identity" set (`SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT`) so the
override's own `super.loadClass(name)` re-entry doesn't redirect back to
itself infinitely:

```rust
pub(crate) fn invoke_single_load_class_override(ctx, this, name_obj) -> Option<MethodCallResult> {
    if !receiver_overrides_load_class_single(ctx, this) { return None; }
    let identity = ctx.identity_hash_code(this);
    let reentrant = SINGLE_LOAD_CLASS_OVERRIDE_IN_FLIGHT.with(|active| { /* push if absent */ });
    if reentrant { return None; }
    let result = ctx.invoke_virtual(this, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[...]);
    /* pop */
    Some(result)
}
```

The guard's assumption is that the **first** time this identity is seen inside
`cl_real_load_class` is also the **first** execution of the override overall —
true when the outer call arrived via the native (`Class.forName`'s path), false
in our repro: the override already ran once via plain bytecode dispatch from
`AggregatedClassLoader.findClass` *before* `cl_real_load_class` was ever entered.
The guard's in-flight set is empty on that first entry (nothing pushed it,
since the earlier bytecode-level call never touched this function), so it
concludes "override not yet dispatched", pushes the identity, and calls
`ctx.invoke_virtual(this, "loadClass", ...)` again — re-running the override's
entire body a second time (call #2) with its side effects (the access-count
append) duplicated, before finally returning up through the original
super-call frame.

This function was added in commit `5a7ccb810` ("fix(spring): honor modified
classpath presence checks", 2026-07-18) for Spring Boot's
`ModifiedClassPathClassLoader`, **after** the HIB-CV-24 fix (`579c4836b`,
2026-06-30) that made `testLookupBefore` pass. It has no visibility into
whether the override it is about to (re-)dispatch already executed via an
external, non-native call path — that blind spot is the regression.

### Fix suggestion

`invoke_single_load_class_override`'s reentrancy guard needs to distinguish
"native-first entry, override not yet run" from "override already running via
an external bytecode call, now reaching its own `super.loadClass` delegation".
One option: instead of (or in addition to) the in-flight identity set, thread a
"we are inside this receiver's own `loadClass(String)` override frame" signal
from the *bytecode* dispatch side (e.g. a per-thread marker set when
`invoke_virtual`/the interpreter dispatches an *ordinary* call to a method the
VM knows is native-shadowed) so the native can tell these two cases apart
without redundantly re-invoking real bytecode that is already on the call
stack. This needs VM/interpreter-side understanding of the actual call path,
not just a native-local guard — same family of issue as
`reference_invoke_virtual_native_dispatch_cache_quirk` and
`reference_dual_dispatch_gate_native_override`.

### Not affected

`testSystemClassLoaderNotOverriding` (`service.ClassLoaderServiceImplTest`, HIB-CV-24 14a)
still passes — that fix (`find_loaded_class_for_loader` first-check in
`cl_real_load_class_base`) is unrelated to this double-dispatch path and is
unaffected.

---

## Summary

Both failures are **regressions** of Hibernate ClassLoaderServiceImplTest bugs
previously fixed under HIB-CV-16/HIB-CV-24 (`579c4836b`, 2026-06-30), not
reverted fixes and not new discoveries in the "never seen before" sense — but
each now presents a **different symptom** than what was originally fixed,
caused by independent, later changes:

- Bug 1 (Windows `file:` URL path bug in `service_loader.rs`) was likely always
  latent — the original 14b fix's verification did not cover Windows
  drive-letter paths.
- Bug 2 (double-dispatch of a user loader's `loadClass(String)` override) was
  introduced 2026-07-18 by commit `5a7ccb810`, an unrelated Spring Boot
  classloader fix, which added a reentrancy guard with a blind spot for
  externally-triggered (non-`Class.forName`) entry into the override.

Neither has been fixed here (no source changes made per this investigation's
scope); this document records root cause and repro for follow-up.

---

## Resolution (2026-07-21)

**Status: FIXED.**

- Plain `file:` SPI descriptor URLs now use the shared
  `file_url_path_to_fs_path` conversion. This preserves `C:/...` drive-letter
  paths while retaining POSIX absolute paths and percent decoding.
- The real-JDK `ClassLoader.loadClass(String)` native shadow now recognizes an
  already executing bytecode override on the same receiver. Its nested
  `super.loadClass(name)` delegation reaches the base path instead of invoking
  the override a second time. Native-first dispatch remains protected by the
  existing in-flight identity guard.

Regression coverage:

- `file_url_path_preserves_windows_drive_letter` covers both `file:/C:/...`
  and percent-decoded POSIX paths.
- `ClassLoaderSingleOverrideDispatch` proves exact-once behavior for both
  native-first `Class.forName` and bytecode-first delegated lookup.

Azure verification with `/data/cratonvm-hibernate-classloader-20260721` and
JDK 25 completed in JIT and `--nojit` modes:

- `service.ClassLoaderServiceImplTest`: `found=2 started=2 ok=2 failed=0`.
- `bootstrap.registry.classloading.ClassLoaderServiceImplTest`:
  `found=7 started=7 ok=7 failed=0`.

The runner's `Failed to close extension context` line remains emitted after
successful methods in both modes; it is a harness cleanup diagnostic and did
not produce any failed, aborted, or skipped test.
