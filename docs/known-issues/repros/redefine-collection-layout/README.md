# Collection reproducer: a redefinition corrupted every synthetic collection

One `Mockito.mock()` anywhere in a process used to change what completely
unrelated, already-populated JDK collections do. Mockito is not part of this
reproducer — its inline mock maker was only ever a route to
`Instrumentation.redefineClasses`, and the corruption is the VM's.

`RedefineCollectionLayoutProbe` builds eleven collections, runs 89 operations on
them, redefines the collection classes **with their own bytes**, and runs the
same 89 again. Nothing about the classes changes across the redefinition, so any
difference is the VM's.

## Build and run

```bash
javac -d . cratonvm/Instrument.java RedefineCollectionLayoutProbe.java
cratonvm -cp . RedefineCollectionLayoutProbe   # must print PROBE PASS
java     -cp . RedefineCollectionLayoutProbe   # control: redefine skipped
```

## What it caught

CratonVM implements these collections as small synthetic objects — a bucket
array and a size, not the JDK's `table` / `root` / `head` field graph — and
every operation is a registered native. A redefinition drops native shadows so
an agent's woven bytecode can run, which is right for an ordinary class and
catastrophic here: the real JDK bodies then index fields the object does not
have.

**32 of 89 operations changed behaviour.** The dangerous ones are silent:

```
DIFF TreeMap.get                v7        -> null
DIFF TreeMap.containsKey        true      -> false
DIFF ConcurrentHashMap.get      v7        -> null
DIFF ConcurrentHashMap.size     12        -> 0
DIFF ConcurrentHashMap.isEmpty  false     -> true
DIFF HashMap.keySet             [k0..k11] -> []
```

The rest throw — `LinkedHashMap$Node.getKey` NoSuchMethodError,
`AnonymousObject$4 cannot be cast to Map$Entry`, `TreeSet` NPEs on a null
`this.m`.

The probe deliberately includes `Attributes extends LinkedHashMap`, the shape of
Spring's `AnnotationAttributes`, because a user subclass reaches the parent's
native through a different dispatch path than a direct instance does.

## Two things this cost, worth not repeating

**The immunity list is consulted from seven places, and six of them
re-assembled their own chain.** Adding the collections to
`redefine_immune_forced_native` took the probe from 32 broken operations to 18,
not to 0, because the invoke-cache sites open-coded
`string_builder || path` and never saw the new arm. If a fix to that predicate
looks partially effective, this is why. `layout_immunity_is_not_open_coded`
now fails the build if anyone names an arm directly.

**Do not fix that by pointing the cache sites at the full predicate.** That was
tried, and it regressed: Spring AOT chunk 3 began failing roughly one run in
eight with `NoSuchMethodError: java.lang.Integer.isArray()Z` and
`Integer.represents(Type)Z` out of ByteBuddy's
`TypeDescription$Generic$Visitor$Substitutor` — a signature that appears in no
earlier log across three full 20-chunk sweeps. Measured under one harness:

| binary | chunk 3, 9 runs |
|---|---|
| unmodified | 9/9 clean |
| cache sites broadened to the full set | 8/9 |
| cache sites given the layout arm only | 9/9 clean |

The full set also covers reflection metadata, JFR, BC crypto, StampedLock and
FileHandler. Those exist for the slow path; asserting them on the cache paths
changes real dispatch. `redefine_immune_layout_native` is the narrow predicate
the cache sites use, and what its members share is checkable: the receiver's
real JDK body indexes a layout CratonVM's object does not have.

## Related

* [`../redefine-builder-layout/`](../redefine-builder-layout/) — the same defect
  for `StringBuilder`, fixed first. Its list is method-wise rather than
  class-wide because two builder methods are genuinely stubbed on mocks.
* [`../redefine-call-cost/`](../redefine-call-cost/) — the throughput half of
  what a redefinition used to cost.
