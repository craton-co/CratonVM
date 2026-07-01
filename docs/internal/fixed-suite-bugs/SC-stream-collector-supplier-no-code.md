# SC-stream-collector-supplier-no-code - archived fixed issue

> **STATUS: RESOLVED (archived).** The stale known-issues entry has been closed:
> CratonVM now registers native bodies for the synthetic `Collector` contract
> methods and the returned functional-interface SAM methods.

## Symptom

Spring's `ReactiveAdapterRegistryTests.toMulti()` could fail under real bytecode
with:

```text
AbstractMethodError: java/util/stream/Collector.supplier()Ljava/util/function/Supplier; has no Code attribute
```

The synthetic collector instances returned by `java.util.stream.Collectors`
implemented the fast-path collector shape internally, but real stream bytecode can
invoke the public `Collector` interface contract:

- `Collector.supplier()`
- `Collector.accumulator()`
- `Collector.finisher()`
- `Collector.combiner()`

The returned `Supplier`, `BiConsumer`, `Function`, and `BinaryOperator` objects
then need their SAM methods to dispatch as native bodies too.

## Resolution

`native-collections/src/lib.rs` now registers:

- `java/util/stream/Collector.supplier()Ljava/util/function/Supplier;`
- `java/util/stream/Collector.accumulator()Ljava/util/function/BiConsumer;`
- `java/util/stream/Collector.finisher()Ljava/util/function/Function;`
- `java/util/stream/Collector.combiner()Ljava/util/function/BinaryOperator;`
- `java/util/function/Supplier.get()Ljava/lang/Object;`
- `java/util/function/BiConsumer.accept(Ljava/lang/Object;Ljava/lang/Object;)V`
- `java/util/function/Function.apply(Ljava/lang/Object;)Ljava/lang/Object;`
- `java/util/function/BinaryOperator.apply(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;`

The native implementations preserve the tagged collector fast paths for list,
set, collection, counting, joining, grouping, partitioning, mapping, and
collecting-and-then collectors while making interface-dispatch users see real
method bodies instead of no-Code interface stubs.

## Regression coverage

`collector_contract_interface_methods_registered` in
`native-collections/src/lib.rs` verifies that the full `Collector` contract and
the four returned functional-interface SAM methods remain registered in
`NativeMethodRegistry`.
