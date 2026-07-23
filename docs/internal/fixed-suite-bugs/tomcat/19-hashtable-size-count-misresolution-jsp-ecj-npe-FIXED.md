# Hashtable size/count field misresolution corrupts modCount, breaks JSP compilation (FIXED)

**Status:** FIXED on branch `fix/tomcat-untriaged-oddities-20260723` (merged
to `dev`). Closes both classes tracked in
`docs/known-issues/tomcat/untriaged-oddities.md` (see that doc's disposition
note) and section G/H of
[18-fixture-environment-gaps-20260724.md](18-fixture-environment-gaps-20260724.md).
**Test:** `org.apache.catalina.startup.TestTomcat` — now **26/26 PASS**
clean under CratonVM (was 3 failures / occasional HANG depending on host
load). Matches HotSpot.

## Symptom

`org.apache.catalina.startup.TestTomcat`'s `testJsps` (and, transitively,
`testSingleWebapp`/`testGetResource`, anything that compiles a JSP through
Jasper) failed with:

```
org.apache.jasper.JasperException: Unable to compile class for JSP
Caused by: java.lang.NullPointerException: Cannot assign field
  "referenceBinding" because "classFile" is null
    at org.eclipse.jdt.internal.compiler.ast.CompilationUnitDeclaration.cleanUp
    at org.eclipse.jdt.internal.compiler.Compiler.processCompiledUnits
    at org.eclipse.jdt.internal.compiler.Compiler.compile
    at org.apache.jasper.compiler.JDTCompiler.generateClass
```

The class-level failure log also always showed `LifecycleException:
Deliberately Broken` / `Deliberately Broken` (from `testBrokenWarOne`/
`testBrokenWarTwo`, which deliberately trigger and catch that exact
exception — confirmed by reading `TestTomcat.java`; those two tests always
pass). That text is a complete red herring and was never the actual failure
cause — the doc name ("Deliberately Broken") this bug was originally filed
under is misleading for exactly that reason.

Reproduces standalone (bypassing the 26-test suite, ~3-10s vs ~4min) via
Jasper's `JspC` CLI on **any** JSP, even a fully static one with no EL:

```sh
java -cp "$CP" org.apache.jasper.JspC -webapp <dir> -d <out> -uriroot <dir> \
  -p testpkg -compile <any>.jsp
```

## Investigation

1. Confirmed byte-identical generated `<jsp>_jsp.java` between CratonVM and
   HotSpot (Jasper's JSP→Java generation is unaffected) — the divergence is
   purely in ECJ's (Eclipse's embedded batch Java compiler, which Jasper uses
   to compile the generated servlet source) own execution.
2. Reproduces with `--nojit` — rules out a JIT-only miscompilation.
3. Reproduces with `--Xmx 8g` and zero GC events logged (`--verbose:gc`) —
   rules out GC-move/identity-hash instability (the class of bug fixed for
   `identity_hash_code()` lazy-minting, 2026-07-21).
4. Decompiled (CFR) `org.eclipse.jdt.internal.compiler.CompilationResult`
   (from `ecj-4.40.jar`): its `compiledTypes` field is a
   `new Hashtable(11)` keyed by `char[]` type names, populated by
   `record(char[], ClassFile)` (always non-null — dereferences `classFile`
   immediately, so a null wouldn't reach `compiledTypes` at all) and read
   back by `getClassFiles()` (`compiledTypes.size()` pre-sizes an array,
   then `compiledTypes.values().toArray(array)` fills it — the standard
   `AbstractCollection.toArray(T[])` contract null-pads any *trailing*
   slots if the live collection turns out smaller than `size()` reported).
5. Minimal standalone repro (no Tomcat/ECJ at all) nailed it down to
   `java.util.Hashtable` itself: a single `Hashtable.put(k, v)` call
   left `size()` reporting **2**, not 1 (confirmed with String, `Object`,
   and `char[]` keys — not specific to ECJ's key type). A 5-`put` loop
   left `size()` at **10**. Plain `HashMap` was unaffected (confirmed via
   a parallel probe) — this is Hashtable-specific.
6. Added a one-call-site debug counter
   (`CRATONVM_DBG_HMPUT_COUNT`, temporary, removed before merge) around the
   native `put` handler: it fires **exactly once** per Java-level
   `put()` call — ruling out a double-dispatch bug (e.g. the
   native-vs-real-JDK-bytecode dispatch-cache quirk documented for
   `invoke_virtual`). The corruption is inside the put/size bookkeeping
   itself, not the call count.
7. Field-level tracing (temporary) around every `Hashtable` size write
   showed the SAME numeric field slot getting incremented **twice** per
   `put()` — once by the (semantically intended) size bookkeeping, once by
   `bump_map_mod_count`'s legitimate `modCount` bump.

## Root cause

`java.util.Hashtable`'s `put`/`get`/`size`/... are unconditionally
force-native-dispatched (`vm/src/runtime/interpreter.rs`, mirrored in
`vm/src/vm/vm_exec.rs::invoke_on_class_shared_inner`), landing in
`native-collections/src/lib.rs`'s generic `native_map_put_evict` /
`map_state` / `set_map_size` — the same machinery `HashMap`/
`LinkedHashMap` use. Two of those helpers resolved the size-tracking
field's slot index with:

```rust
ctx.resolve_field_index("java/util/HashMap", "size")
```

— **hardcoded to `HashMap`'s class metadata regardless of the actual
receiver's class.** `resolve_field_index(class_name, field_name)` resolves
a slot number *within the given class's own hierarchy*; for a
`java.util.Hashtable` receiver (which extends `Dictionary`, not
`AbstractMap`, and has no field literally named `size` at all — its real
field is `count`) this returned a slot number that is only meaningful for
`HashMap` objects, then blindly applied it to the `Hashtable` object's
own (differently laid out) fields.

By coincidence of both classes' field layouts, that borrowed slot number
landed exactly on `Hashtable`'s real `modCount` field — so every
`put()` incremented `modCount`'s backing slot **twice**: once correctly,
via `bump_map_mod_count` (which resolves "modCount" correctly, against
the receiver's own class, via `get_field_by_name`/`set_field_by_name`),
and once more via the misdirected "size" write. `map_state`'s read side
has the same bug, so `Hashtable.size()` faithfully reported back whatever
this corrupted counter held — exactly double the true entry count.

Once inflated past `(cap * 3) / 4`, the phantom "size" also
prematurely triggered `map_resize`, which then further corrupted other
Hashtable-specific fields (observed: `threshold` clobbered to a garbage
value) using the same class-blind field-slot assumptions.

For ECJ's `compiledTypes` Hashtable, this meant `compiledTypes.size()`
over-reported the live entry count by 2x relative to what
`values().toArray()`'s iterator actually produced, which (per the standard
JDK `AbstractCollection.toArray(T[])` contract) null-pads the trailing
slots of an over-sized destination array — exactly the null `ClassFile`
that crashed `CompilationUnitDeclaration.cleanUp()`.

`HashMap`/`LinkedHashMap` receivers were never affected — for those, the
hardcoded `"java/util/HashMap"` class name IS the receiver's actual class
(or an ancestor sharing the same `size` field), so the lookup was already
correct by construction.

## Fix

`native-collections/src/lib.rs` — `map_state` and `set_map_size` now
resolve the size-tracking field against the **receiver's own class
hierarchy** (`resolve_field_index_by_class_id(this_cid, ...)`, using the
already-known `ClassId` rather than a name round-trip), trying `"size"`
first (HashMap/LinkedHashMap) and falling back to `"count"`
(Hashtable/Properties' real field name) when the receiver has no `size`
field at all:

```rust
let this_cid = ctx.class_id_of_object(this);
let size_by_name = ctx
    .resolve_field_index_by_class_id(this_cid, "size")
    .or_else(|| ctx.resolve_field_index_by_class_id(this_cid, "count"))
    .filter(|&slot| slot < ctx.object_num_fields(this))
    .map(|slot| ctx.get_field(this, slot));
```

(and the mirrored write side in `set_map_size`). This is the same
name-resolved-against-the-real-receiver-class pattern
`ensure_hashtable_load_factor` already used correctly for `loadFactor`/
`threshold`, and that bug #13 in this directory
(`13-hashtable-clone-cce-jndirealm-FIXED.md`) used for the `Properties`
`defaults` field — this is the third `Hashtable`-layout-vs-`HashMap`
-layout mismatch found in this area, all with the same shape (a helper
written against `HashMap`'s layout gets reused for `Hashtable` without
checking the receiver's actual class).

**Not touched (lower-risk, no observed symptom):** `native_map_init`,
`native_map_init_capacity`, and `try_set_jdk_map_field` have the same
hardcoded-`"java/util/HashMap"` pattern for `table`/`modCount`/
`threshold`/`loadFactor`, but empirically these did not corrupt a
`Hashtable` receiver in this investigation (both classes happen to declare
their bucket-array field first, and `ensure_hashtable_load_factor`
overwrites `loadFactor`/`threshold` correctly on first `put`,
masking any `<init>`-time mis-write). Worth a follow-up audit if a similar
symptom resurfaces elsewhere, but out of scope here.

## Validation

* Standalone `Hashtable<K,V>.put()`/`.size()` probes (String, `Object`,
  `char[]` keys; single put and a 5-put loop) — now match HotSpot exactly
  (size=1 / size=5, zero `null`s from `values().toArray()`).
* `org.apache.jasper.JspC ... -compile` on both a trivial static JSP and
  `webapps/examples/jsp/jsp2/el/basic-arithmetic.jsp` — `Generation
  completed with [0] errors`, real `.class` produced (was: ECJ NPE, no
  class file).
* `org.apache.catalina.startup.TestTomcat` — 26/26 PASS (was: 3 failures
  under light host load, or a 300s HANG under heavier load — this host's
  contention alone can turn a 5s test into 200s+; that contention was real
  but was masking this separate, genuine, deterministic bug underneath).
* `cargo test -p cratonvm-native-collections` (138 tests across 9 binaries)
  — all green, no regression from the field-resolution change.

## `org.apache.jasper.compiler.TestNonstandardTagPerformance` (the other class in the source doc)

Unrelated, not a CratonVM bug: `.suite/all-tests.txt` (fixture-local, not
git-tracked, per this folder's `README.md`) has a typo at line 452 —
`org.apache.jasper.compiler.TestNonstandardTagPerformance` — but the real
class is `TesterNonstandardTagPerformance` (note the "er"). Its own
source comment explains the `Tester` prefix is deliberate: "This test
requires additional setup and cannot be run as part of a standard test run
so it is excluded due to the name starting Tester..." — it's a
100,000,000-iteration manual EL-arithmetic benchmark, not a functional test.
Confirmed this is the *only* bogus entry in the fixture's 646-class list
(every other line has a matching compiled `.class`). Fixed directly on the
Azure host's fixture (`/data/data/tomcat-dohead-fixture-20260717/.suite/all-tests.txt`,
line removed) — no git action needed, nothing to fix in the repo itself.
