# L5 (reflection and class metadata) is CLEAN — 483 rows, 20 defects, 0 residuals

> **Superseded in two places by
> `L5-residuals-module-packages-and-invokeexact-20260828.md`:** the two items §5
> leaves open are now closed, and the `forName` fix in §4 shipped a regression
> two regression-suite vectors caught (see the CORRECTION in §4).

**Status: COMPLETE 2026-08-28.** Lane L5 of the seven in
`HANDOFF-20260828-SCOPE.md`. Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`.

## 1. The result

| probe | rows | compat | `--jdk-only` |
| --- | ---: | --- | --- |
| `ClassShadowSweep` | 261 | 0 diffs | 0 diffs |
| `FieldMethodShadowSweep` | 132 | 0 diffs | 0 diffs |
| `ClassLoaderShadowSweep` | 90 | 0 diffs | 0 diffs |
| **total** | **483** | **clean** | **clean** |

Covering all 207 `native-won` triples in `Class` (67), `reflect/Field` (32),
`ClassLoader` (29), `System$1` (29), `reflect/Method` (26) and `Module` (24).

**20 defects fixed.** Every one on a contract edge, which is now five families
out of five in this campaign.

## 2. What was wrong

### `java.lang.Class` and `Module` — 9

* `getPackageName()` answered `""` for all six primitives and `void`, where the
  JDK answers `"java.lang"`. Six rows, one bug. The check had to go BEFORE the
  memo: `PACKAGE_NAME_CACHE` is keyed on `ClassId` and a primitive mirror has
  one, so a wrong answer would have been cached for the life of the VM.
* `Class.getResourceAsStream(null)` answered null instead of throwing.
* `Module.canRead(null)` answered `false` instead of throwing — the worse shape,
  because a caller testing `if (!m.canRead(other))` takes the failure branch for
  the wrong reason.
* `Module.canUse` over-approximated (§3).

### `java.lang.reflect.Field` / `Method` — 1

**132 rows, one defect.** This surface is solid: every widening/narrowing rule
holds in both directions (`getInt` on a `byte` widens, `getByte` on an `int`
refuses, `setInt` into a `byte` field refuses), every `invoke` edge holds
including `InvocationTargetException` wrapping and argument widening, and all
the metadata holds.

The one: `Field.getAnnotation(null)` answered null instead of throwing NPE — and
the direction matters more than usual, because null is ALSO this method's
ordinary answer for an absent annotation. A caller passing a class it failed to
resolve got "no such annotation" and took the same branch as a correct negative.

**A clean family is a result.** It says L5's remaining work was in the loading
path, not the accessor path, and that is what the next 90 rows confirmed.

### `java.lang.ClassLoader` and `System$1` — 10

* **An array's loader was not its component's.**
  `MyClass[].class.getClassLoader()` answered null instead of the app loader. An
  application array type reported as bootstrap-loaded looks like a JDK type, and
  a loader-keyed cache, an `isAssignableFrom` against a loader-scoped class, or
  a serializer choosing a resolver all key it wrong. Now recurses to the
  component — which is also why `String[]` correctly stays null.
* **`loadClass` accepted two spellings the JDK refuses**: the internal
  slash form and an array descriptor. §4 is about where that fix belongs.
* **`getResource("/name")` found the resource.** A `ClassLoader` resource name
  is always absolute and must NOT begin with `/` — the exact opposite of
  `Class.getResource`. `trim_start_matches('/')` made the two spellings
  equivalent, which is more permissive than the JDK in the direction that HIDES
  a bug: code passing a `Class.getResource`-shaped name works here and returns
  null everywhere else.
* **`getSystemResource(null)` did not throw.** The existing null guard was
  `args.len() >= 2`, and the STATIC forms take the name at index 0 with no
  receiver, so it never saw them.
* `getDefinedPackage(null)` did not throw.
* **`getDefinedPackages()` returned `Object[]`, not `Package[]`.**
  `alloc_package_array` falls back to an untyped array when
  `class_id_by_name("java/lang/Package")` misses — and it misses merely because
  the class is not LOADED yet. The function directly above it carries a long
  comment explaining why the array must be typed. That fix was present, reached,
  and inert, for exactly the `$RustJvmImpl` reason fixed earlier the same day:
  **a lookup-only helper standing where a load belongs.**
* **`Module.addExports`'s two nulls have DIFFERENT types** and it refused
  neither: a null package is `IllegalArgumentException` (the JDK validates the
  name), a null target module is `NullPointerException` (it dereferences it).
  Returning `this` for both made the call a silent no-op — which on an unnamed
  module is ALSO the correct behaviour for a well-formed call, so the bug was
  invisible precisely because the success path looks identical.
* **`defineClass` twice in one loader threw the wrong type** (§3).

## 3. Two items I recorded as OPEN and then fixed — both deferred for wrong reasons

Worth writing down, because in both cases the reason for deferring was a
plausible inference that one lookup would have refuted.

**`Module.canUse`.** I wrote that a faithful implementation must read the
descriptor's `uses` set, and that doing so means calling back into Java from the
native that exists *because* a named `Module` mirror can carry a null
`descriptor` field — re-entrancy on `ServiceLoader.checkCaller`'s path during
`Console.<clinit>`, for one row.

That was reasoning from the registrar's comment about *why the native exists*
rather than from what the context API offers. **`ctx.module_uses(name)` already
exposes the VM's own module registry**: no Java call, no re-entrancy. Unnamed
modules answer true (they genuinely use anything); a module with no registry
entry keeps the permissive answer rather than newly refusing work that used to
succeed; a registered named module is answered from its declared set.

**`defineClass` duplicate → `LinkageError`.** I assumed this needed a new
`LinkageError` enum variant and called it not worth the churn, noting that
`IncompatibleClassChangeError` IS a `LinkageError` subclass so
`catch (LinkageError)` still worked.

`LinkageError::DuplicateClassDefinition` **already existed**, mapping to
`java/lang/LinkageError` with HotSpot's own wording, and its comment in
`vm/src/runtime/exceptions.rs` even names the callers that catch the base type.
The raise site was simply using the wrong variant.

### The part that was actually hard

Not the fix — the blast radius. **Two independent recovery paths keyed on the
old exception's MESSAGE TEXT**, `"already defined by"`, and both handle a benign
concurrent-definition race in which a thread that loses a `defineClass` recovers
by taking the winner's already-registered copy.

Changing the variant without them would have stopped both from matching and
turned a recovered race into a hard failure — a concurrency regression no
single-threaded probe can see. They are fixed differently on purpose:

| consumer | now matches | why |
| --- | --- | --- |
| `classloading/src/class_manager.rs` | the **variant** | it has the typed error; the next wording change cannot break it |
| `native-builtins/src/classloader.rs` | the new marker **and** the old text | it sits at a `Result<_, String>` boundary fed by `format!("{e:?}")`, and that arm serves other producers of the old text |

**The lesson is about exception-type fixes generally: the type is load-bearing
for someone.** Grepping for the old message was the step that mattered, not the
edit.

## 4. The `forName` / `loadClass` asymmetry, and two wrong placements

`Class.forName("[I")` resolves. `ClassLoader.loadClass("[I")` throws. Both
measured against HotSpot 25.0.3+9.

This VM implemented `forName` by delegating to `loader.loadClass(name)`, so **the
two doors could not disagree** — and adding the binary-name refusal that
`loadClass` owes broke `forName` twice:

1. in the shared `cl_real_load_class_base` — `forName` reaches it;
2. at the `cl_real_load_class` entry — `forName` reaches that too.

The second placement was a guess dressed as a fix: I moved the code without
establishing that `forName` did not reach the new site.

The real defect was the **delegation itself**. `Class.forName` needs no loader
for an array descriptor, and now resolves one directly before any delegation.
Both probes assert both halves so the asymmetry is pinned from either side and
cannot be "simplified" back.

### CORRECTION 2026-08-28: the fix that replaced them shipped a THIRD regression

The `forName` short-circuit below is right, and it went out with a defect this
page did not catch, because this page's acceptance ran the GATE SET and not the
three `regression-suite` arms. `Class.forName("[Lcom.foo.Missing;")` must throw
`ClassNotFoundException("com.foo.Missing")` — JVMS 5.3.3 builds an array class
from its ELEMENT type — and the delegation that was removed was ALSO what
produced that message: the loader saw the element name and reported it. The
short-circuit reported the descriptor.

`RJdkFailure.java:168` and `RExceptions.java:382` both assert it and both went
red on the pushed commit. Fixed, with the full account of how it escaped, in
`L5-residuals-module-packages-and-invokeexact-20260828.md` §6.

**So "0 residuals" on this page means "no residual my probes could see".** Three
probes, 483 rows, and a fourth regression from the same three-line change that
none of them asked about. The vectors did.

**Both regressions were caught by the runner's ROW COUNT check**
(`20 of 90 rows -- the missing 70 are UNTESTED, not clean`) and by **re-running
the already-closed probes in the same batch**. `ClassShadowSweep` was green when
it landed; only re-running it showed three of its rows had gone red.
`FieldMethodShadowSweep` stayed 132/132 throughout, which is the control saying
the damage was confined to the loading path.

## 5. Verification

```text
ClassShadowSweep         261/261   0 diffs both modes
FieldMethodShadowSweep   132/132   0 diffs both modes
ClassLoaderShadowSweep    90/90    0 diffs both modes
gate set                 21 test binaries ok
```

The gate set includes `cratonvm-classloading --lib`, which covers the two
concurrent-definition recovery paths changed in §3.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ClassLoaderShadowSweep
```
