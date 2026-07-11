# Spring CompiledTests handoff - 2026-07-11

## Scope

Current cluster: `org.springframework.core.test.tools.CompiledTests` from `docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST-125.md`.

Remote worktree: `/data/data/cratonvm-worktrees/20260710-162727-spring-compiled-tests`

Branch: `codex/spring-compiled-tests-20260710-162727`

Unique binary under test: `/data/data/cratonvm-worktrees/20260710-162727-spring-compiled-tests/target/debug/cratonvm`

Status: not committed, not merged to `dev`, not pushed. Previous clusters before this one were pushed to `origin/dev`; this cluster was stopped mid-investigation at the user's request.

## Current git state

Expected modified files:

- `native-builtins/src/lib.rs`
- `native-builtins/src/classloader.rs`
- `vm/src/runtime/interpreter.rs`
- this handoff file

`native-builtins/src/lang_system.rs` should be clean again after removing earlier `defineClass1` traces.

## Verified so far

Build and focused Rust guard passed after the javac/FileManager work and before the latest temporary loader tracing:

```text
cargo test -p cratonvm-vm --lib standard_location_force_native_covers_javac_regex_shortcut -j1 -- --nocapture
# 1 passed

cargo build --bin cratonvm -j1
# succeeded
```

After the latest `findBootstrapClass` patch and temporary traces, `cargo build --bin cratonvm -j1` also succeeded.

The original Spring timeout was removed. Earlier `CompiledTests` went from timeout to:

```text
RESULT org.springframework.core.test.tools.CompiledTests found=14 succ=13 fail=1 skip=0 abort=0 ms=126204 status=FAIL
FAILCAUSE ... getInstanceWhenNoDefaultConstructorThrowsException() :: java.lang.AssertionError: Expecting code to raise a throwable.
```

The remaining failure is class-loader isolation for repeated dynamic definitions of `com.example.HelloWorld`.

## Implemented candidate fixes

### javac/FileManager path

`native-builtins/src/lib.rs` registers and implements fast natives for these javac hot paths:

- `javax/tools/StandardLocation.computeIsModuleOrientedLocation(Ljava/lang/String;)Z`
- `com/sun/tools/javac/file/JavacFileManager.checkNotModuleOrientedLocation(Ljavax/tools/JavaFileManager$Location;)V`
- `com/sun/tools/javac/file/JavacFileManager.list(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;Ljava/util/Set;Z)Ljava/lang/Iterable;`
- `com/sun/tools/javac/file/RelativePath.hashCode()I`
- `RelativePath.equals(Ljava/lang/Object;)Z`
- `RelativePath.compareTo(Lcom/sun/tools/javac/file/RelativePath;)I`
- `RelativePath.getPath()Ljava/lang/String;`

`vm/src/runtime/interpreter.rs` forces those methods over real JDK bytecode and includes the targeted unit test `standard_location_force_native_covers_javac_regex_shortcut`.

This work eliminated the hard timeout in Spring's in-memory javac tests.

### loader isolation path

A minimal null-parent sibling loader probe passes:

```text
same=false
c2 ctors=[ p.C(java.lang.String)]
c2 zero=NoSuchMethodException
```

A parented sibling loader probe reproduces the Spring failure before the latest unverified `findBootstrapClass` patch:

```text
same=true
c1 loader=SameNameLoaderParentedProbe$BytesLoader@ca
c2 loader=SameNameLoaderParentedProbe$BytesLoader@ca
c2 ctors=[ p.C()]
c2 zero= p.C()
```

Candidate changes currently in `native-builtins/src/classloader.rs`:

- added `classloader_parent(ctx, loader)` to read real-JDK `parent` by field name first, then fall back to synthetic `CL_PARENT_REF`
- changed `loader_can_see_defining`, base delegation parent detection, parent delegation, and `cl_get_parent` to use that helper

This did not fix the parented probe by itself.

Candidate change currently in `native-builtins/src/lib.rs`:

- `native_classloader_find_bootstrap_class` now rejects a mirror when `ctx.class_id_from_mirror(mirror)` has a recorded `crate::classloader::defining_loader_for(class_id)`. Rationale: `findBootstrapClass` must not return a user/application-loader-defined dynamic class from CratonVM's flat global class store; otherwise parent delegation leaks sibling loader classes before the child loader's `findClass` can define its own copy.

This latest `findBootstrapClass` change built successfully but was not yet probed because the user asked to stop.

## Temporary traces still present

Remove these before committing:

- `native-builtins/src/classloader.rs`
  - `[loader-vis-trace]` in `resolve_global_if_visible`
  - `[load-base-trace]` at base delegation entry
  - `[load-base-trace] own-namespace-hit ...`
  - `[load-base-trace] parent-namespace-hit ...`
- `native-builtins/src/lib.rs`
  - `[load-simple-trace]` in the legacy/simple `native_classloader_load_class`

Earlier temporary traces already removed:

- `[defineClass1-trace]`
- `[loadClass-rich-trace]`
- `[loadClass-simple-trace]` from the earlier simple trace wording
- `[findLoadedClass0-trace]`

## Next recommended steps

1. Rerun the parented sibling loader probe after the `findBootstrapClass` patch.

Expected if the patch works:

```text
same=false
c2 ctors=[ p.C(java.lang.String)]
c2 zero=NoSuchMethodException
```

2. If the parented probe passes, remove the temporary traces listed above.

3. Rebuild and rerun:

```text
cargo test -p cratonvm-vm --lib standard_location_force_native_covers_javac_regex_shortcut -j1 -- --nocapture
cargo build --bin cratonvm -j1
```

4. Rerun Spring `CompiledTests` with the existing per-class harness pattern:

```text
/data/data/cratonvm-probes/<new-dir>/recheck_one.py org.springframework.core.test.tools.CompiledTests 240
```

The harness should point to:

```text
BASE=/data/data/spring-framework-shared
RUNNER=/data/data/spring-suite-runner-shared
JAVA_HOME=/data/data/jdk25-real
VM=/data/data/cratonvm-worktrees/20260710-162727-spring-compiled-tests/target/debug/cratonvm
```

5. If `CompiledTests` passes 14/14, inspect the diff, commit this branch, then push branch and `HEAD:dev` from this worktree without switching `/data/data/cratonvm` off `dev`.

## Useful probe locations

Recent logs/probes:

- `/data/data/cratonvm-probes/spring-compiled-tests-clean-20260711-011931/org_springframework_core_test_tools_CompiledTests.log`
- `/data/data/cratonvm-probes/same-name-loader-parented-20260711-012620`
- `/data/data/cratonvm-probes/same-name-loader-parented-after-parentfix-20260711-021926`
- `/data/data/cratonvm-probes/same-name-loader-parented-branchtrace-20260711-022428`

## Notes

The main `/data/data/cratonvm` worktree must stay on `dev`. Continue using the separate worktree above.

Do not push the current branch until the temporary traces are removed and `CompiledTests` has been reverified.
