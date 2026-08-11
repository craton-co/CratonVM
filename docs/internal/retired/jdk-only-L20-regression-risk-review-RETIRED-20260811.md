> **RETIRED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> An adversarial source review, not a defect record: it owns no corpus vector, changed nothing, and returned SAFE on all seven change clusters. Its question — *can this campaign make something that works today stop working?* — was answered by execution on 2026-08-07: the strict corpus went to 53 passed / 1 failed with Compatible mode unchanged at 30/1, so none of the seven clusters regressed anything. Its two patches were both conditional ("apply **only** if the Tomcat `catalina.webresources` cluster regresses"; "not required for `RJdkForkJoin`") and neither condition fired.
>
> Previous location: `docs/known-issues/jdk-only/L20-regression-risk-review.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# L20 — adversarial regression review of the JDK-only wave-2 campaign

**Status:** SOURCE REVIEW ONLY, 2026-08-06. Nothing built, nothing run. Seven
change clusters reviewed against the 37 currently-PASSING classes of
`regression-suite/run.sh`. No fixes applied; one optional patch proposed.

Question asked of every item: *can this make something that works today stop
working?* An item is `SAFE` only when there is a named mechanism that bounds
the change, not merely an absence of evidence.

## Verdicts

| # | Change | Verdict | Load-bearing evidence |
| --- | --- | --- | --- |
| 1 | `lambda_proxy_marker_satisfies` in `typecheck.rs:285` | **SAFE** | `invokedynamic.rs:1373` `ANY_LAMBDA_MARKERS` |
| 2 | `caller_may_access_member` in `lang_class.rs:603` / `:7426` | **SAFE** | `lang_class.rs:613` fail-closed fallthrough |
| 3 | static `ForkJoinTask.invokeAll` ×3 | **SAFE** (one completeness gap) | three lists agree entry-for-entry |
| 4 | `Files.copy` `FileAlreadyExistsException` | **SAFE, residual** | guard fires only without `REPLACE_EXISTING` |
| 5 | `lang_invoke.rs` placeholder delete + markers + cache | **SAFE** | both modes still register `defineHiddenClass` |
| 6 | JEP 371 flag bits in `lang_system.rs:4092-4093` | **SAFE** | matches `classloader.rs:4048-4050` |
| 7 | real `getsockopt`/`setsockopt` in `net.rs` | **SAFE** | `net.rs:2795-2812` three-step fallback |

No compile errors found. Details below.

---

## 1 — `lambda_proxy_marker_satisfies` (SAFE)

**Can it answer `true` for a non-marker lambda?** No. The first thing
`lambda_proxy_marker_interfaces` (`vm/src/runtime/invokedynamic.rs:1418`) does
is read `ANY_LAMBDA_MARKERS` (`:1373`), a process-global `AtomicBool` that is
only ever set by `record_lambda_proxy_markers` (`:1397`), which returns early
for an empty marker list. A plain `metafactory` lambda therefore costs one
relaxed atomic load and returns `false` before the mutex is touched. Even after
some other proxy flips the flag, the table is keyed `(vm_identity,
proxy_class_id)` so a non-marker proxy misses.

**Double-lock / re-entrancy.** Three locks are in play and none overlaps:

* `shared.classes.lambda_proxies` (RwLock, read) is released by the explicit
  `drop(proxies)` at `typecheck.rs:246`, *before* the new call at `:285`;
* `LAMBDA_PROXY_MARKERS` (parking_lot Mutex) is taken and released inside the
  single expression at `invokedynamic.rs:1425-1428` — the guard is a temporary
  that dies at the end of the `let … else`, so it is **not** held across the
  `load_class_concurrent` at `:1452`;
* `class_manager.read()` at `:1456` is a temporary bounded by the `if`
  condition.

A `<clinit>` run by `load_class_concurrent` that re-enters `instanceof` on a
lambda proxy therefore re-acquires a free mutex. No deadlock.

**Class load during a type test.** `load_class_concurrent` can run Java. It is
reached only for a proxy that recorded markers (rare — intersection casts only)
and only after the direct name comparison at `:1449` misses. The pre-existing
code two lines below at `typecheck.rs:298` already calls
`load_class_concurrent(&iface_name)` on *every* lambda type test, so this is not
a new class of hazard on this path.

**Loader fidelity.** `&**marker == target_name` compares binary names only,
ignoring the loader — the same relaxation `iface_name.as_ref() == target_name`
(`typecheck.rs:294`) already makes. Consistent, and scoped to marker-bearing
proxies.

Endangers: nothing. `RJdkLambdas` / `RLambdaDefaultOverload` /
`RPrivateLambdaOwner` are the only marker-plausible classes and the gate is off
for all of them unless a marker was actually recorded.

## 2 — `caller_may_access_member` on the reflection paths (SAFE)

**Pure widening?** Yes, in both places.

* `check_field_access` (`native-builtins/src/lang_class.rs:580`): the early
  `accessible || ACC_PUBLIC` return at `:588` is untouched; the new arm at
  `:596-613` can only `return Ok(())`; every path that does not take it reaches
  the unchanged `check_access(modifiers, false, member_desc)` at `:613`.
* `native_method_invoke` (`:7407-7437`): the new arm only sets
  `caller_entitled`; `check_access` still runs, unchanged, when it is `false`.

**`resolve_caller_class_id == None`.** Fails **CLOSED** in both. In
`check_field_access` the `if let Some(caller)` at `:597` simply does not fire and
`check_access` runs. In `native_method_invoke` the `match` at `:7424-7429` has
`_ => false`. Correct.

**GC hazard.** None. `field_get`/`field_get_raw` lift `receiver` out of `args`
(`lang_class.rs:5491`, `:5715`) *before* the check, so a Java re-entry inside it
would leave a stale local. It cannot re-enter: every `NativeContext` method
`caller_may_access_member` reaches resolves to a `&self` class-manager read —
`class_id_by_name_near` → `find_class_by_name_for_class` (`vm_exec.rs:6878`),
`nest_host_name` (`:7082`), `nest_member_names` (`:7091`) — none of which loads
a class or runs a `<clinit>`. `runtime_package_of`
(`lang_reflect.rs:1288`) is `class_name_of_id` + `loader_id_of_class`, both the
same shape.

**Performance cliff.** No. The arm is behind `!accessible && !is_public`, so
public reflection (the framework hot path) still short-circuits. On the
non-public path `resolve_caller_class_id` was *already* being called — by the
old declaring-class check in `check_field_access`, and by
`check_reflection_module_access` (`lang_class.rs:780`) immediately after
`native_method_invoke`'s check. Worst case is one extra frame walk on a path
that was already walking frames.

**Over-widening check.** `runtime_package_of` returns `(package, loader_id)`, so
"same runtime package" is loader-aware (`lang_reflect.rs:1295`) and a
default-package regression-suite class cannot reach a JDK package-private
member. `confirmed_nest_host_name` (`:1310`) requires the claimed host to list
the claimant back, per JVMS §5.4.4 — a spoofed `NestHost` fails.

**Negative assertions in the passing set.** Grepped every
`IllegalAccessException` in `regression-suite/src/`: `RJdkHandles:187,198`
(MethodHandles, different path), `RJdkModule:169`, `RJdkReflect:176` (all
`setAccessible`-gated, and `setAccessible` short-circuits before this arm) and
`RSocketChannelInterrupt:56` (uses `setAccessible(true)`, so `accessible` is
already `true` and the arm is unreachable). Nothing in the passing set asserts
that a *non*-`setAccessible` non-public access must fail.

## 3 — static `ForkJoinTask.invokeAll` ×3 (SAFE, one completeness gap)

**Do the lists agree?** Yes, entry-for-entry, for all three descriptors:

| descriptor | native | `keep_real_forkjointask_bridge` | `is_forkjoin_native_override` |
| --- | --- | --- | --- |
| `(Ljava/util/concurrent/ForkJoinTask;Ljava/util/concurrent/ForkJoinTask;)V` | `concurrent.rs:6797` | `registry.rs:5595` | `native_override.rs:1713` |
| `([Ljava/util/concurrent/ForkJoinTask;)V` | `concurrent.rs:6820` | `registry.rs:5599` | `native_override.rs:1717` |
| `(Ljava/util/Collection;)Ljava/util/Collection;` | `concurrent.rs:6849` | `registry.rs:5600` | `native_override.rs:1718` |

All three are byte-exact for JDK 25 (`invokeAll(ForkJoinTask,ForkJoinTask)` →
`void`; varargs → `void`; `<T extends ForkJoinTask<?>> Collection<T>
invokeAll(Collection<T>)` → `Collection`). The `awaitQuiescence` precedent
(`registry.rs:5510-5520`) is not repeated.

The registration is reachable in real-JDK mode: `register_t19_k3_forkjoinpool_common`
(`concurrent.rs:6887`) calls `register_forkjointask_invoke_all_bridge`, and both
that and `register_new15_loom` set `NativeKind::Bridge`, which is what
`keep_real_forkjointask_bridge` (`registry.rs:5564`) requires to survive the
real-FJP drop.

**Does intercepting the static `invokeAll` change a working workload?** No class
in the passing set reaches it. `regression-suite/src/*.java` mentions
`invokeAll`/`ForkJoin`/`parallel` in exactly two files: `RJdkForkJoin` (the
target of the fix) and `RJdkExecutors:132`, whose `ex.invokeAll(batch)` is
`ExecutorService.invokeAll(Collection)` — a different owner class that
`is_forkjoin_native_override` does not match. Parallel streams do not route
through `ForkJoinTask.invokeAll` (`AbstractTask.compute` uses `fork()` +
`compute()`; `Arrays.parallelSort` uses `CountedCompleter`).

Semantics are conservative: `fjt_invoke_all_inline` (`concurrent.rs:6739`) drives
each task through `join()`, which memoises and rethrows unwrapped, and aborts
the batch on the first failure — matching `reportExecutionException`. Null
elements are rejected before anything runs, matching the JDK.

**Gap (not a regression, but the fix is incomplete).** Both allow-lists name
`RecursiveTask` and `RecursiveAction` as owners, but the natives are registered
**only** on `java/util/concurrent/ForkJoinTask` (`concurrent.rs:6793`). A call
site whose constant-pool owner is `RecursiveAction`/`RecursiveTask` will be
force-routed, find no registration, and (per
`intercept_force_registered_native`'s `Ok(None)` contract,
`native_override.rs:5340`) fall back to bytecode — i.e. the original hang would
persist there. Every sibling task-family native handles this by looping over all
three names; see the `getException` registration at `phases_early.rs:9375-9392`.
Harmless today (javac emits `ForkJoinTask` as the owner for an unqualified
static call), but worth the same loop.

## 4 — `Files.copy` `FileAlreadyExistsException` (SAFE, one residual)

**Guard shape** (`native-builtins/src/phases_late/nio_file.rs:4874-4880`):
correct on all four axes.

* `symlink_metadata`, not `metadata` — matches `UnixCopyFile`'s NOFOLLOW
  existence probe, so a dangling symlink at the target counts as existing.
* jarfs destination excluded (`jarfs_decode(&dst_path).is_none()`), so Quarkus's
  `ZipUtils.unzip` path is untouched.
* self-copy excluded, though by **string** comparison; HotSpot compares by
  device/inode, so `Files.copy(Path.of("x"), Path.of("./x"))` would throw here
  and be a no-op on HotSpot. Not exercised by anything in the suite.
* GC-safe: `copy_options_replace_existing` (the only Java re-entry) runs before
  any `ObjectRef` is lifted out of `args`, and `p57_file_already_exists` — which
  does re-enter Java — is on an immediate-`Err` path where `dst` is never used
  again.

**Directory source.** The guard runs *before* the directory arm whose
`Err(AlreadyExists) => Ok(())` tolerance (`:4919-4924`) exists specifically for
`TomcatBaseTest.recursiveCopy`. That tolerance is now reachable only *with*
`REPLACE_EXISTING`. That is exactly what HotSpot does — `Files.copy(dir,
existingDir)` with no options throws `FileAlreadyExistsException` there too — and
Tomcat's `recursiveCopy` must already pass `REPLACE_EXISTING`, or it would fail
on a real JVM the moment the destination temp directory exists. Verdict: SAFE.

**Residual (unverifiable from this tree).** Tomcat's sources are not vendored
here (`apps/tomcat-suite-runner` is a runner only, and `recursiveCopy` appears
in no `.java` in the repo), so the `REPLACE_EXISTING` claim rests on the JDK
contract rather than on the actual call site. If the Tomcat
`catalina.webresources` cluster regresses after this lands, this guard is the
first suspect and the patch below is the fix.

**Hygiene.** `docs/known-issues/.../L5-…md` marks its Patch C
(`native-io/src/lib.rs:12268`) OPTIONAL and it was **not** applied: that
shadowed second `Files.copy` registration still has no guard. Harmless while
`register_phase57_nio_file` runs last, but it means `RJdkNio:102` would silently
regress if registration order ever flipped.

## 5 — `lang_invoke.rs` (SAFE)

**(i) Deleted `defineHiddenClass` placeholder.** No mode is left without a
registration:

* real-JDK / `--jdk-only`: `lookup_define::register_lookup_define_class` runs on
  the essentials path via `register_annotation_overrides`
  (`reflect_annotations.rs:419`, reached from
  `register_essential_natives_with_shims`, `lib.rs:18658`);
* synthetic-jdk: `classloader::register_classloader_natives` registers
  `lk_define_hidden_class` (`classloader.rs:9084`) and
  `lookup_define::register_lookup_define_class` overrides it immediately after
  (`lib.rs:23552` then `:23562`).

With the placeholder gone, the real WP2.3-B implementation is the last writer in
both. No test asserts the placeholder's existence
(`classloader.rs:11219`'s `find` assertion targets `register_classloader_natives`'
own registration).

`hidden_class_base_name` (`lookup_define.rs:390`) compiles against the reader:
`cratonvm_reader::read_class(&[u8]) -> Result<ClassFile, ClassReaderError>`
(`reader/src/class_reader.rs:75`) and `ClassFile::this_class: Arc<str>`
(`reader/src/class_file.rs:27`), so `.to_string()` is valid.
`cratonvm-reader` is already a `native-builtins` dependency
(`native-builtins/Cargo.toml:54`) and is used the same way at
`lang_system.rs:3659`.

**(ii) `marker_interfaces: &[String]`.** Both call sites updated
(`lang_invoke.rs:5113` passes `&[]`, `:5230` passes `&marker_names`). The
hand-off chain type-checks end to end: `NativeContext::register_lambda_proxy_markers(u32,
&[String])` (`registry.rs:622`, defaulted so mock contexts are unaffected) →
`vm_exec.rs:6697` maps to `Vec<Arc<str>>` → `record_lambda_proxy_markers(usize,
ClassId, &[Arc<str>])` (`invokedynamic.rs:1397`). `ClassId::new(u32)` exists
(`types/src/class_id.rs:139`).

**(iii) `let cacheable = flags == 0`.** Not a leak vector. This is the
*reflective* `altMetafactory` only — the `invokedynamic` opcode is handled
inline in `runtime/invokedynamic.rs` and never reaches this native
(`lang_invoke.rs:5070-5072`), so the callers are log4j2's `ServiceLoaderUtil`
and the Elasticsearch CLI bootstrap: bounded, not a per-call-site loop. Proxy
ids are capped at `MAX_LAMBDA_PROXIES = 100_000` (`vm_init.rs:7`) and the
`CallSite` objects are ordinary heap objects. It is also strictly *safer* than
the old behaviour with respect to `LAMBDA_CALLSITE_CACHE`, whose keys are raw
`ObjectRef` addresses (`lambda_key_of`, `lang_invoke.rs:5664`) — fewer entries,
fewer stale-address keys.

## 6 — JEP 371 flag bits (SAFE)

`lang_system.rs:4092-4093` now reads `nestmate = flags & 0x1`, `hidden = flags &
0x2`. That matches JDK 25 `MethodHandleNatives.Constants` (`NESTMATE_CLASS =
0x01`, `HIDDEN_CLASS = 0x02`, `STRONG_LOADER_LINK = 0x04`) and the constants the
sibling implementation has carried all along
(`classloader.rs:4048-4050`, `DEFINE_CLASS0_FLAG_*`).

The path is `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int,
int, ProtectionDomain, boolean, int, Object)`, which is registered **twice** —
`lang_system::native_classloader_define_class0` (`lib.rs:15669`) and
`cl_define_class0` (`classloader.rs:4217`). Before this change the two disagreed
about the bit layout, so which one won mattered; now they agree, which removes a
registration-order dependency rather than creating one.

Nothing can have been compensating: grepped for any test or constant pinning the
old decoding — there is none. The old decoding made `defineHiddenClass()` with no
`ClassOption` (flags `0x2`) come out `hidden = false`, i.e. the class was defined
under its real name and collided; `ClassOption.STRONG` (`0x4`) came out
`nestmate = true`, which wrongly attached a nest host. Both are strictly wrong,
and no passing class calls `Lookup.defineHiddenClass` (the callers are
`RJdkHidden` and `RJdkStrict`, both in the failing set).

## 7 — real `getsockopt`/`setsockopt` (SAFE)

**What does `getIntOption0` answer when `try_lock` fails?** Not `0`.
`with_net_raw_socket` returns `None` (`net.rs:2562`) and
`net_get_int_option0` falls through its remaining two steps
(`net.rs:2805-2812`): the value `setIntOption0` last recorded, then the
platform default probed from a throwaway loopback listener and cached per
`(level, opt)`. A bare `Value::Int(0)` is reached only when the option was never
set *and* the platform probe itself failed — strictly better than the old
"answer everything from the request cache" behaviour that produced `SO_RCVBUF ==
0`.

**Extern declarations.** The Windows `sockopt_sys` block (`net.rs:2441-2446`) is
signature-identical to the `ext_opt_sys` block (`net.rs:3236-3242`):
`getsockopt(usize, i32, i32, *mut u8, *mut i32) -> i32`,
`setsockopt(usize, i32, i32, *const u8, i32) -> i32`,
`WSAGetLastError() -> i32`. `clashing_extern_declarations` compares signatures,
not the `unsafe extern` vs `extern` spelling, so the mixed spelling is fine (both
are legal in edition 2021, workspace `Cargo.toml:16`). Unix uses `libc`, no
declaration at all.

`l.try_lock()?` in a `-> Option<R>` function compiles because the mutex is
`parking_lot::Mutex` (`net.rs:23`), whose `try_lock` returns `Option`.

**Blast radius.** `RJdkNet` is the only class in the whole suite that touches
socket options at all. `RChannelInterrupt` / `RSocketChannelInterrupt` bind
channels but read no options; the only new cost they could see is the one-time
`TcpListener::bind(("127.0.0.1", 0))` default probe, which is cached per
`(level, opt)` and closed at the end of the statement.

## Compile check

Every symbol introduced or re-typed by the campaign was resolved against its
definition. No arity, type, trait-object-coercion, or move/borrow error found.
Specifically confirmed: `lambda_proxy_marker_satisfies`,
`record_lambda_proxy_markers`, `register_lambda_proxy_markers` (three
definitions, two call sites), `hidden_class_base_name` vs
`cratonvm_reader::read_class`, `FjtPinnedTasks`'s `&mut dyn` → `&dyn` coercions
and its move-in-a-diverging-loop-branch, and `parking_lot`'s `Option`-returning
`try_lock`.

**Formatting note (not a build blocker).** `cargo build` does not care, but CI
step 1 is `cargo fmt --all --check` (`.github/workflows/ci.yml:56`). Two of the
edited hunks are not rustfmt output —
`native-builtins/src/lang_invoke.rs:6129-6132` (the `);` and the following `if`
are over-indented) and `native-builtins/src/lookup_define.rs:442-443` (fits on
one 99-column line, so rustfmt would join it). The workspace is already not
rustfmt-clean (e.g. `lookup_define.rs:210` is 108 columns of code), so this
changes nothing about the gate's colour.

## Optional patch — exempt a directory source from the `Files.copy` guard

Apply **only** if the Tomcat `catalina.webresources` cluster regresses. It trades
HotSpot fidelity for the recursive-copy workload; `RJdkNio:94-103` exercises only
the file source, so it still passes either way. Apply A then B.

**A —** `native-builtins/src/phases_late/nio_file.rs`

*old*
```rust
            if !replace_existing
                && src_path != dst_path
                && jarfs_decode(&dst_path).is_none()
                && std::fs::symlink_metadata(&dst_path).is_ok()
            {
                return Err(p57_file_already_exists(ctx, &dst_path));
            }
```
*new*
```rust
            let src_jarfs = jarfs_decode(&src_path);
            let src_is_dir = if let Some((ref jar, ref entry)) = src_jarfs {
                matches!(jarfs_classify(jar, entry), JarFsKind::Dir)
            } else {
                std::fs::symlink_metadata(&src_path)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            };
            // A DIRECTORY source is exempt. `Files.walkFileTree` recursive-copy
            // visitors call `Files.copy(dir, …)` onto a destination tree that
            // already exists; the directory arm below tolerates that on purpose
            // and re-throwing here would undo the `catalina.webresources` fix.
            if !replace_existing
                && !src_is_dir
                && src_path != dst_path
                && jarfs_decode(&dst_path).is_none()
                && std::fs::symlink_metadata(&dst_path).is_ok()
            {
                return Err(p57_file_already_exists(ctx, &dst_path));
            }
```

**B —** same file, remove the now-duplicated computation

*old*
```rust
            let src_jarfs = jarfs_decode(&src_path);
            let src_is_dir = if let Some((ref jar, ref entry)) = src_jarfs {
                matches!(jarfs_classify(jar, entry), JarFsKind::Dir)
            } else {
                std::fs::symlink_metadata(&src_path)
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            };
            let result = if let Some((dst_jar, dst_entry)) = jarfs_decode(&dst_path) {
```
*new*
```rust
            let result = if let Some((dst_jar, dst_entry)) = jarfs_decode(&dst_path) {
```

## Optional patch — close the `ForkJoinTask.invokeAll` owner gap

`native-builtins/src/phases_late/concurrent.rs`: wrap the three
`r.register_with_kind(fjt, …)` calls in the same
`for task_class in ["java/util/concurrent/ForkJoinTask",
"java/util/concurrent/RecursiveTask", "java/util/concurrent/RecursiveAction"]`
loop the sibling `getException` registration uses
(`phases_early.rs:9375-9392`), so a call site whose constant-pool owner is a
`Recursive*` class is covered by the allow-lists that already name it. Not
required for `RJdkForkJoin`.
