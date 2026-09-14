# An absent array element type is thrown as `NoClassDefFoundError`, so `catch (ClassNotFoundException)` misses it

**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **Headline: CLOSED, and now verified.** The old status line — *"fix PARTIAL …
  the two one-line guard patches that consume it are in `native-builtins/`,
  which this lane does not own"* — was **stale**. Both guard patches are in the
  tree, verbatim, since commit `f88feaef1`:
  `native-builtins/src/classloader_real.rs:990-992` and
  `native-builtins/src/classloader.rs:2939`. The helper they consume,
  `array_descriptor_element_class`, is at
  `classloading/src/class_manager.rs:20344` (commit `cb3134447`), re-exported at
  `classloading/src/lib.rs:84`, with its unit test at `class_manager.rs:18753`.
  The verification the record said it was missing was taken 2026-08-12 against
  the dev binary at `ba65f1a19`: `RJdkFailure` runs to `PASS RJdkFailure
  (43 checks)` in **both** `--jdk-only` and `--real-jdk`.
* **Residual: CLOSED — the four "WRONG — latent" inventory rows.** All four
  blockers this record filed as latent behind the L16 fix are fixed elsewhere:
  case 14 `System.loadLibrary(absent)` returning normally — commit `ba50b498b`,
  `native-builtins/src/lang_system.rs:2064` `load_library_or_throw`, raising
  `UnsatisfiedLinkError` at `:2096-2099`; case 16 `ModuleLayer.boot()
  .findModule(absent)` fabricating a Module — same commit,
  `native-builtins/src/jboss_jdkspecific.rs:673`; case 21
  `ModuleFinder.ofSystem().find(absent)` — commit `066856380`,
  `native-builtins/src/reflect_annotations.rs:2055-2062`; case 23
  `Cipher.getInstance("CRATONVM-NO-SUCH-CIPHER")` minting a synthetic `Cipher` —
  commit `eb41d9730`, `native-builtins/src/jca/cipher.rs:887` / `:953`, with the
  refusal asserted at `cipher.rs:4354`. Read the inventory tables below with
  that correction applied.
* **Residual: CLOSED IN SOURCE 2026-08-12, NOT BUILT AND NOT RUN, and it now
  has a scheduled witness.** The residual was real and was re-verified before
  it was touched: `cl_real_load_class_base_rooted`'s step-3 throw
  (`native-builtins/src/classloader_real.rs`, `alloc_single_message_exception(
  ctx, "java/lang/ClassNotFoundException", 1, &class_name)`) names the string
  `loadClass` was given, and `native_class_for_name` handed it the array
  descriptor and then propagated the result unchanged. `native_class_for_name`
  now corrects the *name* on its way out instead of restructuring the
  resolution — see "The residual, and how it was closed" below. The reason the
  vector stayed green is likewise unchanged and is the sharper half of this
  record: **`RJdkFailure` asserts the message only on the first, non-array
  probe.** A vector that asserts a throwable's type and not its message cannot
  see a message defect, which is why 43/43 never closed this. Exact Java for
  the five missing `RJdkFailure` assertions is in "The residual, and how it was
  closed"; the same contract is now also asserted in
  `regression-suite/src/RExceptions.java`, which is in `CORE_CLASSES` and so
  runs on a **default** invocation, not only under `--jdk-only`.

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

### ~~Not landed~~ — LANDED. The two guard patches this lane did not own

> **Reconciled 2026-08-12.** Both patches below are **in the tree, verbatim**,
> and have been since commit `f88feaef1` *wip(jdk-only): array-descriptor forName
> must throw CNFE, not NCDFE* — `native-builtins/src/classloader_real.rs:990-992`
> (real-JDK mode) and `native-builtins/src/classloader.rs:2939` (synthetic-JDK
> mode), each carrying the three-line predicate exactly as written below. The
> heading is kept so the text stays findable; read the section as a record of
> what landed, not as pending work.

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

### ~~Residual divergence, not fixed~~ — the residual, and how it was closed

> **Superseded 2026-08-12.** Kept because the paragraph below states the
> divergence exactly and the section that follows it states the fix. Read the
> two together.

After the guard patches our CNFE message is the **array descriptor**
(`[Lcom.cratonvm.absent.NoSuchClass20260731;`), matching HotSpot's
`ClassLoader.loadClass` shape; HotSpot's `Class.forName` reports the **element**
name. `RJdkFailure` does not assert the message on this check (it asserts the
message only on the first, non-array probe, line 136), so this does not affect
the vector.

## The residual, and how it was closed

### The option that was NOT taken, and why

The section above prescribed *"teaching `native_class_for_name` to strip `[`s
and resolve the element itself — the HotSpot structure — rather than handing
array descriptors to `loadClass` at all."* **That prescription is right about
HotSpot's structure and wrong about the cost/benefit here, and it was declined.**
Resolving the element separately means one extra `loader.loadClass` round trip
on the success path of every array `Class.forName` — a path that today works —
and then re-deriving the array `Class` from the element, which the loader-scoped
`synthesize_array_class_for_loader` already does correctly a layer down. It buys
nothing observable over correcting the name, and it puts a new
arbitrary-Java dispatch on a working path. Recorded as declined rather than
silently skipped, because the next reader will find the old prescription first.

### What landed instead — `native-builtins/src/lang_class.rs`

Two private helpers next to `validate_for_name_dotted`, and four call sites
inside `native_class_for_name`:

```rust
fn for_name_cnfe_name(dotted_name: &str) -> String
fn for_name_rename_array_cnfe(
    ctx: &mut dyn NativeContext,
    dotted_name: &str,
    failed: MethodCallFailed,
) -> MethodCallFailed
```

The first substitutes the element name when `dotted_name` is a reference-array
descriptor, and is used at the two sites that *mint* a
`RuntimeError::ClassNotFoundException` for the requested name — the
`loadClass`-returned-null arm, and the terminal "could not be located on any
classpath source" arm. The second re-mints a `ClassNotFoundException` that a
loader or the global resolution *raised*, and is used at the two propagate-as-is
arms. Both consume the existing `cratonvm_classloading::array_descriptor_element_class`,
which was already in the tree for the guard patches and answers `None` for
everything that is not a reference-array descriptor — so `[I`, plain class
names, `[Lp/X` and `[L;` all keep today's name character for character.

`for_name_rename_array_cnfe` is narrow on both axes, and each narrowing is the
same one the guard patch made:

* Only `java/lang/ClassNotFoundException` is rewritten, matched by exact class
  name. **A `NoClassDefFoundError` is left alone**, which is the case the guard
  patch deliberately did not widen past: if `[Lp/X;`'s element `p/X` exists but
  `p/X`'s own supertype is missing, `load_class_visible_to`'s
  `DependencyMissing` arm names that supertype and `Class.forName` should report
  it.
* Only a reference-array descriptor is rewritten.

The replacement is a fresh `RuntimeError::ClassNotFoundException`, so it carries
**no cause** — which is what HotSpot's array-form `Class.forName` miss reports
(`cause=null`, measured above).

One site was deliberately left alone: the explicit-null-loader refusal earlier
in the same function (`explicit_bootstrap_loader && !is_bootstrap_class_name`).
`Class.forName("[Ljava.lang.String;", false, null)` reaches it because
`is_bootstrap_class_name` tests a package prefix and `[Ljava/lang/String;` has
none, so that arm refuses an array form whose element **is** a bootstrap class.
That is a *wrong refusal*, not a wrong message; renaming its message would make
a wrong answer read more plausibly. Filed here as adjacent and not fixed.

### The coverage, which is the half that kept this open

`regression-suite/src/RExceptions.java` (`CORE_CLASSES`, so it runs on a default
invocation) gains five checks — the file moves **13 → 25** counting the seven
W7-37/W7-33 checks that landed in the same pass:
the array `Class.forName` throws `ClassNotFoundException` and not
`NoClassDefFoundError`; its message is the element; the two-dimensional form
strips every dimension; and two controls asserting `Class.forName("[I")` and
`Class.forName("[Ljava.lang.String;")` still resolve — the record's own cheap
discriminator, so a "fix" that broke array resolution generally does not survive.
The throwable is caught as `Throwable` and its type asserted, rather than caught
as `ClassNotFoundException`, so a regression on the *shape* reports the type it
got instead of dying uncaught.

`RJdkFailure.java` was held by another lane in the wave that closed this, so its
five assertions are recorded here as exact text rather than applied (43 → 48
checks). They replace the block at `missingClass()`'s *"Same for an absent array
element type"* comment:

```java
        // Same for an absent array element type and an absent nested class.
        threw = false;
        String arrayMessage = "";
        try {
            Class.forName("[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            arrayMessage = expected.getMessage();
        }
        check(threw, "an array of an absent class must also fail");
        // L16 - HotSpot never hands an array descriptor to a class loader:
        // JVMS 5.3.3 creates an array class from its ELEMENT type, so
        // Class.forName strips the '[' itself and the name that reaches a
        // loader - and therefore the CNFE message - is the element's.
        check(absent.equals(arrayMessage),
                "an array CNFE must name the element, not the descriptor: " + arrayMessage);

        threw = false;
        arrayMessage = "";
        try {
            Class.forName("[[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            arrayMessage = expected.getMessage();
        }
        check(threw, "a two-dimensional array of an absent class must also fail");
        check(absent.equals(arrayMessage),
                "every dimension is stripped before naming the element: " + arrayMessage);

        // The OTHER shape, and the reason the fix went into Class.forName and
        // not into the loader: ClassLoader.loadClass never resolves an array
        // form at all, so its CNFE names the descriptor it was given.
        threw = false;
        String loaderArrayMessage = "";
        try {
            loader.loadClass("[L" + absent + ";");
        } catch (ClassNotFoundException expected) {
            threw = true;
            loaderArrayMessage = expected.getMessage();
        }
        check(threw, "ClassLoader.loadClass of an array descriptor must throw");
        check(("[L" + absent + ";").equals(loaderArrayMessage),
                "loadClass names the descriptor it was asked for: " + loaderArrayMessage);
```

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
