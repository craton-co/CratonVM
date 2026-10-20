# i133-L2: `TypeVariable.getAnnotations()` interpreted is 16-19x HotSpot `-Xint`

Interpreter round i6, wave 133, lane L2. Status: **OPEN (performance; cause not yet located).**
No switch built: reading the path did not find one dominant VM-side cost, and the fix depends
on which row of the decomposition probe below stands out. Both modes are affected: the JDK's
own `TypeVariableImpl` body runs under `--jdk-only`, and under `--compatible` too (through
`type_variable_impl_body`, `native-builtins/src/lang_reflect.rs`).

## Symptom

`tools/probes/interp/L1/L1W125TypeVariableImplFamily.java` under `cratonvm --nojit` prints the
right output but takes 640 s. Its `TIME getAnnotations` row (`gs[2].getAnnotations()`, 3 reps x
20 000 calls) reads about **8,000,000 ns/op** (orchestrator, w131).

HotSpot 25.0.3 on this host (Windows, one process each, measured by this lane):

| arm | `TIME getAnnotations` ns/op | whole probe |
|---|---:|---:|
| `-Xint` | 499 867 / 485 615 / 511 599 | 32.5 s |
| default flags | 22 155 / 15 503 / 15 945 | 1.7 s |

So `--nojit` is about **16x** HotSpot `-Xint` on this row. The interpreter's usual band against
`-Xint` on real code is about 6-8x
(`docs/internal/performance/h2-update-path-throughput-RETIRED-20260821.md`: "a per-statement
ratio that stands out from this band is a real lead"). This row stands out by 2-3x.

## What the call does (JDK 25, no memo anywhere)

`TypeVariableImpl.getAnnotations` -> `getDeclaredAnnotations` -> `typeVarIndex` and
`TypeAnnotationParser.parseTypeVariableAnnotations` -> `parseAllTypeAnnotations`:

* `Class.getRawTypeAnnotations` and `Class.getConstantPool` (natives);
* a `ByteBuffer` walk over **every** class-level type annotation of the declaring class, not
  only this variable's;
* per annotation, `AnnotationParser.parseAnnotation2`:
  * `parseSig`: `SignatureParser` + `Reifier` + `CoreReflectionFactory.makeNamedType`, which is
    `Class.forName(name, false, declaringLoader)`;
  * `AnnotationType.getInstance`, member values through `ConstantPool.getUTF8At0` etc.;
  * `annotationForMap` -> `Proxy.newProxyInstance`;
* `ArrayList.toArray(new Annotation[0])`.

HotSpot re-parses on every call too. The JDK body caches nothing, and every call answers
**fresh** proxies, nested ones included. The new probe prints
`fresh top=true nested=true equal=true nestedEqual=true`.

**A VM-side memo of parsed annotations is therefore not a fix.** Handing back the same proxies,
or the same nested `Rep` proxies inside a fresh container, changes `==` answers that HotSpot
gives.

HotSpot `-Xint` profile of the loop (this lane's Java-level sampler, the probe's `sample` mode,
3000 samples, inclusive share):

| frame | share |
|---|---:|
| `AnnotationParser.parseAnnotation2` | 90% |
| `AnnotationParser.parseSig` | 38% |
| `AnnotationParser.parseArray` (the container's `Rep[]`) | 35% |
| `SignatureParser.parseTypeSig` | 23% |
| `AnnotationParser.annotationForMap` | 19% |
| `Proxy.newProxyInstance` | 14% |
| `HeapByteBuffer.getShort` | 10% |
| `Proxy.getProxyConstructor` | 7% |
| `Constructor.newInstance` | 6% |

The self time is spread thin: `String.coder` 4%, `Buffer.nextGetIndex` 3%,
`ConstantPool.getUTF8At0` 3%, `Class.forName0` 2%. It is ordinary interpreted parsing.

## Where CratonVM does extra work (by reading; each needs its row's ratio to confirm)

1. **`Class.forName(n, false, appLoader)` runs the loader's Java `loadClass` on every call.**
   * `native_class_for_name` (`native-builtins/src/lang_class.rs`) answers from the loader's
     dictionary (`loader_dictionary_mirror`) only for a **user-defined** loader.
   * For the application and platform loaders it goes to
     `invoke_load_class_as_the_vm_publishing` (`classloader_real.rs`), which is
     `invoke_virtual(loader, "loadClass")`. That is `BuiltinClassLoader.loadClassOrNull`,
     `findLoadedClass`, `checkName`, and for a JDK name the module lookup and the boot-loader
     delegation.
   * HotSpot's `SystemDictionary` finds the class in the loader's dictionary and runs no Java.
     That is why `forName0` is 2% self above.
   * The probe's call makes 4-5 such calls (`Tp`, `Reps`, `Rep` x2, `Tu`).
   * Candidate fix, behind a performance switch: answer a built-in loader's own definition
     (`find_loaded_class_for_loader`) before the call, as the user-loader branch does. A built-in
     loader's `loadClass` has no Java-visible effect, so skipping it is exact.
   * Rows: `forNameApp`, `forNameJdk`.
2. **`Proxy.newProxyInstance` validates the interfaces on every call.**
   * `native_proxy_new_instance` (`native-builtins/src/reflect_annotations.rs`) runs
     `proxy_validate_interfaces` every time. That includes the non-public-interface refusals
     (the probe's annotation types are package-private: a class-loader query and module
     questions per interface).
   * The JDK validates only in `ProxyBuilder`'s constructor, on a proxy-class cache miss.
   * Candidate fix: memo the verdict per (loader namespace, interface ids).
   * Row: `proxy`.
3. **The `ByteBuffer` reads.** `HeapByteBuffer.getShort` goes through `ScopedMemoryAccess` and
   `Unsafe.getShortUnaligned`, several interpreted calls deep. Row: `byteBuffer 3 reads`.
4. **Plain interpreted parsing and calls** (`SignatureParser`, `HashMap`, `LinkedHashMap`,
   `String`). This is the general invoke gap
   (`docs/internal/performance/interpreted-invoke-cost-350ns-RETIRED-20260911.md`), not a
   defect of this path. Rows: `getDeclaredField+genericType` (a pure-Java signature parse),
   `typeParams`, `getName`, `list toArray`.

## The probe

`tools/probes/interp/L2/L2W133GetAnnotationsCost.java`. Its HotSpot block matches default flags
and `-Xint` (SAME, checked). HotSpot `-Xint` TIME rows on this host, ns/op:

| row | `-Xint` | what it isolates |
|---|---:|---|
| `full` | 420 000-550 000 | the whole call (2 annotations on `V`, 3 parsed per call plus 2 nested) |
| `typeParams` | 330-610 | `Class.getTypeParameters` (the `typeVarIndex` step) |
| `forNameApp` | 850 | `Class.forName(app class, false, appLoader)` |
| `forNameJdk` | 590 | `Class.forName("java.util.Map", false, appLoader)` |
| `proxy` | 4 800-4 900 | `Proxy.newProxyInstance` with a package-private interface, cached class |
| `annProxyCall` | 600 | `tp.value()` through the annotation proxy |
| `getDeclaredMethod` | 1 430 | reference |
| `getDeclaredMethod+annotations` | 3 050-3 340 | member annotations (cached on the root `Method`) |
| `getDeclaredField` | 920-940 | reference |
| `getDeclaredField+genericType` | 4 170-4 200 | a fresh `Field`'s signature parse (`SignatureParser`, `Reifier`, `forName` of `Map`) |
| `byteBuffer 3 reads` | 1 980-2 210 | `getShort`, `get`, `getInt` on a heap buffer |
| `list toArray` | 1 400-1 460 | `new ArrayList` x3 `add`, `toArray(new Annotation[0])` |
| `getName` | 110 | the cheapest call, the band's floor |

## What to run (orchestrator)

```text
J="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin"
$J/javac -d out tools/probes/interp/L2/L2W133GetAnnotationsCost.java
$J/java -Xint -cp out L2W133GetAnnotationsCost > hs.txt
$B --jdk-only   --nojit -cp out L2W133GetAnnotationsCost > jdk.txt
$B --compatible --nojit -cp out L2W133GetAnnotationsCost > compat.txt
$B --jdk-only   --nojit -cp out L2W133GetAnnotationsCost sample 1500 > sample.txt
CRATONVM_DBG_INTERP_FRAMES=1 $B --jdk-only --nojit -cp out L2W133GetAnnotationsCost full 2> frames.txt
```

Pass for the parity part: the non-TIME lines of `jdk.txt` and `compat.txt` equal the
HOTSPOT_EXPECTED block. `sample` prints only `full annotations=...` and `sink>0=true` outside
TIME lines; `full` prints those two lines plus the `fresh` line.

How to read the result: divide each cratonvm row by its `-Xint` row.

* The rows that sit at the `getName` / `typeParams` / `genericType` ratio are the general gap.
* A row 2x or more above that ratio names the fix:
  * `forNameApp` / `forNameJdk`: item 1;
  * `proxy`: item 2;
  * `byteBuffer`: item 3.
* If none stands out, `full` is the general gap times a call-heavy path, and this page should go
  to the invoke-cost work rather than get its own switch.

The `sample` run answers the same question directly. Compare its `TIME sample incl` table with
the HotSpot one above: a frame whose share is much larger under cratonvm is the cost. The frames
census (`frames.txt`, one row per method per interpreted frame push; divide by 4000 for frames
per call) shows Java that runs only under cratonvm. Expect `BuiltinClassLoader.loadClassOrNull`
and `findLoadedClass` rows if item 1 holds; HotSpot runs no such frames.

## Run (orchestrator, w131 `07ed7469ce`, Windows, P-cores, `--jdk-only --nojit`, 2026-10-19)

The non-TIME output matches HotSpot. Each row's ns/op, and its ratio to HotSpot `-Xint` (the lane's table):

| row | cratonvm ns/op | ratio to HotSpot `-Xint` |
|---|---|---|
| full | 6.5M-8.7M | 13-18x |
| typeParams | 14,865 | about 32x |
| getName | 3,633 | 33x |
| getDeclaredField+genericType | 69,544 | 17x |
| byteBuffer 3 reads | 50,702 | 24x |
| list toArray | 47,849 | 34x |
| proxy | 162,010 | 34x |
| forNameApp | 38,078 | 45x |
| **forNameJdk** | **88,729** | **150x** |

Only `forNameJdk` is more than 2x the baseline rows' ratio (getName and typeParams, 32-33x). So the decision rule names suspect 1: `Class.forName` of a JDK name on the application loader runs the built-in loader's Java `loadClass` every call. Next step: answer the built-in loader's own (or its parent's) definition before calling `loadClass`, behind a default-OFF performance switch (loader lane).

## Wave 133 (lane L5d), 2026-10-19: suspect 1 built, SWITCH-OFF (unbuilt, A/B pending)

New performance switch **`CRATONVM_BUILTIN_FOR_NAME_RECORD`** (default OFF, `=1` on;
`LoaderFlags::builtin_for_name_record`, token `builtin-for-name-record`).

What it does (`native-builtins/src/lang_class.rs`, `native_class_for_name`):

* After a `Class.forName(name, init, loader)` whose loader is exactly
  `jdk.internal.loader.ClassLoaders$AppClassLoader` or `$PlatformClassLoader`, and whose
  `loadClass` answered a class of the requested name, the VM files that answer per loader and name
  (`BuiltinForNameRecord`, a per-VM row set in the class-loader natives' caches, `ArchiveBytesCaches`
  in `classloader.rs`). This is the row HotSpot's `SystemDictionary` files in the loader's
  dictionary after `load_instance_class`: the loader is then an initiating loader of the class.
* A later `forName` of the same name through the same loader answers from that row. It does not run
  `BuiltinClassLoader.loadClass`. The initialize flag and the `Instrumentation.getInitiatedClasses`
  note are handled as after `loadClass`.
* A row is read back only while `class_id_by_name(name)` still names that class and it is not
  hidden. A replaced class (a `--compatible` stand-in upgraded, a tombstone) and an ambiguous name
  are asked again.

What stays the same:

* Misses are never filed, so a missing class runs `loadClass` every time, and its
  `ClassNotFoundException` and message are the loader's own (HotSpot keeps no negative rows).
* Arrays and names with `/` are not filed.
* A `-Djava.system.class.loader` custom loader, user loaders, bare `URLClassLoader`s and the boot
  loader (`null`, which already runs no Java) are untouched.
* An agent that redefines `loadClass` sees the same calls as on HotSpot: the first `forName` per
  (loader, name) runs it, later ones do not.

One difference from HotSpot is left: a name the loader initiated through the resolution door, or
defined, and was never asked a `forName` for, still runs `loadClass` once on its first `forName`.
Only an instrumented `BuiltinClassLoader` can see that.

Unit tests: `lang_class::i133_l5d_builtin_for_name_record_tests` (2 tests). Correctness probe:
`tools/probes/interp/L5d/L5dW133BuiltinForNameRecord.java`. Its HotSpot block matches default flags
and `-Xint` (SAME, checked). It covers repeat identity, initialize false then true, a lazily named
class, JDK and platform-module names through both loaders, the one-arg overload, arrays, a missing
name (3 times), an application class through the platform loader, and a failing `<clinit>` (EIIE,
then NCDFE).

A/B (orchestrator, P-cores; median of the reps, interleaving the arms):

```text
$J/javac -d out tools/probes/interp/L2/L2W133GetAnnotationsCost.java tools/probes/interp/L5d/L5dW133BuiltinForNameRecord.java
for m in --jdk-only --compatible; do for j in --nojit ""; do
  start /affinity FFFF $B $m $j -cp out L2W133GetAnnotationsCost
  CRATONVM_BUILTIN_FOR_NAME_RECORD=1 start /affinity FFFF $B $m $j -cp out L2W133GetAnnotationsCost
  $B $m $j -cp out L5dW133BuiltinForNameRecord
  CRATONVM_BUILTIN_FOR_NAME_RECORD=1 $B $m $j -cp out L5dW133BuiltinForNameRecord
done; done
```

Pass criteria:

* **Parity:** the non-TIME lines of every `L5dW133BuiltinForNameRecord` run, and of every
  `L2W133GetAnnotationsCost` run, equal their HOTSPOT_EXPECTED blocks, in both modes and both arms.
* **Performance:** with the switch on, the `L2W133` `forNameJdk` and `forNameApp` rows under
  `--nojit` drop toward the `getName` band (target: at most 2x the baseline ratio, about 66x HotSpot
  `-Xint`; today 150x and 45x). The `full` row should drop too (4-5 `forName` calls per
  `getAnnotations`). `getName` and `typeParams` stay flat. In the JIT arm, nothing may regress.
  The same holds for `L5dW133`'s TIME rows.
