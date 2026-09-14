# W5-1 — the `System.loadLibrary` allowlist was too wide

**Status (re-read 2026-08-12, second pass — source and committed baselines only;
nothing here was built or run):**

* **Item 2, the `Runtime` argument index: CLOSED, and now re-verified in the
  tree rather than taken from the record below.** Both arms of the fork in
  `native-builtins/src/lang_system.rs` call `runtime_load_args`: strict at
  `:1522` (`loadLibrary0`) and `:1540` (`load0`), `Compatible` at `:1596` and
  `:1615`. The helper is at `:1717` and reads `args.get(2)` for the name
  (`:1722`) and `args.get(1)` for the `fromClass` mirror (`:1718`). No
  `args.get(1)`-as-name read survives on either arm. There is nothing left to
  apply for this item and no reason to re-open it.
* **Item 1, the `BootLoader.loadLibrary` arming: STILL OPEN. Line cites
  corrected — every one in the older text below had rotted.** The registration
  is `native-builtins/src/lib.rs:14068-14073`, still
  `|_ctx, _args| Ok(None)`; `record_boot_loader_library` is
  `native-builtins/src/lang_system.rs:3244` and still has **zero** callers
  (grepped tree-wide: the only other hits are its own doc comment and this
  directory's records). Exact patch: §"The arming patch, as it must be applied".
* **The ambient `NativeKind` at that registration is no longer an inference from
  reading `set_category` windows — it is MEASURED, in a committed artefact.**
  `scripts/baselines/jdk-only-kind-map-25-linux.tsv:9400`:

  ```
  jdk/internal/loader/BootLoader	loadLibrary	(Ljava/lang/String;)V	0	bridge	0	1
  ```

  One row, so registered exactly **once** (the unit of that file is one
  registration, and a triple registered twice appears twice — `BootLoader
  .setBootLoaderUnnamedModule0` does, at `:9401-9402`). Kind `bridge`;
  `kind_stated` **0**, i.e. ambient, taken from the enclosing
  `set_category(Bridge)` at `lib.rs:7159` in
  `register_essential_natives_with_shims` (`:7103`), which the one temporary
  `Intrinsic` window for regex restores at `:7686-7699`. It must stay `Bridge`:
  `NativeKind::allowed_in` (`native-api/src/registry.rs:4624-4629`) drops
  `SyntheticStub` and only `SyntheticStub` under `JdkOnly`, so a drift of this
  registration into a `SyntheticStub` window deletes the no-op in strict mode,
  runs real `NativeLibraries` bytecode in its place, and restores the JDK
  native-library lock on the Linux boot-class `<clinit>` path that the
  short-circuit exists to avoid.
* **Do NOT "fix" the ambient kind by converting it to
  `register_with_kind(.., NativeKind::Bridge)`.** It is behaviour-identical and
  looks like free hardening, but it flips that row's `kind_stated` from 0 to 1,
  which is a diff in a baseline whose own header says the unit is one
  registration and whose README forbids hand-editing — it can only be re-frozen
  by `regression-suite/bridge-ratchet.sh` **on Linux**. The durable guard is a
  comment at the registration site, and that is written as an out-of-file patch
  below.
* **The measurement §2.6 asks for is WEAK ON WINDOWS, and a green Windows A/B
  must not be read as a licence to arm.** The only in-tree statement of who
  calls this native is `lib.rs:14062-14067`: *Linux* real-JDK boot classes such
  as `java.net.NetworkInterface` reach `BootLoader.loadLibrary("net")` during
  `<clinit>`, which is the road the short-circuit was added for. On Windows the
  plausible caller is `Inflater.<clinit>` → `ZipUtils.loadLibrary()` →
  `BootLoader.loadLibrary("zip")`, and a boot claim on `zip` changes nothing
  observable — `zip` already throws (`DYNAMIC_ALREADY_LOADED`,
  `lang_system.rs:2046`, screened out of `jdk_image_ships_library` at `:2078`).
  So the expected Windows outcome is *no difference*, which is the "correct but
  inert" answer W6-6 already had to disambiguate on its own road, not evidence
  that arming is safe. **Take the A/B on Linux, or state that it did not
  measure the road at risk.**
* **A second thing the A/B must decide, which nothing had written down.** The
  arming faithfully models the JDK rule, but its INPUT is CratonVM's own boot
  sequence, not HotSpot's. HotSpot answers `loadedLibrary=net` for `RJdkJni`
  precisely because nothing in `java.base` boot-loaded `net` before that line.
  If CratonVM's boot does claim `net`, arming does not remove a divergence — it
  manufactures one, and the defect to chase is then the boot sequence, not the
  bookkeeping. That is why the run is `with and without the arming` **on both
  arms with the HotSpot oracle beside them**, not just "does the suite stay
  green".
* **The vector now fails hard instead of merely diverging.**
  `regression-suite/src/RJdkJni.java`, `libraryLoading()`, gained one check
  (40 → **41**): `loaded` must be `"net"`, not merely `"zip" or "net"`. The old
  line accepted `zip`, which is exactly the answer this record's headline
  defect produced, so the headline was carried entirely by `run.sh`'s cross-VM
  `CK` diff — and that diff is **skipped for every class when no HotSpot is on
  the host**, which `run.sh` prints as a NOTE. It is also the trip-wire for the
  arming: a boot claim on `net` turns `loaded` into `"none"` and the fixture
  raises `AssertionError` instead of quietly printing a different `CK` line.
* **Item 3, the key is the spelling not the file: STILL OPEN, and the
  prescription below is INCOMPLETE.** See §"Item 3 re-costed" at the end.

**Status (re-reconciled 2026-08-12 — W7-79-loadlibrary-compatible-arm.md, on a
running binary):**

* **Residual 2, `Runtime.load0`/`loadLibrary0` on the `Compatible` arm: CLOSED.**
  The in-file patch below was applied on 2026-08-12. The diagnosis was verified
  before it was believed rather than after: on one binary at dev `87809196b`,
  `--jdk-only` answered `no cratonvm_probe_zzz in java.library.path` and loaded
  `net`, `--real-jdk` answered `no  in java.library.path` for both. Full table,
  the `Runtime.load` case, the blast radius and the vector are in
  W7-79-loadlibrary-compatible-arm.md. `LoaderScoping::Off` is unchanged on that
  arm, so nothing in the residual below moved with it.
* **The loader-scoped residual is no longer unverified.** It builds, it runs,
  and it fires. Two `URLClassLoader`s over one directory, each loading its own
  `LibProbe` which loads `sunmscapi`: HotSpot and `--jdk-only` both answer
  LOADED / LOADED / `Native Library … already loaded in another classloader`,
  `--real-jdk` answers LOADED three times. The only strict-vs-HotSpot difference
  is the key — bare `sunmscapi` here, `<java.home>\bin\sunmscapi.dll` there —
  which is exactly the named residual this record already carries. The strict
  run through `Runtime.loadLibrary` exercises `requesting_loader_id`'s
  `fromClass` path end to end.
* **Whether `Compatible` needs the loader scoping too: STILL OPEN, and now a
  decision rather than an omission.** Not taken on 2026-08-12, for four reasons
  that are about direction rather than caution. (a) It converts successes into
  errors, where the argument-index fix converted a manufactured error into a
  real answer. (b) Its trigger is two loaders in one VM — the servlet-container
  shape — so its blast radius lands squarely on the Tomcat and Spring Boot
  suites, which this lane cannot run; the argument-index fix's trigger is a
  `Runtime.load*` call that no suite makes at all. (c) The key is the spelling,
  not the file, so closing the first divergence in `Compatible` introduces a
  second one for `loadLibrary("zip")` versus `load("…/zip.dll")`; strict mode's
  contract accepts that trade and a frozen mode does not. (d) Nothing is waiting
  on it: `RJdkJni` is single-loader by construction and cannot assert it, since
  the two modes legitimately differ here.
* **Residual 1, `BootLoader.loadLibrary`: STILL OPEN, unchanged.**
  `native-builtins/src/lib.rs:14050` is still `|_ctx, _args| Ok(None)` and
  `record_boot_loader_library` (`lang_system.rs:3198`) still has no caller.
  Re-grepped 2026-08-12: `BootLoader.loadLibrary` is registered in exactly one
  place — `boot_loader.rs` registers three *other* `BootLoader` triples and
  `lib.rs:14249`/`:18696` two more, none of them `loadLibrary` — so there is no
  last-write-wins ambiguity, only an unarmed body. The A/B this record calls for
  still needs an armed build. The risk it names is live and now re-measured: the
  extended `RJdkJni` still prints `CK RJdkJni loadedLibrary=net` in both modes,
  byte-identical to HotSpot, so arming the recording would still be aimed
  directly at that line. Note the ambient `NativeKind` at that registration is
  whatever `lib.rs`'s last `set_category` left in force, not `Bridge` by
  default — check it when arming.
* **Residual 3, the key is the spelling not the file: STILL OPEN, unchanged**,
  and now visible in a transcript rather than only in source — see the
  cross-loader table above.

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** The allowlist narrowing is in
  `is_vm_provided_jdk_library` (`native-builtins/src/lang_system.rs:1950-1975`):
  `zip`, `sunec`, `jvm` are gone from every platform, `PLATFORM_ONLY` is
  `["sunmscapi"]` on Windows and `["jsig"]` elsewhere, and `zip` moved to
  `DYNAMIC_ALREADY_LOADED` (`:1987`). The binary verification this record said
  it lacked was taken 2026-08-12 against the dev binary at `ba65f1a19`:
  `RJdkJni` runs to `PASS RJdkJni (35 checks)` — 40 since the 2026-08-12
  vector extension — in **both** `--jdk-only` and
  `--real-jdk`, so the one-character `CK RJdkJni loadedLibrary=` divergence
  this record opened for is gone.
* **Residual: CLOSED — loader-scoped `loadedLibraryNames`.** Commit `adbe284ab`,
  `LOADED_LIBRARIES` at `native-builtins/src/lang_system.rs:1757`, `claim_library`
  at `:1777`, `requesting_loader_id` at `:1821`, teardown at `:3143`. **Strict
  mode only** is not a caveat that has since expired — it is the design: the
  mode fork is at `lang_system.rs:1473`, the strict arm registers the four
  triples with `LoaderScoping::On` and the `else` arm with `LoaderScoping::Off`,
  and `load_library_or_throw` early-returns on `Off` (`:1859`). Compatible mode
  is byte-identical to before by construction.
* **Residual: three, re-grepped 2026-08-12. Item 2 has since been CLOSED —
  see the 2026-08-12 re-reconciliation at the top of this file; items 1 and 3
  are still open and this text still describes them.**
  1. **`BootLoader.loadLibrary` is still unarmed.** `record_boot_loader_library`
     exists at `lang_system.rs:3183` and its own doc comment at `:3150` says
     *"THIS HAS NO CALLER IN THE TREE."* The registration it would feed is still
     `|_ctx, _args| Ok(None)` at `native-builtins/src/lib.rs:13813-13818`. The
     "Not applied here" closure body in this record is genuinely unapplied. This
     is also the reason W6-6's boot-loader case still cannot fire.
  2. **CLOSED 2026-08-12.** *(Was: "Compatible-mode `Runtime.load0`/
     `loadLibrary0` still read the wrong argument index." True when written;
     `runtime_load_args` is now called from both arms of the fork.)*
  3. **`load_native_library` still returns a table index, not the resolved
     path** (`native-builtins/src/lib.rs:13913`), so "the key is the spelling,
     not the file" stands.
* **Cannot adjudicate without a run:** whether arming (1) flips `RJdkJni`'s `net`
  probe. Source cannot decide whether `BootLoader.loadLibrary("net")` is reached
  before `RJdkJni.java:189-202`. The A/B is
  `target/release/cratonvm --jdk-only -cp regression-suite/build RJdkJni` and the
  same with `--real-jdk`, compared against `java -cp regression-suite/build
  RJdkJni` on the `CK RJdkJni loadedLibrary=` line, with and without the arming.
* **The "Out-of-file: the campaign README's row" patch is discharged** — the
  README in this directory was rebuilt on 2026-08-12 and no longer claims there
  is no loader-scoped bookkeeping.

Measured on 2026-08-07 on JDK 25.0.3 Windows x64, HotSpot as the oracle. The
JDK path in the original text was `C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot`;
on this host the same build now lives at
`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`, which is what the
2026-08-11 `javap` and `lib/src.zip` readings below used.


> **VERIFIED AGAINST A BINARY 2026-09-04.** The status block says *"nothing here
> was built or run"*, and the reconciliation says the binary verification was
> taken once, on 2026-08-12, against `ba65f1a19`. It has been taken again on a
> binary built from this tree, and widened from the two arms the record names to
> three.
>
> **The headline divergence is gone, and it is gone in Compatible mode too** —
> which this record never claimed. The observable is one character of `CK`
> output, and `run.sh` compares `CK` lines rather than exit codes:
>
> ```text
> HotSpot 25              CK RJdkJni loadedLibrary=net mapped=libfoo.so   PASS (41 checks)
> CratonVM --jdk-only     CK RJdkJni loadedLibrary=net mapped=libfoo.so   PASS (41 checks)
> CratonVM --real-jdk     CK RJdkJni loadedLibrary=net mapped=libfoo.so   PASS (41 checks)
> CratonVM compatible     CK RJdkJni loadedLibrary=net mapped=libfoo.so   PASS (41 checks)
> ```
>
> `RJdkJni.java` tries `System.loadLibrary("zip")` first and only falls through
> to the `net` probe if `zip` throws. Every arm now takes the fallback, so
> `is_vm_provided_jdk_library` no longer answers "success" for `zip`. The check
> count is 41 against this record's 35-then-40; that is the shared vector
> growing, not a result.
>
> **This answers half of the record's own open question.** It lists under
> *"Cannot adjudicate without a run"*: whether arming residual 1 flips
> `RJdkJni`'s `net` probe, since *"source cannot decide whether
> `BootLoader.loadLibrary("net")` is reached before `RJdkJni.java:189-202`."*
> The unarmed half is now measured and it is at parity. So arming is **not
> required** for this observable — and if arming made `net` count as
> already-loaded, it could only move a matching line to a non-matching one. The
> armed half was NOT run: nothing here implements the arming, so the A/B the
> record specifies is still only half done, and no claim is made about what
> arming would do.
>
> **Residuals 1 and 3 are still open, re-checked in this tree at today's
> lines** — the record's own line cites had rotted once already, so these are
> re-derived rather than copied:
>
> ```text
> 1  BootLoader.loadLibrary is still |_ctx, _args| Ok(None)   native-builtins/src/lib.rs:14766
>    record_boot_loader_library                               lang_system.rs:4334
>    its callers: still ZERO (the two other hits are its own doc comment)
> 3  load_native_library still returns a table index, not the resolved path
> ```
>
> Item 2 is confirmed CLOSED at today's lines: `runtime_load_args` reads
> `args.get(1)` for the `fromClass` mirror and `args.get(2)` for the name, and
> no `args.get(1)`-as-name read survives on either arm.
>
> **What this does NOT verify.** The 2026-08-07 HotSpot measurements and the
> `NativeLibraries` same-file-two-loaders rule in "The predicted cause was
> wrong" are oracle and JDK-source readings; they were not re-derived. This note
> measures ONE vector's `CK` line — it does not re-census the allowlist's
> contents, and a name that is wrongly allowed but that `RJdkJni` never asks for
> is invisible here.
## The divergence

One character of `CK` output, and `run.sh` compares `CK`/`PASS` lines, not exit
codes — `RJdkJni` exited 0 and still failed the suite:

```
HotSpot :  CK RJdkJni loadedLibrary=net mapped=foo.dll
CratonVM:  CK RJdkJni loadedLibrary=zip mapped=foo.dll
```

`RJdkJni.java:189-202` tries `System.loadLibrary("zip")` first and falls through
to a `net` probe only if `zip` throws `UnsatisfiedLinkError`. HotSpot takes the
fallback. CratonVM did not, because W2-7's `is_vm_provided_jdk_library`
allowlist answered "success" for `zip`.

## The predicted cause was wrong; the prediction was right

Wave 2 predicted the failure correctly but attributed it to static linking —
"`zip` is folded into `libjava` on this image". It is not. `zip.dll` is a real,
separate file in `<java.home>/bin`, and a *cold* `System.loadLibrary("zip")`
loads it fine. The actual rule is dynamic and has nothing to do with linkage:

`jdk.internal.loader.NativeLibraries` refuses to load the same library FILE into
two different class loaders. `java.base` boot-loads `zip.dll` itself —
`Inflater.<clinit>` -> `ZipUtils.loadLibrary()` ->
`BootLoader.loadLibrary("zip")` — so once anything has touched `java.util.zip`,
an app-class-loader `System.loadLibrary("zip")` throws
`UnsatisfiedLinkError: Native Library …\zip.dll already loaded in another classloader`.

Measured, same JVM image, three states:

| state | `loadLibrary("zip")` |
|---|---|
| cold, classpath is a directory | LOADS |
| after any `java.util.zip` native use | THROWS (already loaded in another classloader) |
| cold, but classpath is a `.jar` | THROWS (already loaded in another classloader) |

The rule is per-library-file and symmetric — pre-touching `java.net` makes
`net` *and* `nio` throw; pre-touching `java.util.prefs` makes `prefs` throw.

`RJdkJni` puts itself in the second state deliberately: `main` calls
`zipNatives()` immediately before `libraryLoading()`. That is why the oracle
says `net`, and it is a *test-produced* state rather than a file-layout fact,
so it holds identically on Linux.

## The measurement

`System.loadLibrary(x)` from the app class loader, nothing pre-loaded,
JDK 25.0.3 Windows x64:

```
java             LOADS
zip              LOADS
net              LOADS
nio              LOADS
jimage           LOADS
verify           LOADS
management       LOADS
management_ext   LOADS
instrument       LOADS
extnet           LOADS
prefs            LOADS
j2pkcs11         LOADS
sunec            THROWS UnsatisfiedLinkError: no sunec in java.library.path
sunmscapi        LOADS
jsig             THROWS UnsatisfiedLinkError: no jsig in java.library.path
jvm              THROWS UnsatisfiedLinkError: no jvm in java.library.path
```

Corroborated by the image contents: `<java.home>/bin` has no `sunec.dll`, no
`jsig.dll`, no `jvm.dll` (the last lives in `bin/server`, which is on neither
`java.library.path` nor `sun.boot.library.path`).

## Why the allowlist is the lever at all

Because the real load always fails first. `LoadLibraryW` on the JDK's own DLLs
from a non-JVM process returns `ERROR_MOD_NOT_FOUND` (126) — they are linked
against `jvm.dll`, which CratonVM's process does not have:

```
java, zip, net, nio, jimage, verify, management, management_ext,
instrument, extnet, prefs        LoadLibrary FAILED err=126
j2pkcs11, sunmscapi              LoadLibrary OK   (self-contained)
```

So `load_library_or_throw` reaches the allowlist for every interesting name,
even though `<java.home>\bin` is on this host's `PATH` and therefore on
CratonVM's `java.library.path`.

## Before / after

Removed: `zip`, `sunec`, `jvm` (all platforms) and `jsig` (Windows only).
Kept, now split by platform: `sunmscapi` on Windows, `jsig` elsewhere.

## The residual, closed — 2026-08-11

### It was still live

Checked before implementing, because this campaign has fifteen records claiming
a hand-off was never applied for a change that is in the tree today. This one
was not one of them. On dev `95b693f2d`, `loadedLibraryNames` appeared in
**five** places in the source tree and every one of them was a comment saying
the bookkeeping does not exist: this record, `W6-6`, the campaign README, and
two comments in `native-builtins/src/lib.rs` explaining that the JDK's own
bytecode holds the set on the *other* road. No table, no loader key, no call
site. Live.

### What the JDK actually specifies

`jdk.internal.loader.NativeLibraries`, JDK 25.0.3 `lib/src.zip`. The class doc
on `newInstance(ClassLoader)` states the restriction as a numbered guarantee of
the type:

> 3. Restriction on a native library that can only be loaded by one class
>    loader. Each class loader manages its own set of native libraries. The
>    same JNI native library cannot be loaded into more than one class loader.

and `loadLibrary(Class,String,boolean)` enforces it in two steps, in this order,
inside `acquireNativeLibraryLock(name)`:

```java
// find if this library has already been loaded and registered in this NativeLibraries
NativeLibrary cached = libraries.get(name);
if (cached != null) {
    return cached;
}

// cannot be loaded by other class loaders
if (loadedLibraryNames.contains(name)) {
    throw new UnsatisfiedLinkError("Native Library " + name +
            " already loaded in another classloader");
}
```

The two structures are **not the same shape**, and that is the whole point:

| structure | scope | on a hit |
|---|---|---|
| `libraries` — `Map<String,NativeLibraryImpl>` | **instance** field of the `NativeLibraries` that `ClassLoader` holds one of per loader (`private final NativeLibraries libraries = NativeLibraries.newInstance(this)`) | returns the SAME library — silent success |
| `loadedLibraryNames` — `Set<String>` | `static` | `UnsatisfiedLinkError` |

`ClassLoader.nativeLibrariesFor(loader)` is the accessor, with
`BootLoader.getNativeLibraries()` standing in for the null loader; and
`loadLibrary(Class,String,boolean)` opens with
`if (this.loader != loader) throw new InternalError(...)`, so an instance is
bound to one loader by assertion. A single process-wide set of names cannot
answer either question: it keys on a string and carries no loader identity, so
it cannot separate "this loader already has it" (success) from "somebody else
does" (error).

The key itself is the library **file**, not the bare name:
`NativeLibraries.loadLibrary(Class,File)` sets `name = file.getCanonicalPath()`
for anything not statically linked into libjvm, and every bare-name road ends
in that overload. That is why the dynamic rule in this record's own tables is
per-library-file and symmetric.

### How it is scoped

`native-builtins/src/lang_system.rs`:

* `LOADED_LIBRARIES`, a `VmScoped<BTreeMap<i32, BTreeSet<String>>>` — loader id
  to library keys, one row per VM. **Both** JDK queries come off one table:
  "does this loader hold it" is a row lookup, "does any loader hold it" is the
  union over rows. `claim_library` does the lookup and the insert in a single
  table acquisition, which is the window HotSpot closes with
  `acquireNativeLibraryLock(name)`.
* **Was it a process global before? There was nothing before** — no state at
  all. What is added is not one either: `VmScoped` is the tree's own mechanism
  for a native side-table partitioned by `vm_identity`, already used two
  hundred lines further down this same file for `SYSTEM_ENV`/`SYSTEM_PROPS`,
  and contract §2's ban is what it exists to satisfy. Torn down from
  `forget_vm_system_singletons`, which `release_vm_native_state` already calls.
  (The pre-existing `SHUTDOWN_HOOKS` in this file *is* an unpartitioned
  `static Mutex<Vec<usize>>`. Not touched, not this lane's, named here only so
  the next reader does not take it as precedent.)
* Keyed on `NativeContext::loader_id_of_class` — `0` bootstrap, `1` platform,
  `2` application, `3+` user-defined — **never on the loader's name**. Two
  loaders can share a name; that is the species.
* The loader is resolved by `requesting_loader_id`, from the JDK's own
  `fromClass` argument where one exists (`Runtime.load0`/`loadLibrary0`, which
  real `Runtime` bytecode fills from `Reflection.getCallerClass()` and which
  `ClassLoader.loadLibrary` itself turns into a loader with
  `fromClass.getClassLoader()`), and otherwise from `frame_class_ids()` —
  each live frame's already-resolved `ClassId`, not a name re-resolution of a
  captured stack trace, which would collapse two same-named classes from
  different loaders onto whichever the class table saw first.
* No `ObjectRef` is stored, so no GC root scan and no post-collection remap are
  needed. Holding the loader *mirror* would have needed both, and would have
  keyed loader identity on an address that moves.

### Where it does not know, and does not guess

`requesting_loader_id` answers `None` when neither source names a class — a load
with no Java frame under it. `loaded_by` then records nothing and throws
nothing. Recording under a stand-in loader id would make the *next* load, the
one from the real owner, throw: the error would be manufactured, just later and
further from the cause. Throwing outright manufactures it immediately. Of the
two failure directions the missed error is the one that leaves a caller's own
`catch (UnsatisfiedLinkError)` fallback reachable.

### Mode

**Strict (`--jdk-only`) only.** `Compatible` is unchanged: the four triples are
registered once per mode and the `Compatible` arm keeps today's bodies with
`LoaderScoping::Off`, under which every success arm of `load_library_or_throw`
returns the `Ok(None)` it returns today. The gate is at registration because a
`NativeCallback` is a bare `fn` pointer with no captures and `NativeContext`
exposes no policy accessor — `CompatibilityMode` is a per-registry field on
purpose. `vm_init.rs` sets the mode before the `register_*` population pass and
says so structurally, so the gate is not vacuous. Each triple is registered
exactly once, not re-registered on top of itself, so no self-shadow row appears
in the duplicate-registration census.

### What still cannot fire, and the patch that would arm it

The record can only hold what something reports to it, and
`jdk/internal/loader/BootLoader.loadLibrary` is a deliberate no-op in
`native-builtins/src/lib.rs` — the short-circuit that stops real
`NativeLibraries` bytecode from blocking on the JDK's native-library lock during
Linux boot-class `<clinit>`. So the case this record opened with — `java.base`
boot-loads `net`, then the application asks for it — still reports success.

`lang_system::record_boot_loader_library` is written and **deliberately
unarmed**; its doc comment says so. Out-of-file patch, the closure body of that
registration (`BootLoader.loadLibrary(String)` is static, so `args[0]` is the
name).

**The sketch below is SUPERSEDED by §"The arming patch, as it must be applied"
at the end of this file** — same body, but with the ambient-`NativeKind` guard
comment the patch has to land with, and against line numbers that have not
rotted:

```rust
    registry.register(
        "jdk/internal/loader/BootLoader",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(o))) = args.first() {
                let name = ctx.read_string(*o).unwrap_or_default();
                crate::lang_system::record_boot_loader_library(ctx, &name);
            }
            Ok(None)
        },
    );
```

Not applied here, and the reason is a measurement rather than caution: arming it
makes `System.loadLibrary("net")` throw for any program that has already reached
a `java.net` boot class. That is HotSpot's answer, and it is exactly what
`RJdkJni.libraryLoading` depends on NOT happening — the `zip` probe falls
through to a `net` probe that must succeed (`RJdkJni.java:189-202`) and `run.sh`
compares `CK` lines, so a spurious throw there fails the vector. Whether
CratonVM reaches `BootLoader.loadLibrary("net")` before that line is not
decidable from source. One A/B on a built binary, both modes, HotSpot oracle
beside it, settles it.

### Out-of-file: the campaign README's row for these two records

`docs/known-issues/jdk-only/README.md` §2.1 is not this lane's file. Its row for
`W5-1` / `W6-6` still reads "there is **no class-loader-scoped
`loadedLibraryNames` bookkeeping in this VM**, so the JDK's dynamic 'already
loaded elsewhere' rule cannot fire. A static allowlist cannot model it."
Replacement:

> Loader-scoped under `--jdk-only` since 2026-08-11 (`LOADED_LIBRARIES` in
> `lang_system.rs`); `Compatible` unchanged. What is left is the boot-loader
> case: `BootLoader.loadLibrary` is a no-op, so `java.base` taking `net`/`nio`/
> `prefs` for itself is never recorded. The patch that arms it is in `W5-1`,
> unapplied because it can flip `RJdkJni`'s `net` probe and that needs one A/B
> on a binary.

### Named residual: the key is the spelling, not the file

HotSpot canonicalises to a path, so `System.loadLibrary("zip")` and
`System.load("<java.home>/bin/zip.dll")` are one key there and two here. Not
rounded up: `NativeContext::load_native_library` returns a library-table index,
not the path it resolved, so a bare name found on `java.library.path` cannot be
canonicalised back to a file without redoing the search — and a key invented by
redoing it would not be the file that was actually opened. Closing this means
returning the resolved path from `load_native_library`, which is a `native-api`
change.

## Separately found: `Runtime.load0`/`loadLibrary0` read the wrong argument

Not a strict-mode defect and not part of the residual — found while reading for
the loader identity, and it is the same two lines.

`javap -p java.lang.Runtime`, JDK 25.0.3:

```
  public void load(java.lang.String);
  void load0(java.lang.Class<?>, java.lang.String);
  public void loadLibrary(java.lang.String);
  void loadLibrary0(java.lang.Class<?>, java.lang.String);
```

Both `load0` and `loadLibrary0` are **instance** methods, so a native body sees
`args[0]` = the `Runtime` receiver, `args[1]` = the `fromClass` mirror, `args[2]`
= the name. The convention is stated repeatedly for this same registry — the
`Runtime.addShutdownHook` registration forty lines above these two says
"args[0] = the `Runtime` receiver, args[1] = the hook `Thread`", and
`vm_exec.rs`'s native argument marshalling describes its inline buffer as
"receiver plus a couple of operands".

Both bodies read `args.get(1)` as the name. That is the `Class` mirror, and
`read_string` of a non-`String` is `None`
(`vm_object.rs::read_string_non_string_object`), so the name arrives empty and
`Runtime.getRuntime().loadLibrary(x)` fails for every `x` with
`no  in java.library.path` — note the double space, which is the whole
signature of the bug. A wrong answer with the *right shape*, which is why it
survived: the regression suite reaches library loading only through
`System.load`/`System.loadLibrary` (`RJdkJni.java:189-217`,
`RJdkFailure.java:257`), never through `Runtime`, so nothing guards it.

Corrected on the strict arm (`runtime_load_args`) 2026-08-11, and on the
`Compatible` arm 2026-08-12: it is mode-independent — nothing about it is a
compatibility-layer substitution — and a `loadLibrary` that fails for every
argument is the one thing the `Compatible` freeze admits, a HotSpot-parity bug
fix. Adjudicated in W7-79-loadlibrary-compatible-arm.md, which carries the
measurement, the blast radius, and the five `RJdkJni` checks that assert the
name now reaches the native.

The in-file patch, as applied — in the `else` arm only, and keeping
`LoaderScoping::Off` so the loader rule stays strict-only, replace each of the
two `Runtime` bodies' first three lines with the one call the strict arm makes:

```rust
            |ctx, args| {
                let (_from_class, name) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,     // `AbsolutePath` on `load0`
                    None,
                    LoaderScoping::Off,
                )
            },
```

Callers that were silently getting an `UnsatisfiedLinkError` for a library that
is present will start loading it.

## Blast radius

### Of the allowlist narrowing (2026-08-07)

`java.base` does not reach this code for its own bootstrap: `java.util.zip`
loads via `BootLoader.loadLibrary` -> `jdk/internal/loader/NativeLibraries.load`
(registered separately in `native-builtins/src/lib.rs`), not via
`System.loadLibrary`. Only user-level
`System.loadLibrary`/`System.load`/`Runtime.load0`/`Runtime.loadLibrary0` are
affected. The documented `catch (UnsatisfiedLinkError)` fallbacks in Netty and
Tomcat/tcnative do not name any of the four removed libraries.

*Corrected 2026-08-11:* this paragraph used to say `NativeLibraries.load`
"unconditionally returns success". It did when this was written; `W6-6` fixed
it the same day and the parenthetical outlived the defect.

### Of the loader scoping (2026-08-11)

Strict mode only, and only where a library is loaded through
`System.load`/`System.loadLibrary`/`Runtime.load*` from **two different class
loaders in one VM**. That is not a hypothetical shape: it is what a servlet
container does when two web applications, each with its own loader, probe one
`tcnative`, and it is the case HotSpot throws on. So it can fire without any
boot-loader bookkeeping, and every caller it fires at is one written around a
`catch (UnsatisfiedLinkError)` fallback — which is the point, and is also a
behaviour change for each of them.

`RJdkJni.libraryLoading` is the corpus vector in range and should be unmoved:
its two `System.loadLibrary("zip")` calls both still throw for the reason this
record already documents (`zip` is on `DYNAMIC_ALREADY_LOADED` and off the
allowlist), so nothing is recorded for them, and the `net` probe that follows is
a single load from a single loader with an empty table under it.

*Verified 2026-08-12* against the dev binary at `87809196b`: `RJdkJni` prints
`CK RJdkJni loadedLibrary=net mapped=foo.dll` in both modes, byte-identical to
HotSpot, and reaches `PASS RJdkJni (40 checks)` on the strict arm after the
vector extension in W7-79-loadlibrary-compatible-arm.md. Unmoved, as predicted.
The scoping itself was verified separately, with two loaders — see the top of
this file. **41 checks since 2026-08-12's second pass** — the `loaded` value is
now asserted, not merely the `CK` line; see the top of this file.

## The arming patch, as it must be applied

Two independent out-of-file patches, both in `native-builtins/src/lib.rs`, which
is not this lane's file. **A is landable now and changes no behaviour; B is the
arming and must not land before the measurement.** A does not depend on B.

### Patch A — the ambient-kind guard, at the registration site

The constraint has lived only in records (W6-6's 2026-08-12 amendment, and the
top of this file). Anyone retagging the `set_category` window that spans
`lib.rs:14068` reads `lib.rs`, not this directory. Insert immediately **above**
`registry.register(` at `lib.rs:14068`, after the existing `KEEP (the
BootLoader.loadLibrary no-op)` comment block that ends at `:14067`:

```rust
    // AMBIENT `NativeKind`, AND IT MUST STAY `Bridge`. This is a bare
    // `register`, so the kind comes from the enclosing `set_category(Bridge)`
    // at the top of this function, restored after the regex `Intrinsic` window.
    // `NativeKind::allowed_in` drops `SyntheticStub` and ONLY `SyntheticStub`
    // under `JdkOnly`: if this registration ever drifts inside a
    // `SyntheticStub` window it is DROPPED in strict mode, real
    // `NativeLibraries` bytecode runs in its place, and the JDK native-library
    // lock this short-circuit exists to avoid is back on the Linux boot-class
    // `<clinit>` path. Re-check the enclosing category, not just this call.
    // Measured, one row: `scripts/baselines/jdk-only-kind-map-25-linux.tsv`
    // has `... loadLibrary (Ljava/lang/String;)V 0 bridge 0 1` — `bridge`,
    // `kind_stated=0`. Do NOT "harden" this by switching to
    // `register_with_kind(.., Bridge)`: identical behaviour, but it flips that
    // row's `kind_stated` 0 -> 1, and that baseline can only be re-frozen by
    // `regression-suite/bridge-ratchet.sh` on Linux.
```

### Patch B — the arming itself

Replace `lib.rs:14068-14073` in full:

```rust
    registry.register(
        "jdk/internal/loader/BootLoader",
        "loadLibrary",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            // The LOAD stays a no-op — that is what avoids the JDK's
            // native-library lock. Only the bookkeeping is added:
            // `BootLoader.loadLibrary(String)` is static, so `args[0]` is the
            // name, and `record_boot_loader_library` claims it for loader id 0.
            if let Some(Value::Object(Some(name_obj))) = args.first() {
                let name = ctx.read_string(*name_obj).unwrap_or_default();
                crate::lang_system::record_boot_loader_library(ctx, &name);
            }
            Ok(None)
        },
    );
```

Nothing else moves: `record_boot_loader_library` is already `pub`
(`lang_system.rs:3244`), already ignores an empty name, and is already inert
under `Compatible` because `loaded_by` early-returns on `LoaderScoping::Off`
(`lang_system.rs:1918`) — so the whole blast radius is the strict arm. Delete
the "THIS HAS NO CALLER IN THE TREE" paragraph from that function's doc comment
in the same change, or the next reader is entitled to believe it.

### The measurement, and what a bad outcome looks like

```
cratonvm --java-home "<jdk-25>" --jdk-only -cp regression-suite/build RJdkJni
cratonvm --java-home "<jdk-25>" --real-jdk -cp regression-suite/build RJdkJni
java -cp regression-suite/build RJdkJni          # HotSpot 25 oracle
```

taken **with and without patch B**, on **Linux**, diffing the
`CK RJdkJni loadedLibrary=` line and the `PASS RJdkJni (41 checks)` line.
`--java-home` is not optional; a hand-run without it has already inverted a
per-mode verdict in this campaign.

* **Bad outcome, and the one to expect if the road is live:**
  `AssertionError: System.loadLibrary must FAIL for zip once java.util.zip has
  boot-loaded it and fall through to net, got: none`, i.e. CratonVM's own boot
  claimed `net` for the boot loader where HotSpot's did not. Do not land B, and
  do not "fix" it by re-adding `net` somewhere — the finding is then a boot
  sequence that touches `java.net` when HotSpot's does not, which is a
  different record.
* **Inert outcome:** byte-identical `CK` lines and 41 checks with and without B,
  on Linux. That licenses B only in the sense that it costs nothing; it does not
  demonstrate the residual is closed, because the claim is that a boot load is
  now *recorded*, and no Java-visible surface reports the table. Say so rather
  than writing "verified".
* **On Windows either outcome is uninformative** — see the top of this file.

## Item 3 re-costed: the prescription is incomplete, and cheaper than it looks

*"Closing this means returning the resolved path from `load_native_library`,
which is a `native-api` change"* is right about the direction and wrong twice
about the size.

* **It is not one signature.** `load_native_library` is a required
  `NativeContext` trait method (`native-api/src/registry.rs:4320`, returning
  `Result<i64, MethodCallFailed>`) with **nine** implementations: the real one
  at `vm/src/vm/vm_exec.rs:15758` and eight mocks/test doubles
  (`native-io/src/test_support.rs:983`,
  `native-collections/tests/common/mod.rs:1089`,
  `native-collections/src/lib.rs:60473`,
  `native-builtins/src/test_utils.rs:2752`, `native-builtins/src/cds.rs:1932`,
  `native-builtins/src/atomic_updater.rs:1792`,
  `native-api/tests/atomic_fetch_add_err_path.rs:481`,
  `native-api/src/test_mock.rs:827`). Widening the return type churns all nine.
  An **additive** accessor with a default body — "resolve this spelling to the
  file it names, or `None`" — is two sites: the trait, and `vm_exec.rs`, where
  `resolve_library_path` (`:15761`) already computes exactly that value and
  currently throws it away.
* **And returning the path from the successful open would still not close it.**
  `load_library_or_throw` (`lang_system.rs:2123`) has three success arms, and
  the open succeeds on only one of them (`:2134`). The other two —
  `is_vm_provided_jdk_library` (`:2141`) and `jdk_image_ships_library` (`:2149`)
  — report success with **nothing opened**, and those are precisely the
  libraries whose spelling HotSpot canonicalises to `<java.home>/bin/zip.dll`.
  For those two arms the path needs no `native-api` change at all:
  `jdk_image_ships_library` (`:2077-2092`) already builds
  `<java.home>/{bin|lib}/platform_lib_name(name)` and tests it with `is_file()`.

  So the shape is: canonicalise the key in `lang_system.rs` for the two
  no-open arms, and use one additive resolver for the real-open arm.
* **It stays a DECISION, not a prescription.** Changing the key changes which
  loads collide under `LoaderScoping::On`, so it is a strict-mode behaviour
  change with no vector demanding it — `RJdkJni` is single-loader by
  construction and cannot assert it, for the same reason the cross-loader table
  is not in it.
