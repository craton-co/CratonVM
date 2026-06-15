# spring-bug-03: `AbstractMethodError … has no Code attribute` on java.util.stream interface methods

| | |
|---|---|
| **Category** | VM-CORRECTNESS (interface/itable dispatch) — likely high impact |
| **Module** | spring-core |
| **CratonVM** | FAIL — `AbstractMethodError` invoking a concrete stream op |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | **FIXED on dev** — commit `4c5a4d09` (merge `d75f7716`); verified `BindingReflectionHintsRegistrarTests` FAIL→OK |
| **Suggested owner** | me (done; LongStream/DoubleStream/Collector.supplier analogs still TODO) |

## Symptom
```
java.lang.AbstractMethodError: method java/util/stream/IntStream.findFirst()Ljava/util/OptionalInt;
  has no Code attribute
java.lang.AbstractMethodError: method java/util/stream/Collector.supplier()Ljava/util/function/Supplier;
  has no Code attribute      (wrapped in CompletionException)
```
Calling a stream terminal/collector op dispatches to the **abstract interface method**
(`IntStream.findFirst`, `Collector.supplier`) which has no body, instead of the concrete
override in the pipeline/collector implementation — i.e. `invokeinterface` resolves to the
abstract declaration rather than the runtime type's implementation.

## Affected test classes (confirmed CV-unique, HotSpot OK)
```
aot.hint.BindingReflectionHintsRegistrarTests   (IntStream.findFirst)
core.ReactiveAdapterRegistryTests               (Collector.supplier, via CompletableFuture)
```
(Any Spring code using these stream ops at runtime is affected — expect more downstream.)

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-core/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.aot.hint.BindingReflectionHintsRegistrarTests
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.aot.hint.BindingReflectionHintsRegistrarTests   # passes
```
Minimal trigger to confirm in isolation:
```java
OptionalInt x = java.util.stream.IntStream.of(1,2,3).findFirst();   // AbstractMethodError under CratonVM?
```

## ROOT CAUSE — CONFIRMED (and fix applied, pending build verification)
The receiver's runtime **class is wrong**. CratonVM intrinsifies `IntStream.range`/`rangeClosed`/
`filter`/`mapToInt` to return a *synthetic* stream object stamped with the **abstract interface
class `java/util/stream/IntStream`** itself:

| expression | CratonVM `getClass()` | HotSpot |
|---|---|---|
| `IntStream.of(...)` | `IntPipeline$Head` ✓ | `IntPipeline$Head` |
| `IntStream.range(...)` | **`IntStream`** ✗ | `IntPipeline$Head` |
| `...range(...).filter(...)` | **`IntStream`** ✗ | `IntPipeline$10` |
| `Stream.mapToInt(...)` | **`IntStream`** ✗ | `ReferencePipeline$4` |
| `Arrays.stream(int[])` | `IntPipeline$Head` ✓ | `IntPipeline$Head` |

The synthetic `IntStream` has natives for most ops (`sum`/`count`/`min`/`max`/`forEach`…) but **no
`findFirst`/`findAny` returning `OptionalInt`** — so the call resolved to the abstract interface
method (no Code) → `AbstractMethodError`. Confirmed **interpreter-level** (reproduces with
`--nojit`). Real `IntPipeline$Head` receivers (from `of`/`Arrays.stream`) run their own bytecode and
work. Minimal repro: `IntStream.range(0,5).findFirst()` (see `spring-suite/probe/StreamProbe2.java`).

**Fix applied** (`native-collections/src/lib.rs`): register `IntStream.findFirst/findAny
()Ljava/util/OptionalInt;` → `native_int_stream_find_first` (mirrors `min`/`max`, returns first
buffered element). Only consulted on the no-Code fallback path (synthetic receivers); real pipelines
unaffected. **TODO after verify:** apply the same to `LongStream→OptionalLong`,
`DoubleStream→OptionalDouble` synthetic streams (likely identical gap).

## (earlier) Suspected root cause — fix locus identified
This is a known, heavily-patched area: CratonVM's **"S111r10 interface-dispatch receiver-walk
fallback"** in `vm/src/runtime/interpreter.rs` (~2215–2360) and the AbstractMethodError sites in
`vm/src/vm/vm_exec.rs` (6337, 6361, 6572–6596, 7958). When an `invokeinterface` resolves to an
abstract interface declaration with no Code, the interpreter is *supposed* to walk the receiver's
runtime class for a concrete override (bytecode or native) before throwing — with special rescues
for lambda proxies, annotation proxies, and receiver-own-class natives. For `IntStream.findFirst`
the receiver is an `IntPipeline`/`IntPipeline.Head` that DOES carry a bytecode override, so the
receiver-walk (`find_method_recursive(recv_cid, "findFirst", …)`) should find it — yet it still
throws. Likely the AbstractMethodError is raised from a path that bypasses the receiver-walk
(e.g. JIT-compiled call site, or `vm_exec.rs` direct dispatch), or `find_method_recursive` fails
on the pipeline's class shape. **Needs reproduction-driven debugging** (build + trace which dispatch
path throws). Confirm first with the 1-line trigger above.

## Notes
Pin down with the minimal trigger above before fixing — if the 1-line snippet reproduces, it's a
clean VM-level dispatch bug independent of Spring.
