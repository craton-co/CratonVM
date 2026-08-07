# `PulsarAutoConfigurationTests` — `ClassCastException: Object cannot be cast to MultiValueMap` — FIXED 2026-08-07

**Status: ✅ FIXED.** `Stream.collect(Collector)` is a CratonVM native, and its
ordinary-JDK-`Collector` path held the accumulated container — and the
collector, the accumulator, the finisher and every stream element — as raw
`ObjectRef`s across five interpreter re-entries **with no `pin_native_root`
anywhere**. A moving young collection in that window relocated the container;
the native then handed the caller its pre-copy address, which the semispace
swap leaves in the inactive semispace, where the vacated slot reads back as an
all-zero header — i.e. as `java.lang.Object`.

That is the failure verbatim:

```java
// OnBeanCondition.java:583
MultiValueMap<String, @Nullable Object> attributes = annotations.stream(annotationType)
    .filter(MergedAnnotationPredicates.unique(MergedAnnotation::getMetaTypes))
    .collect(MergedAnnotationCollectors.toMultiValueMap(Adapt.CLASS_TO_STRING));
```

```
java.lang.ClassCastException: java.lang.Object cannot be cast to org.springframework.util.MultiValueMap
    at org.springframework.boot.autoconfigure.condition.OnBeanCondition$Spec.<init>(OnBeanCondition.java:583)
```

## The defect

`native-collections/src/lib.rs::collect_via_collector_protocol` drives the
standard `Collector` protocol:

```
supplier()  ->  Supplier.get()  ->  accumulator()
            ->  BiConsumer.accept(container, elem)   [once per element]
            ->  finisher()      ->  Function.apply(container)
```

Every one of those is an `invoke_virtual` that re-enters the interpreter and
can allocate, therefore can trigger a moving young collection. The path held
`collector`, `container`, `accumulator`, `finisher` and `elements` across all
of them as plain locals.

`MergedAnnotationCollectors.toMultiValueMap` is a real Spring `Collector`, not
one of CratonVM's own tagged fast-path collectors, so it takes exactly this
path. Its container is a `LinkedMultiValueMap` allocated by `supplier.get()` —
young, and therefore movable — and it is live across the entire accumulate
loop.

**What hid this: the function was PARTIALLY pinned.** Its *other* arm — the
degenerate `java/lang/Object` collector, sixty lines above — pins supplier,
accumulator, finisher, container and every element and explains why. So does
every tagged-collector arm in `native_stream_collect`, under the comment
*"cceres3: pin across GC-capable call (stream stale-at-store wave) … every
element, lambda and accumulated ref must be re-read through a pin after each
such call"*. Reading either one makes the file look like it already has the
discipline. Only the ordinary path was missed — the path a real Spring or JDK
`Collector` takes, and the one no CratonVM unit test exercises.

## Positive control: `probes/CollectorPinProbe.java`

Rather than buy a hit rate on a 1-in-7 flake, this makes the window wide: a
plain user `Collector` whose accumulator allocates hard, so a young collection
lands inside the accumulate loop nearly every round, with the result assigned
to the finisher's declared type (a real checkcast).

| VM / binary | result |
|---|---|
| HotSpot 25.0.3 | `rounds=300 ok=300` **CLEAN** |
| CratonVM, before | `rounds=300 ok=296 other=4` **BROKEN** |
| CratonVM, after | *(see below)* |

and the failing arm emits the same guard the Pulsar `.err.log` carried:

```
NoSuchMethodError java/lang/Object.accept(Ljava/lang/Object;Ljava/lang/Object;)V
cratonvm::gc::guard: receiver points into RECLAIMED memory — a still-referenced object was
  collected. `java.lang.Object` here is the all-zero header the collector left behind…
  location=young TO-space (the inactive semispace) span="0x2002c000000+0x0"
…and NO live heap object holds this address in a decoded reference slot. The holder is
  therefore a frame local, a register, or a native side table — not a heap field.
…in_published_snapshot=false … a root COLLECTION gap, not a mark or sweep one.
```

In the probe the stale reference is the accumulator lambda; in Pulsar it is the
container. Same unpinned window, same function.

## Correction: the GC framing was right, and the page talked itself out of it

This page recorded a `cratonvm::gc::guard` hit naming
`target_class=org.springframework.util.MultiValueMap` 1.4 s before the failure,
and then set it aside as *"a tempting but not yet earned GC theory"*, citing
the standing caveat that the guard's RECLAIMED ring is re-served by the
allocator, so a hit is evidence about the *address's history*.

That caveat is about **reused** addresses. The line here said something else,
and the difference is one field:

* `location=young TO-space (the inactive semispace)`, `span="…+0x0"` — nothing
  has been re-served there. This is a reference that was never remapped.
* the companion lines say it outright: *"the holder is a frame local, a
  register, or a **native side table** — not a heap field"*, and
  *"`in_published_snapshot=false` … a root **COLLECTION** gap, not a mark or
  sweep one"*.

**Read the guard's `location=` and its companion lines before applying the
"it's about the address" caveat.** Doing so here would have pointed at native
code holding an unrooted reference on the first run.

The dispatch framing that replaced it was not wrong to pursue — the ancestry
check against `383e7f5cf` was correct and the site-alias census was worth
running — but it was answering a question the evidence had already closed. The
census result stands as a fact about the workload (1007 site keys observed, key
recycling pervasive) and as a fact about the defence (`site_keyed_memos!`
generates the flush alongside the declaration, so coverage is structural); it
simply is not this bug.

## Reproducing

```bash
javac -d /tmp probes/CollectorPinProbe.java
java     -Xmx512m -cp /tmp CollectorPinProbe 300 40 400      # HotSpot: CLEAN
cratonvm --java-home $JDK --Xmx 512m -c /tmp CollectorPinProbe 300 40 400
```

~20 seconds, no Spring, no suite, no load. Prefer it to the class.

## Affected classes

- `module/spring-boot-pulsar` — `PulsarAutoConfigurationTests$SchemaResolverTests.whenHasUserDefinedBeanDoesNotAutoConfigureBean`

Any `stream.collect(<a real Collector>)` was exposed, so this was never
Pulsar-specific; Pulsar is simply where a young collection happened to land
inside the window often enough to be noticed.
