# The `cratonvm/synthetic/Process` cluster, adjudicated — and its class lies about its own supertype

**Status:** OPEN — adjudicated and measured 2026-08-06, closing the largest open
item of [`l5-native-io-bridge-residuals.md`](l5-native-io-bridge-residuals.md).
Two findings: the `Bridge` tag on 37 rows is wrong by the contract's own
definition, and the receiver class is **not** a subtype of `java.lang.Process`
in the one place the VM does not check for itself.

## The verdict on the tag

Contract §1.5 defines a `Bridge` as *what an `ACC_NATIVE` method binds to*.
`cratonvm/synthetic/Process`, `…/ProcessPipeInputStream`,
`…/ProcessPipeOutputStream`, `…/ProcessExitWaiter` and
`…/AnonymousObject$2` are in **no** JDK image, on **either** platform. There is
no `ACC_NATIVE` method for these to bridge to, so all 37 rows are `Bridge` by
inheritance from an ambient `set_category` and by nothing else.

**37 rows, not the 25 the L5 record states.** That count omits
`cratonvm/synthetic/AnonymousObject$2` (4 rows) and miscounts the pipe streams.
Per class, from `--dump-native-registry` on `JdkOnlyCensusLoadProbe`:

| class | rows | invocations |
|---|---:|---:|
| `cratonvm/synthetic/Process` | 21 | 0 |
| `cratonvm/synthetic/ProcessPipeInputStream` | 6 | 0 |
| `cratonvm/synthetic/ProcessPipeOutputStream` | 5 | 0 |
| `cratonvm/synthetic/AnonymousObject$2` | 4 | 0 |
| `cratonvm/synthetic/ProcessExitWaiter` | 1 | 0 |

The 21 on `Process` are 13 distinct methods: eight triples are registered twice,
`native-io/src/process.rs` overwriting `native-builtins/src/phases_late.rs`, and
the superseded halves own no slot (see the `owns_slot` census column).

## Why retagging is not the whole answer, measured

The obvious move is `SyntheticStub`, the one kind `--jdk-only` rejects. Under
`--jdk-only` the receiver class is one §5 forbids fabricating, so dropping its
natives ought to change nothing there — and `Compatible` mode registers all three
kinds, so nothing should change there either.

**It does not work out that way, because `--jdk-only` fabricates the class
anyway.** `probes/SubprocessKindProbe.java`, JDK 25:

| | HotSpot | CratonVM `--real-jdk` | CratonVM `--jdk-only` |
|---|---|---|---|
| `ProcessBuilder.start().getClass()` | `java.lang.ProcessImpl` | `cratonvm.synthetic.Process` | `cratonvm.synthetic.Process` |
| `…getSuperclass()` | `java.lang.Process` | `java.lang.Object` | `java.lang.Object` |
| subprocess works | yes | yes | yes |

So strict mode is running the exact substitution §5 forbids, and the `Bridge`
tag is what admits it. That is the same shape as the `Function$Identity`
successor defect, and it is the live half of
[`ensure-synthetic-class-cannot-enforce-only-record.md`](ensure-synthetic-class-cannot-enforce-only-record.md)
— whose measured "zero fabrications" results are for three workloads that never
spawn a subprocess. **The subprocess path is an unmeasured hole in that record's
coverage**, not a contradiction of it.

Retagging these 37 to `SyntheticStub` without first giving
`ProcessBuilder.start()` a real `java.lang.Process` subclass to return would
therefore break subprocess spawning under `--jdk-only` rather than fix it. Retag
first, then refuse — the order that record already establishes as the whole
lesson.

## The independent defect: the class is not a subtype of its own supertype

Found while establishing the above, and it is not a `--jdk-only` question — it
reproduces under `--real-jdk`, which is the default mode.

`probes/SubprocessSubtypeProbe.java` asks the subtype question seven ways. Six
agree with HotSpot: `instanceof Process`, `(Process) o`,
`Process.class.isInstance`, `Process.class.isAssignableFrom`, `List.of(p)`,
`Process[] arr; arr[0] = p`. All `true`/`ok`, all matching.

The seventh does not. Walking the class's own superclass chain:

```
HotSpot   chain=java.lang.ProcessImpl -> java.lang.Process -> java.lang.Object -> null
          ProcessInSuperclassChain=true   isAssignableFrom=true   CONSISTENT=true

CratonVM  chain=cratonvm.synthetic.Process -> java.lang.Object -> null
          ProcessInSuperclassChain=false  isAssignableFrom=true   CONSISTENT=false
```

`isAssignableFrom` says yes and the reflective hierarchy says no, **in the same
VM, about the same pair of classes**. Any library that decides assignability by
walking `getSuperclass()` — rather than asking `Class` — gets the opposite
answer from the one `instanceof` gives. That is a broad shape: serialization
frameworks, DI containers, matchers and mock frameworks all walk hierarchies by
hand.

It also means the `Bridge`-registered natives are the *only* reason this object
answers `Process`'s methods at all: it inherits nothing from `java.lang.Process`
because it does not extend it. Which is precisely why
[`process-natives-answer-for-user-subclasses-FIXED-20260806.md`](../../internal/process-natives-answer-for-user-subclasses-FIXED-20260806.md)
found the concrete methods answering from the synthetic layout — for this
receiver that is correct and necessary, and for a real subclass it is the bug.
The two records are the same registration seen from its two receivers.

## What would close this

1. **Give `spawn_and_wrap` a receiver that really extends `java.lang.Process`.**
   That fixes the supertype lie, makes the `getSuperclass()` walk agree with
   `isAssignableFrom`, lets the abstract-method registrations be reached by
   ordinary inheritance, and is the precondition for retagging. `toHandle()`'s
   existing note is the precedent: it stopped fabricating
   `java/lang/ProcessHandle` and built a real `ProcessHandleImpl` instead, for
   the same class of reason.
2. **Then retag the 37 to `SyntheticStub`** and let §5 refuse them, measuring
   `--jdk-only` subprocess spawning before and after. Expect `stub_ratchet`'s
   `BASELINE_SYNTHETIC_STUBS` and the `CRATONVM_NO_STUBS` drop list to move by
   exactly 37; a different number means something else was retagged too.
3. **Add a subprocess workload to the `ensure_synthetic_class` coverage.** Its
   three workloads report zero fabrications and none of them spawns anything, so
   the number is true and narrower than it reads.

Deliberately not attempted here: step 1 changes what `ProcessBuilder.start()`
returns, on a path WildFly, the Spring suites and the process-handle family all
depend on, and it wants its own change with a real-subprocess differential
rather than riding along with a measurement.
