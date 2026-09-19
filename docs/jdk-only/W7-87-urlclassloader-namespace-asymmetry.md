# W7-87 — a bare `URLClassLoader` saw application classes it never loaded

**Status: FIXED 2026-08-12, with a vector, a probe and a measured consumer
census.** `new java.net.URLClassLoader(urls, null)` — the standard idiom for a
loader that deliberately cannot reach the application classpath — resolved
application classes anyway. `loadClass` and `Class.forName(name, false, loader)`
returned the *application* loader's class where HotSpot 25 raises
`ClassNotFoundException`, and `findLoadedClass` reported it where HotSpot
reports `null`. Reproduced in **both** modes, fixed in the same shared helper
W7-82-forname-duplicate-define.md fixed, in the opposite direction.

Branch `fix/bare-urlclassloader-namespace-asymmetry-20260812`, off dev
`d40417496` (the W7-82 merge).

This is the residual W7-82 named, measured and deliberately did not fix. That
record was right that it is a second axis; it was wrong only in scale — the
leak is not confined to `findLoadedClass`. It reaches `loadClass`.

## What the predicate actually does

`is_builtin_loader_class` lists `java/net/URLClassLoader`, so
`is_user_defined_loader` answers **false** for a bare instance. But
`loader_namespace_id_at` and `peek_loader_namespace_id` both spell their guard
`!is_user_defined_loader(..) && !is_bare_url_class_loader(..)`, so the namespace
allocator treats the same object as **user-defined** and hands it namespace
id ≥ 3.

`find_loaded_class_for_loader_inner` was the one site left out of that carve-out,
and it therefore disagreed with the allocator in **both** directions:

| direction | built-in-branch clause | symptom | record |
|---|---|---|---|
| too narrow | `loader_id_of_class(cid) > 2 -> None` | the loader could not see the class **it had itself defined**, so every repeat lookup re-drove the define and the second `Class.forName` threw | W7-82, fixed 08-12 |
| too wide | the **global fallback** `ctx.class_id_by_name(..)` | the loader saw **application-namespace classes it never defined and was never asked to load** | this record |

W7-82 was closed additively (a miss fell through unchanged). This half is the
opposite direction — it must start returning `None` where it returned a class —
so it is a **narrowing**, with real blast radius. Said plainly, and assessed
below.

## Measured: what a bare `URLClassLoader` could see

`probes/UclLeakChar` (scratch), HotSpot 25.0.3 beside one CratonVM binary, mode
flag as the only variable. `bareBoot` = `new URLClassLoader("bareBoot",
new URL[]{auxDir}, null)`; `bareApp` = `new URLClassLoader("bareApp",
new URL[0], appLoader)`; `subBoot` = a one-line `extends URLClassLoader` with
the same arguments; `UclLeakChar` is an application-classpath class the
application loader is already holding.

| shape | HotSpot 25 | CratonVM (both arms) |
|---|---|---|
| `bareBoot.findLoadedClass("UclLeakChar")`, never asked | `null` | **`UclLeakChar@app`** |
| `bareBoot.loadClass("UclLeakChar")` | `ClassNotFoundException` | **`UclLeakChar@app`** |
| `Class.forName("UclLeakChar", false, bareBoot)` | `ClassNotFoundException` | **`UclLeakChar@app`** |
| `bareBoot.loadClass(<app class loaded meanwhile>)` | `ClassNotFoundException` | **leaked** |
| `bareBoot.findLoadedClass("java.util.zip.CRC32")` after loading it | `null` | **`CRC32@<boot>`** |
| `bareApp.findLoadedClass("UclLeakChar")`, never asked | `null` | **`UclLeakChar@app`** |
| **`subBoot.findLoadedClass("UclLeakChar")`** | `null` | `null` ✔ |
| **`subBoot.loadClass("UclLeakChar")`** | `ClassNotFoundException` | `ClassNotFoundException` ✔ |
| `stranger.loadClass(<another bare loader's class>)` | `ClassNotFoundException` | `ClassNotFoundException` ✔ |

The **subclass rows are the discriminator again**, and they point the same way
they did in W7-82: same URLs, same parent, same call, one line of `extends`
between pass and fail. A subclass is `is_user_defined_loader`, takes the
user-defined branch, and that branch has **no global fallback** — which is
exactly the answer the bare instance needed.

**Route.** The isolating shape is not a `findLoadedClass` curiosity. In
`cl_load_class`, a loader with `parent_is_null && has_private_url_path` and a
non-bootstrap class name is routed straight into `ucl_try_define_local_class`,
whose **first line** is `find_loaded_class_for_loader`. The global fallback
answered there, so the loader's own URL search never ran. That function's own
doc comment already names the rule it was breaking — *"would let a
`URLClassLoader(urls, null)` resolve application classes its own (failed) URL
search should have hidden from it"* — the intention was written down and one
blind probe overrode it, the same shape as W7-82's four defeated recoveries.

**No VM-supplied loader is in range.** Measured on both arms and on HotSpot:
system / application / TCCL are `jdk.internal.loader.ClassLoaders$AppClassLoader`,
the platform loader is `ClassLoaders$PlatformClassLoader`. The only bare
`java.net.URLClassLoader` instances in a run are ones the application
constructed, plus whatever `URLClassLoader.newInstance(urls, parent)` returns
(measured: a bare `java.net.URLClassLoader` on both HotSpot and CratonVM). This
bounds the whole blast radius.

## Defining vs initiating — read, then measured, because guessing gets it backwards

`java.base/java/lang/ClassLoader.java` in `lib/src.zip` documents `loadClass` as
*"Invoke `findLoadedClass(String)` to check if the class has already been
loaded"*, then parent, then `findClass`. `findLoadedClass` itself is
`protected final` over the native `findLoadedClass0`, which asks the SystemDictionary
whether **this loader** is a recorded loader of that name. The record HotSpot
keeps is the **initiating**-loader record, not merely the defining one — and the
sharp edge is exactly which operations write to it:

| operation on loader L, for name N | does L become a recorded loader of N? | measured |
|---|---|---|
| `L.defineClass(N, ...)` | **yes** (defining ⇒ initiating) | `findLoadedClass` → the class |
| `Class.forName(N, false, L)` — JVM-driven | **yes**, even when the class was defined by L's PARENT | `findLoadedClass` → the parent's class |
| constant-pool resolution from a class defined in L | yes | (same road as above) |
| **`L.loadClass(N)` — an ordinary Java call** | **NO** | `findLoadedClass` → **`null`**, even though `loadClass` just returned the class |

That last row is the one that cannot be guessed. `bareBoot.loadClass(
"java.util.zip.CRC32")` succeeds, and `bareBoot.findLoadedClass(
"java.util.zip.CRC32")` immediately afterwards is still `null` on HotSpot 25 —
a Java-level `loadClass` call is not a JVM-initiated load and updates nothing.
Both group 6 assertions in the probe rest on this, and both were measured before
being written down.

**What CratonVM actually implements, and what this fix does NOT change.**
CratonVM has no initiating-loader record at all. Its user-defined branch answers
`findLoadedClass` from *defining* identity only: own namespace, then
`class_defined_by_this_loader_object`, then "the global class of this name
records THIS loader as definer". Measured consequence, on every user loader,
before and after this change:

```
HotSpot   forName(TARGET, false, e); e.findLoadedClass(TARGET) -> TARGET@c   (e initiated it)
CratonVM  same shape                                           -> null
```

So CratonVM **under-reports** initiated-but-not-defined classes. That is
pre-existing, shared by every user-defined loader in the VM, and left alone
here — deliberately, and for a reason that decides the direction of this fix:

* **under-reporting is benign.** A `None` from this function sends the caller to
  real delegation, which returns the same class. It costs a probe, not an answer.
* **over-reporting is the leak.** A `Some(someone else's class)` *replaces* the
  delegation that would have produced the right answer, or the
  `ClassNotFoundException` that would have been the right answer.

Getting the rule backwards — "HotSpot reports initiated classes, so widen" —
would have kept the leak and called it parity. The rule implemented is the
narrower, safer one, and it is the same rule every other user loader in this VM
already lives under. Closing the initiating-loader gap is a separate lane; it is
named as a residual at the end.

## What changed

**`native-builtins/src/classloader.rs`,
`find_loaded_class_for_loader_inner` — one branch predicate, mode-independent.**
The built-in branch is now taken only when the loader is genuinely built-in:

```rust
let takes_builtin_branch = !is_user_defined
    && (is_generated_proxy_name(internal_name) || !is_bare_url_class_loader(ctx, this));
if takes_builtin_branch {
    return ctx.class_id_by_name(internal_name).and_then(|cid| { /* unchanged */ });
}
```

A bare `URLClassLoader` now takes the **user-defined branch** outright — exactly
the predicate `loader_namespace_id_at` and `peek_loader_namespace_id` use, so
the two halves finally agree.

**W7-82's block is deleted, not weakened.** Its two additive probes were
`peek_loader_namespace_id` + `class_id_defined_by_loader_exact`, then
`class_defined_by_this_loader_object`. Those are *verbatim* steps 1 and 2 of the
user-defined branch this now falls into, which then adds a third ("a globally
known class whose recorded defining loader IS this object"). The fix therefore
**subsumes** W7-82 and gives up nothing it fixed; group 8 of the probe asserts
that directly, and `ForNameCacheProbe` groups 1–3 are the end-to-end form.

**One case stays on the built-in branch: a generated-proxy name.** That arm is
already loader-identity- and delegation-aware (`proxy_hidden_from` →
`loader_can_see_defining`), so it is not a leak — and
`classloader_real::load_class_visible_to` short-circuits proxy resolution to
this function **before** parent delegation runs, so dropping it would convert a
proxy a bare `URLClassLoader` can legitimately see through its parent into a
`ClassNotFoundException`. Narrowing one axis at a time.

**Mode statement.** One edit, in a helper both modes share, reached identically
by `findLoadedClass` (`classloader.rs`, `classloader_real.rs` ×2),
`Class.forName` (`lang_class.rs`), `ucl_try_define_local_class` (×4) and eight
other sites. Admissible under the `Compatible` freeze as a genuine HotSpot-parity
fix — HotSpot's answer was measured for every shape in the table above, not
inferred — but it is a **narrowing**, so see the blast radius before merging.
`classloader.rs` sets no ambient `NativeKind` over the edited region, and
`find_loaded_class_for_loader` is a plain helper, not a registration, so
last-write-wins has nothing to decide about it. **No `CRATONVM_*` variable was
added**, so nothing in `flag_groups.rs`, `flag-surface.txt`, `flag-tokens.md` or
`flag-inventory.md` moves.

**Which registration wins** was re-established for this lane rather than
inherited: `URLClassLoader.findClass` still has the three registrations W7-82
listed (`classloader.rs` `ucl_find_class`, `lib.rs`
`classloader_real::ucl_real_find_class`, and the inline closure in `servlet.rs`
that resolves globally). The fix does not depend on the winner — the two
mode-specific bodies share `ucl_try_define_local_class`, and the servlet closure
demonstrably does not win in either mode, because if it did, probe group 3's
"two bare loaders yield two distinct classes" would fail, and it passes on both
arms both before and after. Determined by reading and by measurement, **never by
indentation**: the column-0 `}` at `native-builtins/src/lib.rs:11604` is a
mis-indented closure tail, not a function end.

**One unit test**, `classloader_tests::
test_bare_url_class_loader_does_not_see_an_app_namespace_class_it_never_loaded`.
The receiver is a **bare** `java/net/URLClassLoader` on purpose: the 2026-07-01
commit that produced W7-82 shipped two tests that instantiate
`java/lang/ClassLoader`, which is precisely why that case survived six weeks,
and a test written against a `URLClassLoader` *subclass* cannot fail here
either. The built-in-loader control is asserted in the same test, beside the
untouched `test_builtin_find_loaded_class_keeps_application_namespace_hit`.
**No existing test was weakened.**

## Group 6 flipped — half of it

Asked directly, because W7-82 left group 6 as a pin of the current behaviour so
that record would not freeze an answer it was not settling.

| group 6 assertion | before | after | why |
|---|---|---|---|
| `forName` ×2 through a delegating bare `URLClassLoader` are `==` | PASS | PASS | HotSpot-correct already; unchanged |
| …and equal the parent's class | PASS | PASS | HotSpot-correct already; unchanged |
| `l3.findLoadedClass("ForNameCacheProbe")`, never initiated | *not asserted*; VM answered `ForNameCacheProbe@app` | **asserted `null`** | **flipped** |
| `l3.findLoadedClass("java.lang.String")`, never initiated | *not asserted*; VM answered `String@<boot>` | **asserted `null`** | **flipped** |
| `l3.loadClass("ForNameCacheProbe") == ForNameCacheProbe.class` | *not asserted* | asserted | new over-correction guard |

So: the group's `forName` half did **not** flip — it was HotSpot-correct all
along, and it is now the guard that narrowing the cache probe has not broken
parent-first delegation. What flipped is the part the group pinned without
asserting: what `findLoadedClass` reports there. The group was **updated with
its new expected value, not deleted**, exactly as W7-82 asked.

Group 7 — the duplicate-`defineClass` `LinkageError` rule from 2026-08-11 — is
untouched and still passes on all three arms. A "fix" that deleted the
duplicate-define check still cannot pass.

## The predicate's full consumer population

Every call site of `is_builtin_loader_class` / `is_user_defined_loader` /
`is_bare_url_class_loader` / `loader_namespace_id{,_at}` /
`peek_loader_namespace_id`, read (not grepped) and classified. Doc-comment
mentions and `#[cfg(test)]` uses are excluded. **This census is the deliverable,
not a side effect: a single missing site cost six weeks last time.**

The carve-out exists in exactly **three** shapes today:

1. the namespace allocator's `&& !is_bare_url_class_loader(..)` —
   `classloader.rs` `loader_namespace_id_at`, `peek_loader_namespace_id`, and
   (as of this fix) `find_loaded_class_for_loader_inner`;
2. the resource layer's `object_extends(.., "java/net/URLClassLoader") ||` —
   `classloader.rs` `cl_get_resource`, `cl_get_resources_impl`,
   `cl_get_resource_as_stream`; `lang_class.rs` `native_class_get_resource{,_as_stream}`;
3. the `findClass`-walk's explicit `name == "java/net/URLClassLoader"`
   pre-check — `classloader.rs` `receiver_overrides_find_class`,
   `find_class_is_urlclassloader_native`, `receiver_overrides_load_class`.

### SYMMETRIC (13)

`classloader.rs`: `receiver_overrides_find_class`,
`find_class_is_urlclassloader_native`, `receiver_overrides_load_class` (shape 3);
`find_loaded_class_for_loader_inner` — **as of this fix**; its
`peek_loader_namespace_id` probe; `cl_get_resource`, `cl_get_resources_impl`,
`cl_get_resource_as_stream` (shape 2); `ucl_try_define_local_class`'s two
`loader_namespace_id` calls; `get_or_assign_loader_id`.
`lang_class.rs`: `native_class_get_resource_as_stream`, `native_class_get_resource`
(shape 2); `native_class_get_component_type`;
`i2_classloader_get_defined_package(s)`; `native_class_get_class_loader` (the
`is_subclass` disjunct already answers, the predicate is redundant there).
`lookup_define.rs` `inherit_lookup_loader`; `cglib_enhancer.rs`
`loader_namespace_for_object`; `vm/src/runtime/interpreter/constants.rs`
`resolve_class_loader_aware`.

`locale_resources.rs` `caller_bundle_class_loader` is a **deliberate
non-consumer**: it rejects the app-loader singleton by pointer identity and its
comment says explicitly that it avoids `is_user_defined_loader` *because* that
predicate would exclude a bare `java.net.URLClassLoader`. One site in this tree
already reasoned about the asymmetry and routed around it.

### ASYMMETRIC (17) — the population this record is really for

None of these is fixed here. They are listed so the next lane does not have to
find them again, roughly in descending order of sharpness.

| site | what a bare `URLClassLoader` gets wrong |
|---|---|
| `lang_system.rs` `native_classloader_define_class1` / `..._class2` / `..._class0` (namespace pick, 3 sites) | falls to the `_ => 0` arm and defines into namespace **0 (Application)** while `ucl_try_define_local_class` and `loader_id_for` give the **same loader** ns ≥ 3. A true namespace split for one loader object; two bare loaders (or one bare loader and the app loader) then collide as `IncompatibleClassChangeError: already defined by application loader` — the exact failure `is_bare_url_class_loader` was written to stop. **Sharpest of the set.** |
| `reflect_annotations.rs` `proxy_loader_namespace` | a proxy made with a bare `URLClassLoader` is keyed under `identity_hash_code(loader)` instead of its canonical ns ≥ 3, so the loader's own `loadClass(proxyName)` cannot find it — the `ClassUtilsTests.isCacheSafe` shape that function's own doc says it fixed |
| `reflect_annotations.rs` `native_proxy_new_instance`, `native_proxy_get_proxy_class` | `proxy.getClass().getClassLoader()` reports the **app** loader, not the bare loader passed to `newProxyInstance` |
| `classloader.rs` `define_class_via_full` | defines into ns ≥ 3 but writes **no** `defining_loader_store` row, so `Class.getClassLoader()`, `inherit_lookup_loader` and `resolve_class_loader_aware` all fall back to the app loader — the class lives in a private namespace yet reports as app-loaded. Two SYMMETRIC sites above inherit this gap rather than adding one. |
| `classloader.rs` `builtin_loader_reachable` | short-circuits to `true` on the loader's **own** class, so `scoped_user_chain` is false even for `new URLClassLoader(urls, null)`. Latent: the `url_classloader_isolated_from_app` branch intercepts null/platform-parented URL loaders first (two callers: `classloader.rs`, `classloader_real.rs`) |
| `classloader.rs` `cl_load_class_base_delegation_rooted` (own-class-first probe) and `classloader_real.rs` `cl_real_load_class_base_rooted` (JVMS §5.3.2 step 1) | the own-namespace-first probe is skipped, so `loadClass` can hand back the app loader's same-named class instead of the copy this loader defined |
| the same two functions' **parent** checks | a child parented to a bare `URLClassLoader` never runs the parent's own URL search; in real mode `parent_user_defined_authoritative_miss` is also never set, so the parent's deliberate miss is re-resolved globally |
| `lang_class.rs` `native_class_for_name` | `Class.forName(n, init, bareUCL)` loses the "the JVM already knows this loader defined it" fast path |
| `lang_class.rs` `wrap_annotation_in_real_proxy` | an annotation loaded by a bare `URLClassLoader` gets a proxy reporting the app loader — the `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader` shape, one loader kind short |
| `lang_system.rs` `preload_supertypes_via_loader` | the `defineClass1/2` route skips the initiating-loader supertype preload (the `ucl_try_define_local_class` route uses the ungated `preload_isolated_loader_supertypes`, so only that road is affected) |
| `service_loader.rs` `usable_loader`, and two sites in `discover_providers` | `ServiceLoader.load(S, new URLClassLoader(urls))` never drives that loader; providers and `../../../apps/META-INF/services` descriptors reachable only through its URLs are silently dropped to the flat `-cp` scan |
| `phases_late/jar_manifest.rs` `spring_class_utils_for_name_impl` | `ClassUtils.forName` collapses onto the loader-blind global resolution, busting `Class`-identity-keyed caches |
| `spring_startup_bootstrap.rs` `resolve_class_id_via_tccl` | returns `None` for a bare-`URLClassLoader` TCCL, so resolution falls to the global table |

### DEAD (2)

`classloader.rs` `cl_load_class_base_delegation_rooted`'s `loader_namespace_id`
re-probe and `lang_system.rs` `preload_supertypes_via_loader`'s namespace probe
are both unreachable for a bare `URLClassLoader` — each sits *behind* an
asymmetric guard listed above, so neither can be reached until that guard is
fixed. They are dead only in this sense; fixing the guard revives them, and they
look correct when it does.

**Read the shape of that list.** The asymmetric sites are not scattered: they
cluster on **defining** (`defineClassN`, `define_class_via_full`, proxies) and on
**delegation** (`loadClass`'s own-class and parent probes). The three
`defineClassN` namespace picks are the only ones that can produce a *hard* error
today, and they produce the one this predicate was invented to prevent. They
want their own lane, their own oracle, and a full suite run — the same sentence
W7-82 wrote about this record's subject, which is why this one exists.

## The vector and the probe

`regression-suite/src/RLoaderChurnDefine.java` (`CORE_CLASSES`, both modes and
HotSpot), new section `anIsolatingLoaderCannotSeeTheAppLoadersClass`, **+7
checks, 1193 → 1200**. Extended rather than replaced, per W7-82: that vector
already held both halves of the duplicate-**define** rule and, since W7-82, the
repeated-**lookup** rule; this adds the **visibility** rule.

Driven through a bare `java.net.URLClassLoader` **on purpose**, with the
subclass as the control in the same section. The URL is `java.io.tmpdir` — a
directory that certainly exists (so the loader has a genuine, usable URL search
path rather than the degenerate empty one, which takes a different road inside
`ucl_try_define_local_class`) and certainly does not contain the class. Nothing
is written.

**Anti-vacuity, three ways, all of them load-bearing:**

* the application loader is asserted to be **holding** `RLoaderChurnDefine$Sealed`
  *before* the isolating loader is asked for it. Ask for a name nothing has
  loaded and a leaky VM passes too;
* the same isolating instance is asserted to **still delegate to bootstrap**, so
  "isolated" cannot degenerate into "broken" and score a green;
* a bare `URLClassLoader` **parented to the application loader** is asserted to
  still find the class, and to answer with the *parent's* `Class` object — the
  obvious over-correction, caught by identity rather than by "did not throw".

`probes/ForNameCacheProbe.java`: group 6 extended (above), **group 8 added** —
the isolating shape end to end, including "the loader still sees its OWN defined
class" as the W7-82 regression guard, and the subclass running every check beside
the bare instance. Group 7 untouched.

Group 8 needs `--add-opens java.base/java.lang=ALL-UNNAMED`, because
`findLoadedClass` is `protected` and the *whole content of this bug* is that a
bare instance differs from a subclass — exposing the method through a subclass
would measure the arm that was never broken, so it is reached by reflection with
the receiver left bare. **A missing flag is reported as a FAILED check, never a
skip**; a silent skip is a vacuous green on exactly the check that has to be
able to fail. The flag was proved inert first: groups 0–7 return the identical
verdict with and without it, on both arms.

## Prove the RED

On a dev binary that **already carries W7-82** (so nothing below is W7-82's
symptom):

```
HotSpot 25.0.3   PROBE result=OK                     PASS RLoaderChurnDefine (1200 checks)
--jdk-only       PROBE result=FAILED(5)              AssertionError: new URLClassLoader(urls, null)
--real-jdk       PROBE result=FAILED(5), identical   .loadClass(RLoaderChurnDefine$Sealed) must raise
                                                     ClassNotFoundException (HotSpot does); got
                                                     class RLoaderChurnDefine$Sealed owned by
                                                     jdk.internal.loader.ClassLoaders$AppClassLoader
```

The five probe failures, identical on both arms:

```
CK group6 FAIL l3.findLoadedClass(ForNameCacheProbe) is null -- [got ForNameCacheProbe@app]
CK group6 FAIL l3.findLoadedClass(java.lang.String)  is null -- [got java.lang.String@<boot>]
CK group8 FAIL iso.findLoadedClass(ForNameCacheProbe) is null -- [got ForNameCacheProbe@app]
CK group8 FAIL loadClass of an app class through an isolating loader must raise CNFE, returned ...@app
CK group8 FAIL forName  of an app class through an isolating loader must raise CNFE, returned ...@app
```

Reverify after the build:

```
cargo build --release -p cratonvm-cli
cargo test -p cratonvm-native-builtins classloader_tests

javac -d regression-suite/build regression-suite/src/RLoaderChurnDefine.java
java                            -cp regression-suite/build RLoaderChurnDefine   # 1200, the oracle
target/release/cratonvm --real-jdk -cp regression-suite/build RLoaderChurnDefine  # 1200
target/release/cratonvm --jdk-only -cp regression-suite/build RLoaderChurnDefine  # 1200

javac -d probes/build-aux probes/ForNameCacheProbeTarget.java
javac -d probes/build     probes/ForNameCacheProbe.java
java --add-opens java.base/java.lang=ALL-UNNAMED -cp probes/build ForNameCacheProbe probes/build-aux
target/release/cratonvm --real-jdk --add-opens java.base/java.lang=ALL-UNNAMED \
    -cp probes/build ForNameCacheProbe probes/build-aux
target/release/cratonvm --jdk-only --add-opens java.base/java.lang=ALL-UNNAMED \
    -cp probes/build ForNameCacheProbe probes/build-aux
```

**A full suite run is required before merge.** This is a narrowing on the class
loading path; the sections below say what to watch, but they are predictions and
the suite is the measurement.

## Blast radius — this is a NARROWING, and it is on every framework's startup path

W7-82 was monotone: it could only turn a `None` into a `Some(the loader's own
class)`, so nothing that resolved could start resolving differently. **This one
is the opposite and must be read as such.** It turns a `Some(the application
loader's class)` into a `None` for one loader shape. Code that was relying on
the leak will start failing.

**In range.** Only objects whose runtime class is exactly
`java.net.URLClassLoader` — measured above: no VM-supplied loader qualifies, and
every subclass (Spring Boot's `LaunchedURLClassLoader` and
`ModifiedClassPathClassLoader`, Tomcat's `WebappClassLoader`, JBoss module
loaders, Groovy, ByteBuddy, every `MLet`) was already on the user-defined branch
and is bit-for-bit unaffected. What is in range is explicit application code:
`new URLClassLoader(...)` and `URLClassLoader.newInstance(...)`, both common in
test harnesses, plugin containers, `javax.tools` drivers, and isolation fixtures.

**What breaks, honestly.** A bare `URLClassLoader` whose URL set does **not**
contain a class, and whose parent chain cannot reach it either, used to get the
application loader's copy and now gets `ClassNotFoundException`. Two shapes:

* `new URLClassLoader(urls, null)` — the bootstrap-parented isolating loader.
  This is the deliberate case, and every such failure is a **correct** failure
  that HotSpot also produces. If a workload regresses here, the workload was
  depending on CratonVM's leak, and its HotSpot run would already disagree.
* `new URLClassLoader(urls, someParent)` where the class is on neither. Also
  correct, and also what HotSpot does — but this is the shape where a latent
  wrong classpath in a fixture will surface as a new failure rather than
  silently working.

**What does not change**, each checked rather than assumed:

* parent-first delegation through a bare `URLClassLoader` — the miss falls
  through to real delegation, which is the path the user-defined branch has
  always used, and the `URLClassLoader` **subclass** with a parent was measured
  resolving correctly through it on both arms *before* this change. Group 6's
  `loadClass` assertion and the vector's fourth arm pin it;
* bootstrap delegation — `is_bootstrap_class_name` diverts JDK names before the
  isolating branch is ever reached; measured, and pinned in both group 8 and the
  vector;
* the loader's own defined classes — the user-defined branch's steps 1 and 2
  *are* W7-82's probes; group 8 and `ForNameCacheProbe` groups 1–3 pin it;
* generated-proxy visibility — explicitly kept on the built-in branch, above;
* the app / platform / bootstrap loaders — unchanged branch, unchanged code;
* `defineClass` — a different road entirely, and one of the asymmetric sites
  above, untouched here.

**Cost.** A bare `URLClassLoader` that repeatedly asks about classes it did not
define now reaches `class_defined_by_this_loader_object`, a linear scan of the
defining-loader store under a global mutex, and then returns `None` instead of a
global hash lookup. That is not a new cost *shape* — every user-defined loader in
the VM already pays exactly this on the same kind of miss — but a bare
`URLClassLoader` in a pure-delegation role now joins them, and unlike W7-82's
case it pays the scan and gets nothing back. If a startup profile moves, this is
the line to look at; the honest fix would be an initiating-loader record, which
is the residual below.

**Widening, in the other direction.** Code written around a `catch
(ClassNotFoundException)` fallback for an isolating loader was previously never
taking that fallback and will now take it. That is the intended behaviour and it
is what HotSpot does, but it means fallback code that has never executed in this
VM is about to execute for the first time.

## Named residuals, measured and deliberately not fixed here

1. **CratonVM has no initiating-loader record.** `findLoadedClass` on **any**
   user-defined loader under-reports a class the loader initiated but did not
   define (measured above: HotSpot returns the class, CratonVM returns `null`).
   Benign in the sense that matters — it costs a redundant delegation, never a
   wrong answer — and pre-existing, shared by every user loader, and untouched
   by this change. Closing it means giving `ClassLoaderData` an initiated set
   and writing to it from `Class.forName` and constant-pool resolution: a new
   data structure on the hottest reflective path, wanting its own lane.
2. **The 17 asymmetric sites above**, of which the three `defineClassN`
   namespace picks are the only ones that can raise a hard error today, and
   raise precisely the `IncompatibleClassChangeError: already defined by
   application loader` that `is_bare_url_class_loader` exists to prevent. Not
   speculative — read at each site, with the divergent namespace named.

## Instrument notes

Two things nearly manufactured a wrong result, and both are worth carrying.

**The shared binary moved under the measurement.**
`/c/craton/CratonVM/target/release/cratonvm.exe` is whatever the last session
built. It was rebuilt *between two of these runs* (mtime 06:47 → 06:54, main
worktree HEAD `d40417496` → `aaed3f2ef`), and the two builds straddled the W7-82
merge — so `ForNameCacheProbe` returned `FAILED(10)` and then `OK` for the same
command with no change to anything under my control. Every measurement in this
record was retaken against a **copy of the binary pinned into the session
scratchpad**, and the pinned copy is post-W7-82, which is what makes the
`FAILED(5)` above this record's RED rather than the previous one's.

**The added flag was cleared before it was used.** `--add-opens
java.base/java.lang=ALL-UNNAMED` is new to this probe's invocation, so it is a
variable of the comparison until proved otherwise. Proved: the unmodified probe
returns `PROBE result=OK` with and without it, on both arms, on the pinned
binary. Had that not been checked, the flag would have been the obvious — and
wrong — explanation for the `FAILED(10)` → `OK` swing the rebuild actually
caused.

---

## Re-verified against the working tree, 2026-08-12 (lane A2, P1-B/P3-C)

Read, not rebuilt. Nothing here was built or run; these are source facts.

**The fix is present and unmodified.** `native-builtins/src/classloader.rs:2706`:

```rust
let takes_builtin_branch = !is_user_defined
    && (is_generated_proxy_name(internal_name) || !is_bare_url_class_loader(ctx, this));
```

with the W7-82/W7-87 rationale at `classloader.rs:2649-2705`. The allocator's two
matching guards are still spelled the same way, at `classloader.rs:2401`
(`loader_namespace_id_at`) and `:2503` (`peek_loader_namespace_id`), so the three
sites still agree. `is_bare_url_class_loader` is `classloader.rs:2275`. **Status
FIXED stands.**

**The sharpest ASYMMETRIC row is still open, and its line numbers have moved.**
The three `defineClassN` namespace picks are today at
`native-builtins/src/lang_system.rs:5323` (`defineClass1`), `:5418`
(`defineClass2`) and `:5513` (`defineClass0`). All three still read

```rust
Some(Value::Object(Some(loader_obj)))
    if crate::classloader::is_user_defined_loader(ctx, *loader_obj) && (…) => …
_ => 0,
```

with no `|| is_bare_url_class_loader(..)` disjunct, so a bare `URLClassLoader`
still defines into namespace 0 while `ucl_try_define_local_class` gives the same
object ns ≥ 3. `lang_system.rs`'s other row, `preload_supertypes_via_loader`
(`lang_system.rs:4984-4986`), also still returns early for a bare instance.

**A blocker the record did not name.** `is_bare_url_class_loader` is a private
`fn` in `classloader.rs` — not `pub(crate)` — so the `lang_system.rs` sites
**cannot call it at all** as the tree stands. Closing that row is therefore a
two-file change (a visibility widening in `classloader.rs` plus the three
predicates), which is one more reason it belongs in the dedicated lane §7 asks
for rather than being folded into an adjacent fix. Lane A2 owned
`lang_system.rs` and deliberately did **not** touch these three sites: a
narrowing on the class-defining path, in a nine-lane parallel campaign, with no
build and no oracle available, is exactly the shape this record warns about.

**Unrelated edits landed in `lang_system.rs` in the same pass** (`System.getenv`'s
wrapper, and typed linkage errors out of `defineClass0/1/2`). Neither touches
loader-namespace selection. The `defineClassN` failure tail now calls
`define_class_linkage_error` instead of `define_class_format_error`, which
changes the *type* of the exception a duplicate-namespace collision surfaces as:
the `IncompatibleClassChangeError: already defined by application loader` this
record predicts will now arrive as a real `java.lang.IncompatibleClassChangeError`
rather than as a `ClassFormatError` wrapping its `Debug` text. That makes the
predicted failure **easier** to recognise, not harder, and does not change
whether it happens.
