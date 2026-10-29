# t1 W13: a `-cp` / `URLClassLoader` jar class load is still ~10x HotSpot: define bookkeeping, the `forName` door, the built-in loaders' package sync

Filed 2026-10-28 by t1 wave 13, lane L71, while retiring the two t1 L71 pages
(`docs/internal/fixed-bugs/interpreter/t1-L71-jar-get-entry-bimodal-and-define-native-cost-FIXED-20261028.md`,
`docs/internal/fixed-bugs/interpreter/t1-L71-loader-path-jdk-bodies-stay-interpreted-FIXED-20261028.md`).
What they named is fixed, refuted or root-caused. This page carries the loader-side costs that are left;
the JIT-side ones are three pages under `docs/known-issues/jit/t1-w13-*`. Measured on the Linux host 64.177
(dev 214b45112c + the wave-13 branch, `--jdk-only`, HotSpot 25.0.4.1 interleaved); not fixed.

## Where the time is

`L5W117JarClassPhaseSplitBench` (us a class): `e2e` 295 (HotSpot 30), `remainder` 227 (8.3), `define` 35
(8.2), `link-init` 13.5 (3.6), `find-resource` 14.6 (2.1), `delegate-miss` 15.3 (4.0), `read-inflate` 11.5
(9.7), `get-entry` 8.7 (0.23; see the JIT getEntry page).

The perf profile (frame-pointer build, `T1L71JarRemainderSplit`, 27 rounds, 10 064 samples) is flat: no
symbol above 2.4 %. The main share is 4 091 samples of interpreter time entered from compiled code:

1. **The `Class.forName` native's `loadClass` re-entry, 1 638 samples (16 %).** `native_class_for_name` →
   `invoke_load_class_as_the_vm_publishing` → `invoke_or_native_impl_walk` → `invoke_by_class_id_shared` →
   `interpreter::execute`. No compiled frame appears between the `[c1]`/`[c2]` `Class.forName` frame and
   the interpreter, so the loader's `loadClass` chain under this door runs as bytecode. HotSpot's
   `forName0` calls `loadClass` through `JavaCalls`, into compiled code. Next: find which method of the
   chain `execute` interprets (`CRATONVM_DBG=interp-frames` restricted to the `fn-true` row; it is not in
   `jit_skip_set`, whose only seals on this path are `<clinit>`s) and why `get_for_reentry` misses it.
2. **Virtual calls from compiled callers, served interpreted by `try_jit_virtual_bytecode_callee`:**
   `URLClassLoader.defineClass` 350, `PlatformClassLoader.loadClass` 317, `JarLoader.getResource` 92. The
   synchronized ones are the JIT wrapped-body page's; for the others,
   `bytecode_callee_compiled_or_nominate` found no published body for the key at those calls.

## The define native (`define-plain` 29.6 us, HotSpot 7.2; `define-cs` 38.9 us, 7.9)

`CRATONVM_DBG=define-census`, 21 600 defines (shares; the run was under load): `native-total` 71 us =
`class-manager-define` 28.7 (`register` 11.5: `store-add` 4.6, `vtable-install-hook` 2.9 of which
`install-convert` 1.5; `define-pre-checks` 3.7; `cm-write-acquire` 2.2; `parse` 2.1; `define-build` 1.8)
+ `supertype-prepass` 11.1 (`prepass-parse` 5.2: the pre-pass parses the class for its supertype
names) + `mirror-reserve` 5.3 + `native-entry` 3.2 + `vm-pre-define` 1.3 + ~21 us outside every named row
(the bind / settle steps, `add_defined_class_to_loader` and its `ClassLoader.addClass` upcall: 93 samples in
the profile). Candidates: check what the define's own `parse` row (2.1 us) still does with
`CRATONVM_DEFINE_ONE_PARSE` on, which hands it the pre-pass's parse. Put the unnamed ~21 us under census
rows before cutting it.

## Built-in loaders' package sync at define time (the old page's item 2)

`ClassLoader.getDefinedPackage` is a reviewed `Intrinsic`
(`lang_class::classloader_get_defined_package_synced`). It runs for every receiver because the VM defines a
platform / class-path class without running `BuiltinClassLoader.defineClass`, whose `postDefineClass`
fills the loader's `packages` map on HotSpot. Wave 13 sends a receiver it has nothing to sync for (any
loader but a built-in one) straight to the JDK body (`get-defined-package`, a `URLClassLoader` receiver: 1.46 -> 1.16 us; HotSpot 66-126 ns). What is left there is the `invoke_special_bytecode_only` door itself. A built-in receiver still pays the whole door. Moving the
sync to the VM's define of a built-in loader's class (once per package: the memo row's `in_loader_map`) would
let the Intrinsic go. The JDK body is compiled already (`CRATONVM_DBG=jit-compiled` shows its `put` with and
without `CRATONVM_JIT_DEFINED_PACKAGE_REENTRY_BODY`). Every define path of the application and platform
loaders has to sync, image classes of platform modules included (`image_package_defined_to`), or
`getDefinedPackage` answers `null` where HotSpot answers the package
(`tools/probes/interp/L5/L5W92PlatformLoaderDefinedPackage.java`).

## Verify

`L5W117JarClassPhaseSplitBench` `e2e` / `remainder` / `define` and `T1L71JarRemainderSplit` against HotSpot
on one host, interleaved. The `forName` door: the 1 638-sample bucket of the frame-pointer profile shrinks.
The package sync: `L5W92PlatformLoaderDefinedPackage` and `T1W9IntvShadowPackagesLoader` stay SAME as
HotSpot with the Intrinsic gone.

## 2026-10-29 exact-tree follow-up (OPEN)

`T1L71JarLoadMicro platform-loadclass-miss` still records the probe's `lambda$main$0` as interpreted on
40,000/40,000 misses in both modes. `L5W117JarClassPhaseSplitBench` returned matching deterministic class,
read, define, end-to-end, delegate, and resource facts, but the shared-host e2e rows were HotSpot 167 us/class,
JDK-only 3.24 ms/class, and compatible 2.62 ms/class; the remainder rows were 80.6 us, 2.69 ms, and 2.15 ms.
This confirms the loader-side costs remain and does not pass the `forName`-door, define, or package-sync gates.

## 2026-10-29 final-dev-tree follow-up (OPEN)

On the rebased tree `71c6acdd97bcefce681440e3ff7d1a620699b783`, the class-phase probe preserved all
deterministic rows (`classes=400`, read/resource counts 400, checksums 80200). E2e was 19.9 us/class
on HotSpot, 207 us in JDK-only, and 254 us in compatible; the `remainder` row was 4.28 us, 147 us,
and 188 us respectively. `T1L71JarLoadMicro platform-loadclass-miss` still interpreted its lambda
on 40,000/40,000 calls in both modes, with shared-host times 6.63/7.10 us versus HotSpot 1.23 us.
The timing snapshot is noisy and the interpreter-frame and throughput gates remain open.

## 2026-10-29 exact-source W13 catch/loader follow-up

The exact tree based on current `origin/dev` plus commit `20b71359e9` builds and compiles the
`T1L71JarLoadMicro.lambda$main$0` protected-trap body in C2. At four million checks, all HotSpot,
JDK-only, and compatible checksums matched. HotSpot / JDK-only / compatible times were 2,812 /
14,427 / 12,700 ns/op on the shared host. The lambda recorded 1,118 interpreted warm-up frames in
JDK-only and 4,274 in compatible, with exact diagnostics confirming C2 admission and publication.
The body no longer stays interpreted; the remaining ratio is tracked here with the `Class.forName`
door, define, and package-sync costs.
The more specific W13 compiled-catch issue is retired at
`docs/internal/fixed-bugs/jit/t1-w13-ir-catch-of-a-local-throw-deopts-every-call-FIXED-20261029.md`.
This broader class-phase page stays open because its own e2e, `forName`, define, and package-sync
acceptance gates remain unsatisfied.

## 2026-10-29 exact-source loader phase recheck (OPEN)

On the current task source, `L5W117JarClassPhaseSplitBench` again returned the complete deterministic
set (`classes`, read counts, resource/delegate counts = 400; both checksums = 80200). The fresh
HotSpot / JDK-only / compatible e2e rows were 20.97 / 157.87 / 246.76 us per class, and `remainder`
was 5.37 / 101.84 / 182.87 us. Define was 5.12 / 35.78 / 38.48 us; link-init was 2.96 / 9.58 /
9.44 us. These shared-host numbers retain a substantial gap and are diagnostic, not quiet-host pricing.

The separate 40,000-call `platform-loadclass-miss` row now records 5.81 us/op in JDK-only and
6.28 us/op in compatible versus HotSpot's 1.25 us/op. Its deterministic check stayed 40000. Fresh
interpreter-frame diagnostics show the loader and probe lambda in warm-up frames, while C1/C2 bodies
for `loadClassOrNull` and `loadClass` are published in JDK-only; compatible's probe lambda warm-up
count was 4,107. This closes the earlier claim that the miss lambda remains interpreted for all
calls. The `Class.forName` door, define/package synchronization, and end-to-end gates remain OPEN.

The same probe was run three times per arm, pinned to CPU 3, with the default-on
`CRATONVM_DEFINE_ONE_PARSE` enabled and explicitly disabled as an A/B. All six CratonVM runs
preserved `classes=400`, read/resource/delegate counts = 400, and both checksums = 80200. Median
`define` was 84.5 us/class with the flag off and 93.3 us with it on in JDK-only; compatible medians
were 62.6 us off and 71.6 us on. This loaded-host A/B is too noisy to attribute the difference and
does not justify changing the established default. A `define-census` run with the flag on counted
5,600 definitions: `supertype-prepass` averaged 13.8 us (including 5.1 us prepass parse),
`class-manager-define` 17.5 us (1.6 us parse), `vm-post-define` 21.6 us, and `native-mirror` 17.3 us
per definition. The user-visible define/e2e gap remains open; the census identifies multiple costs
but no single safe change that closes it. Logs are `/tmp/t1-define-*-20261029.out` and
`/tmp/t1-define-census-20261029.out` on host 64.177.
