# E20 / R11 — the guard that could not fail for the case it was written for, and the census built from its own answer

**Date:** 2026-08-13 **Lane:** E20
**Closes:** NOM E16-2 in `E16-R11-P59-MODULE-LAYER-TWIN-20260813.md` §6.

**This lane did not build or run CratonVM, and did not run `cargo`.** Every
CratonVM "after" below is **PREDICTED**. The HotSpot 25 measurement in §4 was
taken on this host today. Everything else is source read in this working tree.

**Edits applied (owned file only):** `native-builtins/src/phases_late.rs`

| what | where |
|---|---|
| `phase59_module_vs_essential_natives` rewritten as a two-way ratchet over BOTH halves of the partition | `:9965-10315` (was `:9965-10051`) |
| `every_public_mac_method_is_registered` given the JDK's population instead of the registry's | `:9542-9645` |

**Nominations in §6.** None is required for the tree to compile.

---

## 1. (a) The brief's line range and the `difference` claim — both verified, with one correction

`phase59_module_vs_essential_natives` was at `:10006-10051`; the brief's
`:9965-10051` is the doc comment's first line through the closing brace, which
is right. The enclosing `#[cfg(test)] mod essential_vs_synthetic_jdk_coverage_audit`
starts at `:9948` and its `dump_triples` helper at `:9954` — outside the quoted
range, and the rewrite needed both. Nothing was truncated.

The claim is exact. The whole check was:

```rust
let missing: Vec<_> = p59
    .difference(&essential)
    .filter(|t| !already_triaged.contains(*t))
    .collect();
```

`BTreeSet::difference` yields the elements of `p59` that are **not** in
`essential`. A triple registered by both sides is, by construction, absent from
that iterator. `already_triaged` then removed six more. So the population the
guard could ever report was `p59 \ essential \ already_triaged`, and the
question "do the two bodies for the same triple agree?" was not in it.

**What would have made it red, as written:** adding a brand-new triple to
`register_p59_module` for a method `register_essential_natives` does not
register. Nothing else. In particular, neither of the two divergences E16 §1.4
established could ever have tripped it, because both are on triples both
registrars install.

## 2. The partition, computed by reading

`register_p59_module` (`native-builtins/src/phases_late/reflect_invoke.rs:2981-3363`)
registers **21** triples. Every one of its `r.register(` calls is at statement
level in the function body — none is behind an `if` or a `#[cfg]`.

`register_essential_natives` → `register_essential_natives_with_shims`
(`native-builtins/src/lib.rs:7097`, `:7103`). Two `#[cfg(feature = "synthetic-jdk")]`
blocks live inside it, `:7168-8718` and `:11153-11173`; neither registers any
`java/lang/Module`, `java/lang/ModuleLayer`, `java/lang/module/ModuleDescriptor`
or `java/lang/Class.getModule` triple, so **the partition below is the same in
both feature configs** — which is the thing `[2cfgs]` says to check before
believing a `#[cfg(test)]` guard.

**3 of the 21 are p59-only:**

| triple | why it is not on the essential path |
|---|---|
| `java/lang/Module.isNamed()Z` | real-bytecode fallback reads the dual-written `name` field |
| `java/lang/Module.toString()Ljava/lang/String;` | real `toString` never touches the null `descriptor`/`reads` |
| `java/lang/Module.addReads(Ljava/lang/Module;)Ljava/lang/Module;` | real `implAddReads` delegates to `addReads0`, which IS essential-registered (`lib.rs:12260`) |

**18 of the 21 are in the intersection** — registered by both, invisible to the
old check:

| triple | essential-side owner | shape |
|---|---|---|
| `ModuleLayer.boot()` | `jboss_jdkspecific.rs:2012` | TWIN, **known divergent** |
| `ModuleLayer.modules()` | `jboss_jdkspecific.rs:2030` | TWIN, **known divergent** |
| `ModuleLayer.findModule(String)` | `jboss_jdkspecific.rs:2018` | TWIN |
| `Module.getName()` | `jboss_jdkspecific.rs:2058` | TWIN |
| `Module.getPackages()` | `jboss_jdkspecific.rs:2059` | TWIN |
| `Module.getLayer()` | `jboss_jdkspecific.rs:2065` | TWIN |
| `Module.canRead(Module)` | `lib.rs:8826` | **SHARED FN** (`phases_late::native_module_can_read`) |
| `Module.addExports(String,Module)` | `lib.rs:8843` | **SHARED FN** |
| `Module.addOpens(String,Module)` | `lib.rs:8849` | **SHARED FN** |
| `Module.getDescriptor()` | `lib.rs:12229` | TWIN — **was listed as p59-only** |
| `Module.isExported(String)` / `(String,Module)` | `lib.rs:12145` / `:12160` | TWIN |
| `Module.isOpen(String)` / `(String,Module)` | `lib.rs:12179` / `:12194` | TWIN |
| `ModuleDescriptor.name()` | `reflect_annotations.rs:2220` | TWIN |
| `ModuleDescriptor.isAutomatic()` | `reflect_annotations.rs:2276` | TWIN — **was listed as p59-only** |
| `ModuleDescriptor.isOpen()` | `reflect_annotations.rs:2282` | TWIN — **was listed as p59-only** |
| `Class.getModule()` | `lib.rs:11983` | TWIN — two independent canonical-Module caches for one identity invariant |

`register_jboss_jdkspecific` reaches the essential path through
`lib.rs:10004`; `register_module_builder_overrides` through `lib.rs:10074`.
Both call sites are unconditional statements in
`register_essential_natives_with_shims`.

### 2.1 Three rows had already rotted into permission

`Module.getDescriptor`, `ModuleDescriptor.isAutomatic` and
`ModuleDescriptor.isOpen` sat in `already_triaged` with a note saying the fix
lived on an unmerged branch (`fix/es-module-getdescriptor-null`) and would
"resolve once that branch merges". **It merged.** All three are registered on
the essential path today, so all three had left the `difference` population
entirely — the filter kept testing them against an iterator they could never
appear in. A one-way list cannot notice that. That is not a hypothetical decay
mode; it is 3 of the list's 6 rows, found the first time anyone looked.

## 3. (b) The fix — a two-way ratchet over both halves

The guard now declares the whole partition, in two `const`s of
`(class, method, descriptor, why)` rows, and fails on **four** kinds of input:

1. a p59 triple absent from `essential` and unnamed in `P59_ONLY` — the
   original check, unchanged in strength;
2. a triple registered by **both** and unnamed in `P59_AND_ESSENTIAL` — the
   blind spot;
3. a row on either list that no longer matches reality — the reverse direction.
   A triple that stops being p59-only (because the essential path picked it up)
   fails with "MOVE the row"; a triple that stops being double-registered fails
   with "DELETE the row";
4. a triple named twice, or named on both lists.

Because 1 and 2 partition p59 exactly, **every** triple `register_p59_module`
registers must now appear on exactly one list. Adding a registration to that
function is loud no matter which side it lands on.

The fourth field is a prose reason, and each intersection row is classified
`SHARED FN` (both sides register the same `fn` item — one body, safe by
construction) or `TWIN` (two separately written bodies). The two rows E16
established as divergent carry that fact and a pointer to E16 §1.4 / §5.

**What this does NOT do, stated so nobody reads more into a green run:** it
does not compare bodies. It cannot tell you that p59's `ModuleLayer.modules()`
dropped the `servicesCatalog` side effect. It makes an unnamed intersection
loud and a stale name loud, so the divergence has a place to be written down
and a moment where somebody has to write it.

### 3.1 Mutation controls — what turns each new assertion red

| # | mutation | which assertion fires |
|---|---|---|
| M1 | add `r.register(m, "getClassLoader", "()Ljava/lang/ClassLoader;", …)` to `register_p59_module` (essential registers it, `jboss_jdkspecific.rs:2219`) | UNDECLARED in `P59_AND_ESSENTIAL` |
| M2 | add a p59 registration for a method nothing else registers | UNDECLARED in `P59_ONLY` |
| M3 | delete the `Module.canRead` registration from `lib.rs:8826` | STALE in `P59_AND_ESSENTIAL` **and** UNDECLARED in `P59_ONLY` |
| M4 | delete `r.register(m, "isNamed", …)` from `register_p59_module` | STALE row in `P59_ONLY` |
| M5 | register `Module.isNamed` on the essential path (the presumed "fix") | STALE in `P59_ONLY` + UNDECLARED in `P59_AND_ESSENTIAL` — i.e. the guard demands the row be MOVED, not deleted |
| M6 | copy any row so it appears twice | the `declared()` duplicate assertion |
| M7 | list one triple on both `const`s | the `on_both_lists` assertion |

M3 and M5 are the ones the old check could not produce at all.

**PREDICTED:** with the tree as it stands, `phase59_module_vs_essential_natives`
passes. If it does not, the failure text names the triple and which of the four
kinds it is, and the fix is a list edit, not a code edit — unless it is M3's
shape, in which case something on the essential path was dropped.

## 4. (c) The two named divergences — both in a file this lane does not own

Both bodies are in `native-builtins/src/phases_late/reflect_invoke.rs`, which
lane E16 edited today. NOM E20-1 and E20-2 below.

### 4.1 The layer width — 1 vs 2, and what actually reads the second slot

**Against the real class, neither number is right.** `javap -p java.lang.ModuleLayer`
on openjdk 25.0.3+9 (this host, today) declares **six** instance fields:

```
private final java.lang.module.Configuration cf;
private final java.util.List<java.lang.ModuleLayer> parents;
private final java.util.Map<java.lang.String,java.lang.Module> nameToModule;
private volatile java.util.List<java.lang.ModuleLayer> allLayers;
private volatile java.util.Set<java.lang.Module> modules;
private volatile jdk.internal.module.ServicesCatalog servicesCatalog;
```

So `2` is not "the real width" either. `2` is **this VM's declared synthetic
width** — `classloading/src/class_manager.rs:15403` maps
`"java/lang/ModuleLayer" => instance_fields(2)`, and
`instance_fields(n)` (`class_manager.rs:11929`) fabricates fields literally
named `_f0 … _f{n-1}`, all `Ljava/lang/Object;`. `jboss_jdkspecific.rs:190`
declares the same 2 as `MODULE_LAYER_FIELD_COUNT` and documents only slot 0
("`boot` boolean flag"). **Slot 1 is undocumented and nothing in the tree reads
it by index.** That is the honest answer to "what reads that second field":
*nothing does, by index* — and by name, `build_boot_layer`
(`jboss_jdkspecific.rs:298-313`) writes `parents`, `nameToModule` and `modules`
through `set_field_by_name`, which on the synthetic class is asking for names
that class does not declare.

**Is the `1` a wrong-type-read hazard? Not at the allocation, and the reason
matters.** `try_alloc_concurrent_synthetic`
(`native-builtins/src/util_concurrent_ext.rs:929`) ends its success arm with:

```rust
let n = num_fields.max(real);
```

The requested count is a floor, not the width: an under-request is clamped up
to the class's own declared count. So p59's `boot()` asking for 1 still yields a
2-slot object whenever `java/lang/ModuleLayer` resolves. What the `1` actually
buys is a `report_layout_alias(class_name, 1, 2)` hit on every call, via
`layout_alias::classify` — the mismatch detector, firing on a mismatch that is
this file's own.

The hazard is in the **other** arm. When `ensure_class_initialized` fails, the
same function takes `refused_class(ctx, class_name, num_fields)` →
`try_ensure_synthetic_class(class_name, 1)` and then `alloc_object(cid, 1)` —
fabricating a **1-field** `java/lang/ModuleLayer` class and an object to match.
That is the `[Ok≠use]` shape: the fallback fabricates rather than fails, and the
width it fabricates is the caller's guess. A later `MODULE_LAYER_FIELD_COUNT`
write into that class is then out of bounds by construction.

**Verdict for the nomination: `1` should be `2`.** Not because 2 is the real
JDK width (it is 6), but because 2 is the width this VM *declares* for its
synthetic stand-in, is what the essential twin allocates, and is the only number
that keeps the two bodies interchangeable — which is exactly the property the
intersection makes load-bearing. The same argument applies to p59's
`java/lang/Module` allocations at `1`-off widths (it asks for **2**;
`class_manager.rs:15407` declares **5**; `jboss_jdkspecific.rs:214`
`MODULE_FIELD_COUNT` is **5**) and its `java/lang/module/ModuleDescriptor` (it
asks for **2**; `jboss_jdkspecific.rs:1642` and `reflect_annotations.rs:1672`
both allocate **16**). Same family, `[w×3]`.

### 4.2 The `servicesCatalog` side effect

`service_loader.rs:775-785` invokes `ModuleLayer.modules()` for its side effect,
discards the result, and then reads `layer.servicesCatalog` by name. The
essential twin (`jboss_jdkspecific.rs:1093-1160`) derives the catalog from
`nameToModule` and writes it back. p59's body ignores its receiver entirely.
In synthetic-jdk mode p59 wins, so that read finds nothing. Unchanged by this
lane; NOM E20-2.

## 5. (d) The sweep — other guards of the same shape in this file

`.difference(` appears **once** in `phases_late.rs` (the guard above), so the
literal shape does not recur. The broader shape — *a census whose population
excludes the interesting case* — does, once, and it is worse than the first
because it says out loud that it is complete.

### 5.1 `every_public_mac_method_is_registered` — the population was the answer

The doc comment reads *"Every PUBLIC method of `javax.crypto.Mac` must be
registered — not most of them"*, and explains that this exists because
`doFinal([BI)V` once went missing and broke every SCRAM-SHA-256 login. The
list under it held **16** rows.

`javap -public -s javax.crypto.Mac` on openjdk 25.0.3+9 reports **17** public
methods. The missing one is

```
public static final javax.crypto.Mac getInstance(java.lang.String, java.security.Provider)
    descriptor: (Ljava/lang/String;Ljava/security/Provider;)Ljavax/crypto/Mac;
```

and it is registered **nowhere** — `grep -rn 'javax/crypto/Mac' --include=*.rs`
over the whole tree returns `phases_late/ssl_security.rs` (the registrar, which
installs the other two `getInstance` overloads at `:193` and `:234`),
`phases_late.rs` (this test) and `vm/src/vm/tests.rs`. So the list was 16 of
17 and every one of the 16 passes: the census was assembled from the registry it
was meant to audit, and could not go red for a missing overload — the one
failure it was written for.

**Fixed here** by giving it the JDK's population: 17 rows, each with an
`expect_registered` flag ratcheted in both directions, and the one `false` row
carrying a note that says it is an untriaged gap rather than an accepted one.
Registering the overload now requires deleting the `false` — the reverse
direction, the `harness-uncounted.txt` rule.

**What makes it fail now:** any of the 16 registrations disappearing, **or**
`getInstance(String, Provider)` being registered without its row being deleted.
**What still would not:** JDK 26 adding an eighteenth public method. The
population is a transcript of `javap`, taken by hand, dated in the doc comment.
That is a real residual and the honest fix is a generated list, which is out of
scope for a lane that cannot run the build.

### 5.2 Two guards examined and deliberately left alone

* `b6_submission_publisher_core_methods_registered` (`:8989`) asserts 4 triples
  on `SubmissionPublisher`. **Fails if** one of those exact four registrations
  is deleted. It cannot fail for a fifth method going missing — but it is named
  "core methods", claims nothing more, and no defect is attributed to it.
* `sq_real_rendezvous_methods_registered` (`:9304`) asserts 5 triples on
  `SynchronousQueue`, whose registrar
  (`phases_late/concurrent.rs:1819`) installs at least 14. **Fails if** one of
  those five is deleted. Same limitation, same honest scope.

Neither was widened: turning a smoke test into a census means deciding what the
complete population is and what each absence means, and doing that from
`javap` without a run is how a green gate becomes a red one for a reason nobody
measured. They are recorded here so the next reader does not have to re-derive
that they are partial.

## 6. NOMINATIONS

### NOM E20-1 — `native-builtins/src/phases_late/reflect_invoke.rs:2991` — the layer width

Owned by the `reflect_invoke.rs` lane. Anchor verified unique in the working
tree today.

OLD:

```rust
        let layer = try_alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", 1)?;
```

NEW:

```rust
        // 2, not 1: `class_manager.rs:15403` declares the synthetic
        // `java/lang/ModuleLayer` as `instance_fields(2)` and the essential twin
        // allocates `MODULE_LAYER_FIELD_COUNT = 2`
        // (`jboss_jdkspecific.rs:190`). Requesting 1 does NOT under-allocate —
        // `try_alloc_concurrent_synthetic` ends with `num_fields.max(real)` — but
        // it does fire `report_layout_alias(.., 1, 2)` on every call, and in the
        // `ensure_class_initialized` failure arm it fabricates a genuinely
        // 1-field class via `refused_class(ctx, name, 1)`. This triple is in the
        // p59/essential INTERSECTION (see phases_late.rs
        // `P59_AND_ESSENTIAL`): the two bodies must stay interchangeable,
        // because which one runs is decided by the VM mode, not by the caller.
        let layer = try_alloc_concurrent_synthetic(ctx, "java/lang/ModuleLayer", 2)?;
```

**PREDICTED effect:** none observable at runtime in either mode (the allocation
was already clamped to 2); one fewer layout-alias report per
`ModuleLayer.boot()` call in synthetic-jdk mode; the class-load-failure arm
stops fabricating a 1-field `ModuleLayer`.

### NOM E20-2 — `native-builtins/src/phases_late/reflect_invoke.rs:3038` — `modules()` drops the catalog side effect

No exact replacement text offered, deliberately: E16 §5.1 already established
that repairing this means deciding whether synthetic-jdk mode should use
`jboss_jdkspecific`'s boot layer at all, which changes behaviour for every
synthetic-mode `ServiceLoader` call and needs a synthetic-mode measurement to
land. This nomination adds one fact E16 did not have: **the guard that was
supposed to notice this now names the row.** `P59_AND_ESSENTIAL` in
`phases_late.rs` carries `java/lang/ModuleLayer.modules()` marked
`TWIN, KNOWN DIVERGENT` with the `service_loader.rs:775` dependency written
into it, so whoever next edits either body is told the other exists.

### NOM E20-3 — `native-builtins/src/phases_late/ssl_security.rs` — register the seventeenth `Mac` method

`javax.crypto.Mac.getInstance(String, java.security.Provider)` is public,
final, static, and unregistered (§5.1). The other two `getInstance` overloads
allocate a 4-slot synthetic `javax/crypto/Mac` and seed `mac_state_table`; this
one falls through to the real JDK body, so it returns a `Mac` with **no**
`mac_state_table` row, while `init`/`update`/`doFinal` on that object are all
intercepted by natives that expect one. Whether that reaches a caller is
**unmeasured** — this lane cannot run the VM, and the fact that nobody has
measured it is precisely because the census that claimed to cover this class
never listed the method.

Suggested shape: the same body as the `(String, String)` overload, resolving the
provider argument by `Provider.getName()` instead of by string. It is registered
right after `:234` in that file.

The row in `phases_late.rs` must be deleted in the same change — the census
fails if the method becomes registered while its `false` row survives.

### NOM E20-4 — `docs/known-issues/jdk-only/E16-R11-P59-MODULE-LAYER-TWIN-20260813.md` §6 NOM E16-2 — close it

Owned by lane E16. The suggested form was "compute `p59.intersection(&essential)`
and assert it against a checked-in allowlist". Landed, with one change worth
recording: the allowlist is **two-way**, and so is the pre-existing
`already_triaged` list, which is why three of its six rows turned out to be
stale (§2.1). E16's suggested membership was
`ModuleLayer.{boot,modules,findModule}` and `Module.{getName,getLayer,getPackages}`
— correct as far as it goes; the actual intersection is **18** triples, three
times that.

## 7. The lesson

**A guard's population is a claim, and it is usually the least examined line in
the file.** Two instruments here, both green, both unable to fail for the thing
they were built for, and both for the same reason: the set they compared was
derived from one side of the comparison. `p59.difference(&essential)` cannot
contain a triple both sides register. A hand-written list of "every public
method" assembled by reading the registrar cannot contain a method the registrar
forgot. In each case the gate was green and nobody could say what would make it
red — and in each case the answer, once written down, took one `javap` and one
`grep`.

**And a triaged list is a dated claim, not a fact.** Half of
`already_triaged`'s rows said "resolves once branch X merges". X merged. Nothing
told anybody, because the only direction the check ran was the one that adds
rows.
