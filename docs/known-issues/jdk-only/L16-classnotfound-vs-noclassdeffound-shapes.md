# An absent array element type is thrown as `NoClassDefFoundError`, so `catch (ClassNotFoundException)` misses it

**Status:** ROOT-CAUSED, fix PARTIAL (the helper landed in
`classloading/src/class_manager.rs`; the two one-line guard patches that consume
it are in `native-builtins/`, which this lane does not own, and are recorded
verbatim below). Unverified — no binary was built in the session that wrote
this. Lane L16 of the jdk-wave2 pool.

## The failure

`regression-suite/src/RJdkFailure.java` fails in **both** `--real-jdk` and
`--jdk-only`, identically. HotSpot 25 passes it: 43 checks, exit 0.

```
Exception in thread "main" java/lang/NoClassDefFoundError: com/cratonvm/absent/NoSuchClass20260731
    at RJdkFailure.main(RJdkFailure.java:373)
    at RJdkFailure.missingClass(RJdkFailure.java:157)
Caused by: java/lang/ClassNotFoundException: com.cratonvm.absent.NoSuchClass20260731
```

### Correcting the lane brief on two points

1. **This is not the first check.** The brief said the vector "fails at the
   FIRST check, producing no `CK` lines". No `CK` line is printed, but that is
   because the `CK` for this method is at line 177, *after* all seven of its
   checks. Line 157 is the **fourth** probe. The first three —
   `Class.forName(absent)`, `Class.forName(absent, false, loader)`, and
   `loader.loadClass(absent)` — all pass, and the second of those also asserts
   the CNFE's message is exactly the dotted class name. So the plain
   `ClassNotFoundException` path is **already correct**; only the *array* form
   is broken.

2. **The brief's inventory does not match the file.** It lists "missing fields →
   `NoSuchFieldError`, bad casts → `ClassCastException`, abstract instantiation
   → `InstantiationError`, wrong types → `IncompatibleClassChangeError`, init
   failures → `ExceptionInInitializerError` then `NoClassDefFoundError` on
   re-use, verification failures → `VerifyError`, stack overflow, OOM".
   **None of those appear in `RJdkFailure.java`.** The `<clinit>`-latch question
   the brief asks about is not exercised by this vector at all, so this lane did
   not chase it. The real inventory is in the table at the bottom.

## The JDK rule

The two types are in different hierarchies — `ClassNotFoundException` is a
checked `Exception`, `NoClassDefFoundError` is an `Error` — so
`catch (ClassNotFoundException e)` **cannot** catch a `NoClassDefFoundError`.
Getting the shape wrong is not a message-quality problem; the throwable sails
straight through the caller's handler, which is exactly what happened here.

* `Class.forName(name)` / `ClassLoader.loadClass(name)` on an absent class →
  **`ClassNotFoundException`**.
* A *resolution* failure — a class named symbolically by executing bytecode is
  absent at link time → **`NoClassDefFoundError`**, usually with the CNFE as
  its cause.

The case this vector hits is the boundary between them, and JVMS §5.3.3 settles
it: an array class is created by the VM **from its element type**; no `.class`
file for the array itself is ever consulted. So an absent element type means
*the requested thing* is absent — a `Class.forName` miss, not a link-time
resolution failure of something that was found.

Measured against HotSpot 25 (`jdk-25.0.3.9-hotspot`, 2026-08-06):

```
Class.forName("com.cratonvm.absent.NoSuchClass20260731")
    -> ClassNotFoundException  msg="com.cratonvm.absent.NoSuchClass20260731"  cause=null
Class.forName("[Lcom.cratonvm.absent.NoSuchClass20260731;")
    -> ClassNotFoundException  msg="com.cratonvm.absent.NoSuchClass20260731"  cause=null
Class.forName("[[Lcom.cratonvm.absent.NoSuchClass20260731;")
    -> ClassNotFoundException  msg="com.cratonvm.absent.NoSuchClass20260731"  cause=null
```

Note the message: the **element** name, not the array descriptor, and no cause.

A second HotSpot measurement, which turns out to matter for choosing where to
fix this:

```
loader.loadClass("[Lcom.cratonvm.absent.NoSuchClass20260731;") -> CNFE msg="[Lcom.cratonvm.absent.NoSuchClass20260731;"
loader.loadClass("[Ljava.lang.String;")                        -> CNFE msg="[Ljava.lang.String;"
loader.loadClass("[I")                                         -> CNFE msg="[I"
```

**`ClassLoader.loadClass` never resolves an array descriptor at all** — not even
one whose element exists. Array classes are made by the VM, so `Class.forName`
handles the `[` form itself and only ever passes the *element* name to a loader.

## Which API the test uses, and what we threw

```java
// RJdkFailure.java:154-161
threw = false;
try {
    Class.forName("[L" + absent + ";");     // line 157
} catch (ClassNotFoundException expected) {
    threw = true;
}
check(threw, "an array of an absent class must also fail");
```

`Class.forName(String)` and `catch (ClassNotFoundException)`. So this is the
brief's first branch — we wrongly wrap into `NoClassDefFoundError` — but the
wrapping does **not** happen in `Class.forName`, and it happens only for the
array form.

The chain:

1. `Class.forName(String)` is caller-sensitive, so `native_class_for_name`
   (`native-builtins/src/lang_class.rs:1966`) resolves the caller's loader and
   invokes `loader.loadClass("[Lcom.cratonvm.absent.NoSuchClass20260731;")`.
   (Divergence #1 from HotSpot, benign on its own: we hand the loader an array
   descriptor, which HotSpot never does.)
2. That lands in `cl_real_load_class_base_rooted`
   (`native-builtins/src/classloader_real.rs:1115`), which calls
   `load_class_visible_to(ctx, this, "[Lcom/cratonvm/absent/NoSuchClass20260731;")`.
3. `ctx.load_class` bottoms out in
   `ClassManager::synthesize_array_class_for_loader`
   (`classloading/src/class_manager.rs:9013`), whose `b'L'` arm resolves the
   element and correctly propagates
   `ClassFileError::ClassNotFound { class_name: "com/cratonvm/absent/NoSuchClass20260731" }`.
4. **The bug.** `load_class_visible_to` classifies that error with a single
   heuristic (`classloader_real.rs:984`):

   ```rust
   )) if class_name != internal => {          // -> ClassLookup::DependencyMissing
   ```

   The intent is a genuinely useful 2026-07-17 fix: when `load_class` reports a
   *different* class missing than the one requested, the requested class file
   **was** found and only a supertype/interface is absent — JVMS §5.3/§5.4's
   `NoClassDefFoundError` case. But for an array descriptor the names *always*
   differ (`[Lp/X;` vs `p/X`) while nothing was found at all, so the heuristic
   misfires on every absent-element array.
5. `no_class_def_found_error` (`classloader_real.rs:1030`) then builds exactly
   the observed object: `NoClassDefFoundError` with the missing name in
   slash form, caused by `ClassNotFoundException` in dotted form. Both halves of
   the captured output match this constructor character-for-character, which is
   what pins the diagnosis to this call site rather than to its identical twin
   `raise_no_class_def_found_with_cause` (`vm/src/runtime/exceptions.rs:1854`) —
   that one is reached only from `convert_class_not_found`, which is wired to
   opcode boundaries (`opcodes.rs`, `constants.rs`, `dispatch_static.rs`) and
   not to `Class.forName`.

The same misfire exists a second time, in the synthetic-JDK-mode parallel
implementation of the same delegation: `resolve_global_if_visible`
(`native-builtins/src/classloader.rs:2667`) carries a byte-identical guard and
calls the same `no_class_def_found_error`. Since the end-state maps today's
`--real-jdk` onto `synthetic-jdk`, both must be fixed or the bug survives the
rename.

## Why the fix does not belong in `classloading`

The tempting one-liner is to make `synthesize_array_class_for_loader` report
`ClassNotFound { class_name: <the array name> }`, so `class_name == internal`
and the guard stops firing. **That is wrong**, and it would trade this bug for a
quieter one.

The array-synthesis error is also consumed by the bytecode path —
`anewarray` / `checkcast` / `ldc` of `[Lp/X;` reach `convert_class_not_found`.
There, HotSpot's answer is `NoClassDefFoundError` naming the **element**
(`p/X`), with a CNFE cause. Renaming the error to the array descriptor would
send `convert_class_not_found` down its `missing == class_name` branch and
produce `NoClassDefFoundError: [Lp/X;` with **no cause** — losing both the name
HotSpot reports and the cause chain.

So the element name in that error is load-bearing and correct. What is wrong is
one caller's *interpretation* of it. The fix goes at the `ClassLoader.loadClass`
boundary, which is the only place that needs the distinction.

## What changed

### Landed — `classloading/src/class_manager.rs` (this lane owns it)

A pure, IO-free helper plus its crate-root re-export in
`classloading/src/lib.rs`:

```rust
pub fn array_descriptor_element_class(descriptor: &str) -> Option<&str>
```

`[Lp/X;` → `Some("p/X")`; `[[[Lp/X;` → `Some("p/X")` (every dimension stripped,
because §5.3.3 recurses through the inner arrays to the same element class);
`[I` → `None` (a primitive element is not a class); a non-array name or a
malformed descriptor (`[Lp/X`, `[L;`) → `None`. Returning `None` for everything
that is not a reference-array descriptor is what makes it safe as a guard: every
non-array case keeps today's behaviour exactly.

Unit test: `l16_array_descriptor_element_class_strips_every_dimension`.

### Not landed — two guard patches this lane does not own

Both are the same edit, and both are recorded verbatim in the lane report. Each
narrows the "a *different* class is missing, so a dependency must be absent"
heuristic to exclude the one case where the names differ for a structural
reason: the missing class **is** the requested array's element type.

```rust
)) if class_name != internal
    && cratonvm_classloading::array_descriptor_element_class(internal)
        != Some(class_name.as_str()) =>
```

* `native-builtins/src/classloader_real.rs:984` — real-JDK mode.
* `native-builtins/src/classloader.rs:2667` — synthetic-JDK mode.

`cratonvm_classloading::` is already in scope in both files (each already calls
`cratonvm_classloading::is_bootstrap_appended_class`).

With the guard in place the arm falls through to `ClassLookup::NotFound` /
`Ok(None)`, `loadClass` reaches its step-3 `ClassNotFoundException` throw, and
`Class.forName` propagates it. The test's `catch (ClassNotFoundException)` then
fires.

Deliberately **not** widened to "any `ClassNotFound` under an array descriptor":
if `[Lp/X;`'s element `p/X` exists but `p/X`'s own superclass is missing, the
guard still lets `NoClassDefFoundError` name that superclass, which is what
`Class.forName` should report.

### Residual divergence, not fixed

After the patch our CNFE message is the **array descriptor**
(`[Lcom.cratonvm.absent.NoSuchClass20260731;`), matching HotSpot's
`ClassLoader.loadClass` shape; HotSpot's `Class.forName` reports the **element**
name. `RJdkFailure` does not assert the message on this check (it asserts the
message only on the first, non-array probe, line 136), so this does not affect
the vector. Closing it properly means teaching `native_class_for_name` to strip
`[`s and resolve the element itself — the HotSpot structure — rather than
handing array descriptors to `loadClass` at all. That is a `lang_class.rs`
change and a larger blast radius; recorded here, not attempted.

## The inventory

Every negative case `RJdkFailure.java` asserts, with what HotSpot throws and
what we do. Verdicts are from source reading only — no CratonVM binary was run.
The vector dies at case 4, so cases 5 onward have never actually executed.

### `missingClass()` — lines 123-178

| # | Line | Case | HotSpot | CratonVM | Verdict |
|---|------|------|---------|----------|---------|
| 1 | 130 | `Class.forName(absent)` | `ClassNotFoundException`, msg = dotted name | same | **correct** (proven — it passes at runtime) |
| 2 | 140 | `Class.forName(absent, false, loader)` | `ClassNotFoundException` | same | **correct** (proven) |
| 3 | 148 | `loader.loadClass(absent)` | `ClassNotFoundException` | same | **correct** (proven) |
| 4 | 157 | `Class.forName("[L"+absent+";")` | `ClassNotFoundException` | `NoClassDefFoundError` + CNFE cause | **WRONG — this lane's fix** |
| 5 | 166 | `Class.forName("java/lang/String")` (slash form) | `ClassNotFoundException` | `ClassNotFoundException` via `validate_for_name_dotted` rejecting `/` (`lang_class.rs:1918`), pinned by `for_name_rejection_is_class_not_found` | likely correct |
| 6 | 173 | `loader.getResource(absent) == null` | `null` | `null` (`classloader.rs:4793-4807`) | likely correct |
| 7 | 175 | `loader.getResourceAsStream(absent) == null` | `null` | `null`; no path fabricates an empty stream (`classloader.rs:5846/5863`) | likely correct |

### `linkageErrors()` — lines 180-239

Genuine linkage errors, manufactured by rewriting one same-length ASCII
constant-pool entry in javac's own output and defining the result as a hidden
class.

| # | Line | Case | HotSpot | CratonVM | Verdict |
|---|------|------|---------|----------|---------|
| 8 | 191 | patched call to absent static method | `NoSuchMethodError`, msg names `victimAbsentZerX` | `NoSuchMethodError`, msg `<dotted.Class>.<method><descriptor>` (`vm_exec.rs:23684`, formatted `exceptions.rs:1921`) — contains the name | likely correct |
| 9 | 208 | patched reference to absent class | `NoClassDefFoundError`, msg names `VictiX`, cause `null` or CNFE | `NoClassDefFoundError` via `convert_class_not_found` → `raise_no_class_def_found` (no cause, allowed by the assertion at :213); msg is **slash** form, and the assertion is `contains("VictiX")` so form does not matter | likely correct |
| 10 | 220 | the **unpatched** caller still returns `"absent0"` | works | hidden-class define + `Task` dispatch; no reason to differ | unknown (control case) |
| 11 | 226 | `getDeclaredMethod("noSuchMethodAtAll")` | `NoSuchMethodException` (checked) | `NoSuchMethodException` (`lang_class.rs:8991`) | likely correct |
| 12 | 233 | `getDeclaredField("noSuchFieldAtAll")` | `NoSuchFieldException` (checked) | `NoSuchFieldException` (`lang_class.rs:6378`) | likely correct |

### `missingNativeBinding()` — lines 241-271

| # | Line | Case | HotSpot | CratonVM | Verdict |
|---|------|------|---------|----------|---------|
| 13 | 248 | call `ACC_NATIVE` method with no binding | `UnsatisfiedLinkError`, msg names the method | `UnsatisfiedLinkError`, msg `<Class>.<method><desc>` (`vm_exec.rs:24204`) — contains `missingNative` | likely correct |
| 14 | 257 | `System.loadLibrary("cratonvm_absent_library_20260731")` | `UnsatisfiedLinkError` | **returns normally, throws nothing** — `let _ = ctx.load_native_library(...)` at `lang_system.rs:1492` discards the error, and the force-native-override list (`vm_exec.rs:20613`) keeps the real JDK's throwing path unreachable | **WRONG — latent, blocks this vector after the L16 fix** |
| 15 | 265 | second call to the same unbound native throws again | `UnsatisfiedLinkError` | no positive memo on the failure path; the registry's negative memo is generation-keyed and can only reproduce `None` | likely correct |

### `missingModule()` — lines 273-296

| # | Line | Case | HotSpot | CratonVM | Verdict |
|---|------|------|---------|----------|---------|
| 16 | 274 | `ModuleLayer.boot().findModule("cratonvm.no.such.module")` is **empty** | empty | **present** — `native_module_layer_find_module` (`jboss_jdkspecific.rs:436`) fabricates a `Module` for any syntactically valid name; there is no `Optional.empty()` path | **WRONG — latent** (independently found by lane L9) |
| 17 | 276 | `findModule("java.base")` is present | present | present (accidentally — same unconditional fabrication) | correct by accident |
| 18 | 282 | `Class.forName("javafx.application.Application")` | `ClassNotFoundException` | ordinary absent-class path, same as case 1 | likely correct |
| 19 | 289 | `ModuleFinder.of().find("anything")` is empty | empty | no native registered on `ModuleFinder.of`/`find`; delegates to real JDK | unknown |
| 20 | 291 | `ModuleFinder.ofSystem().find("java.base")` present | present | present (`reflect_annotations.rs:1886` fabricates unconditionally) | correct by accident |
| 21 | 293 | `ModuleFinder.ofSystem().find("cratonvm.absent")` is **empty** | empty | **present** — same unconditional fabrication; only a null name yields empty | **WRONG — latent** |

### `unsupportedPlatformService()` — lines 298-370

| # | Line | Case | HotSpot | CratonVM | Verdict |
|---|------|------|---------|----------|---------|
| 22 | 302 | `MessageDigest.getInstance("CRATONVM-NO-SUCH-DIGEST")` | `NoSuchAlgorithmException` | `NoSuchAlgorithmException` (`jca/message_digest.rs:165` → `provider_chain.rs:2187`) | likely correct |
| 23 | 309 | `Cipher.getInstance("CRATONVM-NO-SUCH-CIPHER")` | `NoSuchAlgorithmException` | **returns a synthetic `Cipher`** — `check_transformation_supported` (`jca/cipher.rs:674`) rejects only `mode == "CCM"`; the unknown name parses to cipher=`CRATONVM-NO-SUCH-CIPHER`, mode=`ECB` and is accepted | **WRONG — latent** |
| 24 | 316 | `KeyFactory.getInstance("CRATONVM-NO-SUCH-KEYFACTORY")` | `NoSuchAlgorithmException` | `NoSuchAlgorithmException` (`jca/key_factory.rs:1982`) | likely correct |
| 25 | 325 | `Charset.forName("CRATONVM-NO-SUCH-CHARSET")` | `UnsupportedCharsetException` | `UnsupportedCharsetException` (`lib.rs:33002` → `:33044`) | likely correct |
| 26 | 330 | `Charset.isSupported(bogus)` is false | false | false (`lib.rs:33130`, alias table misses) | likely correct |
| 27 | 332 | `Charset.isSupported("UTF-8")` is true | true | true | likely correct |
| 28 | 338 | `getFileAttributeView(tmp, BasicFileAttributeView.class)` non-null | non-null | non-null (`nio_file.rs:1281`) | likely correct |
| 29 | 341 | `supportedFileAttributeViews()` contains `"basic"` | yes | yes (`nio_file.rs:1022`) | likely correct |
| 30 | 345 | `readAttributes(tmp, "cratonvmnosuchview:size")` | `UnsupportedOperationException` | `UnsupportedOperationException` (`nio_file.rs:15529`) | likely correct |
| 31 | 356 | `TimeZone.getTimeZone("Cratonvm/Nowhere").getID()` is `"GMT"` — a **fallback**, and a VM that throws here is wrong | `"GMT"` | `"GMT"` (`lib.rs:19720`) | likely correct |
| 32 | 360 | `ZoneId.of("Cratonvm/Nowhere")` | `ZoneRulesException` | no native registered in real-JDK mode; real JDK parsing + tzdb decides | unknown |

Four distinct defects block this vector, only one of which is L16's: case 4
(fixed here), then cases 14, 16/21, and 23 — each in `native-builtins/`, each a
fabricated success where the spec mandates a failure, which is precisely the
class of defect `--jdk-only` exists to expose.

## Verifying

```
cd regression-suite

# oracle
"/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/java" -cp build RJdkFailure
# expected: PASS RJdkFailure (43 checks), rc=0

cratonvm.exe --real-jdk  -cp build RJdkFailure
cratonvm.exe --jdk-only  -cp build RJdkFailure
```

With only the L16 fix applied, the expected progress is: `missingClass` now
prints its `CK` line and `linkageErrors` runs, then the vector dies at line 261
on `System.loadLibrary` (case 14).

Rust-side:

```
cargo test -p cratonvm-classloading --lib l16_array_descriptor_element_class
```

## What would falsify the diagnosis

A run in which `Class.forName("[Lcom.cratonvm.absent.NoSuchClass20260731;")`
still produces `NoClassDefFoundError` after both guard patches are applied.
That would mean `Class.forName` is not reaching the loader's `loadClass` on this
path at all, and the `NoClassDefFoundError` is coming from
`raise_no_class_def_found_with_cause` (`vm/src/runtime/exceptions.rs:1854`) —
the identically-shaped twin — via some caller this lane did not find. The two
constructors produce byte-identical objects, so the captured output alone cannot
tell them apart; the diagnosis rests on `convert_class_not_found` being wired
only to opcode boundaries, which is what a verifier should re-check first.

The cheap discriminator, needing no rebuild: run `Class.forName("[I")` and
`Class.forName("[Ljava.lang.String;")`. Both must already succeed today. If
either fails, array resolution is broken far more generally than this report
claims and the element-vs-dependency analysis is beside the point.
