# G88-1 — which collections carry REAL state, and which carry ours

**Status:** MEASURED, then **PARTLY REFUTED BY A SECOND EXPERIMENT — read §5
before using §2.** Both experiments reverted, nothing shipped.
**Provenance:** eight `native-collections` registrars retagged to
`SyntheticStub` at once, five collection vectors run under `--jdk-only`,
CratonVM `C:/craton/target-nolto`, 2026-08-19. Reverted; tree unchanged.

---

## 0. The question this answers

`G85-1` §3a established, from one `LinkedHashMap` failure, that a registrar is
safe to retag only if the real bytecode can READ its receiver — and that class
identity does not settle it (`RealObjectCheck` reports 0 of 31 diverging while
`LinkedHashMap.keySet()` still broke).

One failure is an anecdote. The question the row actually turns on is: **which
collection classes carry real state, and which carry VM-owned state?** That is
answerable by experiment, and cheaply: retag everything, see what breaks.

## 1. The experiment

Eight registrars retagged to `SyntheticStub` in one build — `arraylist`,
`hashmap`, `linked_hashmap`, `hashset`, `concurrent_hashmap`, `array_deque`,
`priority_queue`, `stack`. (`tree_map` and `tree_set` did not match the edit
pattern and were left; they are unmeasured, not cleared.)

**All five collection vectors failed, and the messages say why:**

| vector | failure |
| --- | --- |
| `RCollections` | `NullPointerException` — `toArray()` on a keySet view |
| `RJdkCollections` | same |
| `RJdkMapViews` | `keySet IS content-equal (AbstractSet)` — the real view sees a different map |
| `RChmKeySetView` | `add() did not reach the backing map` |
| `RJdkBridge1` | `stringPropertyNames must not have changed the map's size` |

Every one is the same shape: real JDK bytecode running against a real JDK
object whose real fields were never populated, because the container's contents
live in a CratonVM side structure.

## 2. The result

**VM-owned state is pervasive across the stateful containers.** It is not a
`LinkedHashMap` quirk. Maps, sets, lists, deques, queues and `Properties` all
keep their contents somewhere the real bytecode cannot see.

That separates cleanly from what DID retag safely this session:

| retagged safely | why it worked |
| --- | --- |
| `unmodifiable` (300) | the wrapper is a REAL `Collections$Unmodifiable*`; measured directly |
| collection factories (36) | `List.of`/`Map.of`/`Set.of` return real immutable objects |
| `Comparator` (14) | stateless — there is no receiver state to read |
| `Vector` (28) | 0 invocations: unexercised, so unproven rather than proven |

**The rule the evidence supports:** a registrar retags safely when its surface
is STATELESS or its objects carry REAL state. It does not when the VM owns the
container's contents. That is one line, and it decides the rest of the row.

## 3. What this means for the P0 row

`Wholesale Bridge over-tagging` is **not** gated on tagging judgement, census
work, or reading 48 rationales. Those are all now done or cheap. It is gated on
**retiring the side state** so the real fields carry the truth.

That is the same work as:

* the *Residual synthetic native set* row — retiring the stubs IS retiring the
  state they maintain;
* `G80-1` N1 option B — the AWT raster, where the rasterizer owns pixels the
  real `DataBuffer` should hold;
* the `cratonvm/stream/LazyOp` marker (`G84-1`) — the synthetic stream model.

**Four P0 rows are facets of one project: the VM owns state that belongs to
real JDK objects.** Contract §8 assigns that to wave 2, with a
subsystem-per-PR discipline. No amount of measurement closes it, and this
record is the measurement that says so with numbers instead of a hunch.

## 4. NOMINATIONS

**N1 — `tree_map` and `tree_set` are UNMEASURED, not cleared.** They did not
match the edit pattern (their heads differ) and were skipped. 82 and 69
registrations respectively. Nothing here says they are safe.

**N2 — the safe-retag rule belongs in the crate header.** §2's one-liner —
stateless or real-state surfaces retag; VM-owned containers do not — is what
the next person needs before touching any registrar, and it is currently
spread across `G85-1` and this record.

**N3 — retiring state is testable incrementally, per container.** `HashMap`
first: make the real `table` the truth and delete the side map, then the
registrar retags by itself and the five vectors above become the proof. That is
a wave-2 shape, but it is a bounded first step rather than a rewrite.

## 5. CORRECTION — §2's conclusion was an artefact of my own experiment design

§1 retagged the CONTAINERS (`hashmap`, `linked_hashmap`, `hashset`,
`concurrent_hashmap`, …) and left the VIEW CARRIERS
(`map_view_carrier`, `set_view_carrier`, `chm_key_set_view`) as `Bridge`. So
`put`/`get` became real bytecode writing the real table, while `keySet()`
stayed a shim reading the side structure the containers no longer wrote.

**I created the split, then read the resulting failures as evidence of
pervasive VM-owned state.** That inference does not survive its own control.

Retagging the WHOLE cluster together — containers, view carriers, iterators,
map entries, bulk ops, ten registrars in one build:

| vector | §1 (containers only) | §5 (whole cluster) |
| --- | --- | --- |
| `RCollections` | NPE on `toArray()` | **PASS (53 checks)** |
| `RJdkMapViews` | `keySet IS content-equal` | **PASS (74 checks)** |
| `RChmKeySetView` | `add() did not reach the backing map` | **PASS** |
| `RJdkCollections` | NPE | NPE — but at the STREAM boundary |
| `RJdkBridge1` | `stringPropertyNames` size | `Hashtable`'s `equals/isEmpty` |

**Three of five went from broken to green.** And the two still failing are
outside the cluster, failing the same way for the same reason one level out:
`RJdkBridge1` on `register_properties_natives` (Hashtable/Properties, not
retagged), `RJdkCollections` at `java.util.List.size()` on a collector result
(the stream/collectors surface, not retagged).

**So the real finding is the opposite of §2's.** Real JDK bytecode CAN own
these containers. What fails is a PARTIAL retag: state ownership has to move as
a complete cluster, because a shim reading side state and real bytecode writing
real fields cannot both be half-right.

§2's rule stands only in this weaker form: a registrar cannot be retagged
ALONE if anything else still shims the same object's state. The unit of work is
the ownership cluster, not the registrar — which is why the per-registrar
discipline that worked for `native-awt` mis-fires here.

**What §2 got right and keeps:** `unmodifiable`, the factories and `Comparator`
retag safely as individuals precisely because they have no shared mutable state
to split — real wrappers, real immutable objects, and a stateless surface.

**This does not close the row either.** Two clusters remain unmapped
(Properties/Hashtable; streams/collectors, whose registrars hold 254 of the
crate's abstract-interface registrations and are the ones the file header warns
hardest about), and a whole-cluster retag has not been run through the arms —
only through five vectors.
