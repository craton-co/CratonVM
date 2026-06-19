# bug-06: the genuine CV correctness tail (assertion mismatches) — clusters into ~6 families

| | |
|---|---|
| **Category** | **VM-CORRECTNESS** (the real CV-unique FAIL residual, once env/cascade/GC-race noise is removed) |
| **Source** | 529 captured `AssertionFailedError`/`AssertionError` failcauses (the ~14% genuine-mismatch slice of the run) |
| **CratonVM HEAD** | `8e8e47d9` (suite run) |
| **Status** | OPEN — clustered; top family has a confirmed one-spot fix |

The 529 assertion mismatches are **not** hundreds of independent bugs — they collapse into ~6 root-cause
families. Ranked by fixability × blast radius:

## 1. Synthetic concurrent field-updaters missing methods ⭐ (top fix — confirmed)
```
NoSuchMethodError: java/util/concurrent/atomic/AtomicIntegerFieldUpdater$RustJvmImpl.getAndIncrement(Ljava/lang/Object;)I   ×18
NoSuchMethodError: java/util/concurrent/atomic/AtomicLongFieldUpdater$RustJvmImpl.addAndGet(Ljava/lang/Object;J)J          ×4
NoSuchMethodError: ...AtomicLongFieldUpdater$RustJvmImpl.getAndIncrement(...)                                              ×1
```
**Root cause (confirmed in `native-builtins/src/atomic_updater.rs`):** the synthetic `$RustJvmImpl`
registers `get/set/compareAndSet/getAndSet/getAndAdd/lazySet/incrementAndGet/decrementAndGet/
updateAndGet/weakCompareAndSet` — but **NOT `getAndIncrement`, `addAndGet`, `getAndDecrement`,
`accumulateAndGet`** (concrete methods on the abstract base that the synthetic subclass does not
inherit). **Reactor** uses `getAndIncrement`/`addAndGet` on field updaters for backpressure/state, so
this breaks reactive across **messaging + webflux** (the `expectComplete → onError(NoSuchMethodError)`
StepVerifier failures).
**Fix:** register the missing methods delegating to `getAndAdd` (mirror the already-present
`incrementAndGet`): `getAndIncrement(t) = getAndAdd(t, 1)`, `addAndGet(t, d) = getAndAdd(t, d) + d`,
`getAndDecrement(t) = getAndAdd(t, -1)`. ~5-line change, high reactive blast radius.

## 2. Interface "no Code attribute" dispatch (= spring-bug-03 family, recount)
```
java/net/http/HttpClient.executor()Ljava/util/Optional;                ×11
org/springframework/web/util/RfcUriParser$State.handleNext(...)        ×5
javax/xml/stream/XMLStreamReader.getNamespaceContext()                 ×4
java/util/function/Consumer.accept / Collector.supplier / Delayed.getDelay / FileSystemProvider.isSameFile / WritableByteChannel.write …
```
Interface / default-method / functional-interface invocations land on the **abstract** method ("has no
Code attribute"). The known itable/dispatch bug ([[spring-suite/bugs/spring-bug-03]]); `HttpClient.executor`
and the StAX `XMLStreamReader` instances are new high-count members. Affects http-client, oxm, uri parsing.

## 3. Type-filter / classpath-scanning metadata (ASM `core.type.classreading`)
`AnnotationTypeFilterTests`, `AspectJTypeFilterTests`, `AssignableTypeFilterTests`,
`DefaultAnnotationMetadataTests`, `SimpleAnnotationMetadataTests`, `ComponentScanParser…`,
`AnnotationMetadataAssemblerTests`. Symptom: `Class [...] should not have been loaded` — CratonVM's
ASM-based class-metadata reading / type-filter matching includes classes the filter should exclude.
A distinct metadata-reading correctness bug; underpins component scanning, so it cascades into
`@ComponentScan`-driven context tests.

## 4. Synthetic `AnonymousObject` missing methods
```
cratonvm/synthetic/AnonymousObject$4.clone()  ×4    AnonymousObject$3.end()  ×1
```
CratonVM's synthetic anonymous-class objects don't expose all methods the code calls (`clone`, `end`).
Synthetic-class-generation gap (cf. `MetadataNamingStrategyTests`).

## 5. Reflection / context returns `null` (mix of CV + cascade)
```
Cannot invoke getDeclaredMethod on null  ×28   currentContext on null ×15   add/get/size/length/write on null …
```
`getDeclaredMethod on null` (×28) = a reflection/class lookup returning `null` where HotSpot returns a
value (overlaps the generics/annotation families). `currentContext on null` (×15) = Reactor context
(downstream of family 1). **`getStandardFileManager`/`getSystemJavaCompiler` on null (×5+) is
ENVIRONMENTAL** — CratonVM has no in-process `javax.tools.JavaCompiler` (same as the H2/in-process-javac
gap), HotSpot-with-JDK has one; not a correctness bug.

## 6. Annotation subsystem (= spring-bug-01)
`core.annotation` (23) + `context.annotation` (20) assertion fails — synthesized-annotation / `@AliasFor`
/ meta-annotation value mismatches. The long-known annotation cluster.

---

## Fix status (this session)
- **Family 1 — field-updaters: FIXED ✓** (`fe52db3a`, verified vs HotSpot). Added
  `getAndIncrement`/`getAndDecrement`/`addAndGet` (int+long) to the synthetic updaters.
- **Family 2 (top) — `HttpClient.executor()`: FIXED** (staged; returns `Optional.empty()` like HotSpot).
- **`FileSystemProvider.isSameFile`: FIXED** (staged; synthetic provider now answers path-equality).
- **Family 2 GC-race members** (`TestEngine.getId`, enum `handleNext`, `Function`/`Consumer`) — folded
  into the **precise-JIT-maps GC fix already on dev**; verify they clear on the rerun.

### Moderate follow-ups (deterministic, but bigger than a stub registration)
- **Multi-dim array runtime class** — `new String[2][2].getClass()` → `[Ljava.lang.Object;` instead of
  `[[Ljava.lang.String;` (and `int[3][4]` → `[Ljava.lang.Object;`). `alloc_multi_array`
  (`vm/src/runtime/interpreter.rs`) builds reference dims with `new_ref_array(ClassId(0)=Object)`, so
  the array's component class is `Object`, not the real `T[]`. Fix = thread the leaf **class id** (not
  just `ArrayElementType`) through `alloc_multi_array` and create the proper `[[L<leaf>;` array class.
  Deterministic, broad (all multi-dim array reflection / `instanceof` / array-store checks).
- **`Collectors.toList().supplier()`** — the synthetic `java/util/stream/Collector`
  (`native-builtins/src/phases_late.rs:2944+`) doesn't implement `supplier()` → AbstractMethodError.
  Needs a synthetic `Supplier` (returns the accumulator container ctor), not a one-liner.
- **`ScheduledFuture.getDelay`** — NPE `Cannot invoke add on null` (scheduler queue), distinct from the
  "no Code attribute" family; a `ScheduledThreadPoolExecutor` modeling gap.

## Families 3–6 progress (2026-06-17, branch `fix/bug06-families-3-6`, worktree `CratonVM-bug06fam`)

### Family 4 — ✅ FIXED (`clone`/Object-method half) — commit `40b6d94a`
**Root cause (confirmed in code, not hypothesised):** `ensure_synthetic_class`
(`classloading/src/class_manager.rs`) built every synthetic stub with `superclass: None`.
A `cratonvm/synthetic/AnonymousObject$N` therefore had no link to `java/lang/Object`, so
dispatch walked an empty superclass chain and never reached the natives registered on
`java/lang/Object` → spurious `NoSuchMethodError: …AnonymousObject$4.clone()`. The
receiver-rescue at `vm_exec.rs:10074` doesn't help: it only fires when the *dispatch* class
is `java/lang/Object`, and `clone` is an `is_object_member`, so the rescue is skipped.
**Fix:** synthetic stubs now inherit `java/lang/Object` (Object itself + array stubs keep
`None`). Unblocks `clone`/`equals`/`hashCode`/`toString`/`getClass`/`wait`/`notify` on all
synthetic objects. Verified: 2 new unit tests + full classloading suite (488 pass, 0 regress).
**Not fixed:** `AnonymousObject$3.end()` (×1) — `end()` isn't an Object method; it depends on
which real anonymous class the native stood in for, so it is not generically fixable.

### Family 3 — ✅ FIXED & VERIFIED — commit `4b923e86` (merged to dev `1a7c2268`)
**Root cause (pinned with a reproducer, NOT the original hypothesis):** it was *not*
metadata-reading loading the class — it was `ClassLoader.findLoadedClass` itself. The
real-JDK-mode native (`native-builtins/src/classloader_real.rs`) called
`ctx.load_class(&internal)`, which **loads** any classpath-resolvable class as a side
effect and then reports it loaded. Spring's `assertClassNotLoaded` → reflective
`findLoadedClass` → CratonVM returned non-null for a scanned-but-not-loaded class →
"should not have been loaded". Per the JVM spec `findLoadedClass` must never trigger loading.
**Fix:** no-load lookup (`class_id_by_name`, the loaded-class set) — matches HotSpot and the
synthetic-mode `cl_find_loaded_class` handler. **Verified on a built VM (--java-home JDK25):**
- `FindLoaded` probe: `findLoadedClass("java.util.zip.CRC32C")` before use now returns `null`
  (was: the class) — byte-identical to HotSpot.
- `AnnotationTypeFilterTests` 0/6 → **6/6 PASS**; `AssignableTypeFilterTests` **4/4 PASS**;
  `AspectJTypeFilterTests` **8/8 PASS** — all three type-filter classes at HotSpot parity
  (the "should not have been loaded" family). No load-path regression (filter targets still load).
- Probes: `spring-suite/probe/FindLoaded.java`, `fam3-validate.sh`.
- The two heavier ASM-metadata classes (`Default/SimpleAnnotationMetadataTests`, 49 tests each,
  HS 49/49) did **not** complete inside a 320–560 s timeout on the *debug* VM — they exercise
  ASM `ClassReader` metadata decoding, not the `findLoadedClass` path the fix addresses; whether
  this is debug-build slowness or a separate metadata-reading issue is unresolved (re-check on a
  release build). The core family-3 "should not have been loaded" failure is fully resolved.

### Family 5 — ✅ PINNED as a CASCADE; one clean reflection-null FIXED (lambda `getGenericSuperclass`)
**Status (2026-06-18, branch `fix/bug06-fam5-refl-null`, worktree `CratonVM-fam5`):** the
`getDeclaredMethod on null` ×28 does **NOT** reduce to a single clean
"reflection-native-returns-null" bug. Established by **reproducer-driven diffing** (build VM,
diff probes vs HotSpot JDK 25 — the same method that settled families 3 & 5's siblings):

- **Refuted (again):** the `synthetic_class_mirror` slot-0 theory — `lang_class.rs:660-661`
  documents `Object(None)` in slot 0 is *intentional*; the mirror is always non-null. **Do
  not** touch slot 0 (breaks the `isArray` contract).
- **`Refl5`** (29 common-accessor cases): byte-identical to HotSpot (re-confirmed on the fixed VM).
- **`Refl6`** (`scratch/fam5/Refl6.java`, ≈240 cases over *every* `Class`/`Method`/`Field`/
  `RecordComponent`-returning accessor × a wide receiver zoo — nested/local/anon/enum-body/
  record/proxy/lambda/array/annotation): identical for **all normal classes**.
- **`BridgeProbe`** (`scratch/fam5/BridgeProbe.java`): drives `BridgeMethodResolver.findBridgedMethod`
  (the unguarded `searchForMatch` → `type.getDeclaredMethod`) + `ClassUtils.getAllInterfacesForClass`
  over generic hierarchies → identical (only `getDeclaredMethods` *ordering* differs). The
  "null interface element" hypothesis does **not** reproduce.
- **`SpringFam5`** (`scratch/fam5/SpringFam5.java`): drives Spring's **real** machinery —
  `ResolvableType` / `GenericTypeResolver` / `MethodParameter` / `MergedAnnotations` /
  `AnnotatedElementUtils` / `AnnotationUtils` / `BeanUtils` over generic + `@AliasFor`-meta +
  event-listener fixtures (mirrors the bug-05 generics + annotation test families). **Byte-identical
  to HotSpot** except the lambda *name* (below). So Spring's generics+annotation reflection is correct.

**Conclusion:** the ×28 is a **cascade** (matches this doc's own "(mix of CV + cascade)"
hedge) — downstream of bug-04 (GC mirror→Object/null), bug-05 (generics reification, partial
fix `d01345d1`), and the synthetic-type metadata gaps below — NOT a standalone reflection bug.

**The one clean, family-5-shaped NULL found & FIXED:** for lambda/method-ref proxies
(class id ≥ `0x8000_0000`, not in the class store), `Class.getGenericSuperclass()` returned
**`null`** where HotSpot returns **`Object`** — even though `getSuperclass()` already returns
`Object` for the same lambda (the SB-14 guard in `native_class_get_superclass`). Spring's
`ResolvableType` generic-hierarchy walk calls `getGenericSuperclass()` on functional-interface /
listener lambdas, and a null there propagates into a null resolved class →
`getDeclaredMethod on null`. **Fix:** mirror the SB-14 lambda guard in
`native_class_get_generic_superclass` (`native-builtins/src/lang_class.rs`) →
`lambda_functional_interface(id).is_some()` returns the `Object` mirror. Verified: `Refl6`
`lambda`/`mref` `genericSuperclass` `<NULL>`→`java.lang.Object`; normal classes, `Refl5`,
`BridgeProbe`, `SpringFam5` unchanged (no regression).

**Remaining synthetic-type reflection gaps (documented follow-ups, distinct bugs, NOT the
family-5 null):**
1. lambda/hidden-class `getName()`/`getCanonicalName()`/`getSimpleName()` = `unknown_<classid>`
   (HS `Foo$$Lambda/0x..`) + bogus `getNestHost()` + `isSynthetic()=false` — the lambda proxy
   id isn't registered in the class-name map (`alloc_lambda_proxy_id` → `lambda_proxies`, not
   the class manager). Bigger change (name synthesis from the lambda registry).
2. `getNestMembers0` returns only `[self]` instead of all nestmates.
3. JDK `Proxy` subclass leaks: `getSuperclass()`/`getGenericSuperclass()` = `Proxy$Instance`
   (HS `java.lang.reflect.Proxy`), `isSynthetic()=true` (HS false).

### Family 6 — open sub-bugs in Spring's synthesis layer (`spring-bug-01`)
Raw annotation reading is JVMS-conformant (sub-bugs #0/#1 already fixed). Residual mismatches
(#2 Repeatable accessor, #3 `@AliasFor` mirror sees empty `{}` defaults, #4 annotation-array
rank ClassCast) live in Spring's `MergedAnnotation`/`MirrorSets` synthesis; reproduce only in
the full Spring stack, ~1h in-context tracing each. **New datum:** `AnnotationUtilsTests`
(HS 72/72) does not just fail — it aborts with a fixed ~2.0 GB allocation (`memory allocation
of 2127788032 bytes failed`), reproduced **identically on the pre-family-3 binary**, so it is
a separate pre-existing annotation-synthesis runaway, not a findLoadedClass effect.

**Method note:** all four agent "root causes" were hypotheses; only family 4's survived
contact with the source. Families 3 and 5 were settled by **building the VM and diffing
reproducers vs HotSpot** — which *refuted* the family-3 metadata-loading hypothesis AND the
family-5 mirror hypothesis, and pinned family 3 to `findLoadedClass`. Lesson reaffirmed:
reproducer-driven bisection, not source-reading guesses.

**Tooling note:** the `VirtualQuery` clashing-extern warning (`vm/src/runtime/{crash_handler,
memwatch}.rs`) is **FIXED & merged to dev** (`18707ee3`) — memwatch's decl now matches
crash_handler's structurally. The layouts were already equal (implicit `repr(C)` pad == explicit
`_pad`), so the ~2 GB-OOM was a **full-disk incremental-build artifact**, not this clash (a
healthy build runs the probes clean). The **second** clash — `GetCurrentProcess`
(`-> *mut c_void` in `crash_handler.rs:324` vs `-> isize` in `jit/helpers.rs:4644`) — is also
now **unified** (`3be68ca5`, on `*mut c_void`), making the vm crate clash-free (verified:
`cargo build -p cratonvm-vm` = 0 `clashing_extern` occurrences). With both clashes gone,
`clashing_extern_declarations = "deny"` is now enabled in `[workspace.lints.rust]` so any future
divergent native re-declaration fails the build. **Fully verified:** a from-clean
`cargo build -p cratonvm-cli` (entire production binary tree) finishes clean under the deny
(3m04s, 0 clashing/deny errors).

## Takeaway
- The "344 + 181 = 525" genuine mismatches reduce to **~6 families**, three of which (1, 2, 4) are
  concrete CV synthetic/dispatch gaps with clear fixes; (3, 6) are two known correctness subsystems
  (metadata-reading, annotations); (5) is partly cascade/environmental.
- **Family 1 (field-updaters) is the best ROI** — a ~5-line `atomic_updater.rs` change that unblocks a
  swath of reactive (messaging/webflux) tests. Recommend doing it next.
- Re-running on **current dev** (with the just-landed `CRATONVM_PRECISE_JIT_MAPS` GC-race fix + my crash
  fixes) will further shrink families 5/6's load-dependent share.
