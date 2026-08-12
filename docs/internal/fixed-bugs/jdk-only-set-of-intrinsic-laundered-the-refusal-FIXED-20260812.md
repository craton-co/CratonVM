# `Set.of` returned a MUTABLE `HashSet` under `--jdk-only`, because the refusal landed on a mis-tagged `Intrinsic`

| | |
|---|---|
| **Status** | FIXED 2026-08-12. `--jdk-only` is now byte-identical to HotSpot across all 89 rows of the new differential; `--real-jdk` is byte-identical to itself. |
| **Area** | `native-builtins/src/lib.rs::register_t19_h2_lookup_clinit_deps` — ten `java/util/Set.of` overloads |
| **Found by** | schema-4 census + `scripts/jdk-only-adjudicate.py`, looking for `SyntheticStub` rows that shadow concrete image bytecode |
| **Probe** | `probes/ImmutableCollectionsDifferentialProbe.java` (new) |

## The defect

```
                    HotSpot                              --jdk-only (before)
  Set.of("x")       java.util.ImmutableCollections$Set12  java.util.HashSet
  .add("y")         UnsupportedOperationException         NO THROW, size 2
```

A `Set.of` result that accepts `add` is not a nearly-right immutable set. It is
a mutable one, handed to callers that are entitled to assume otherwise — and
strict mode, the mode whose entire promise is that real class bytes are
authoritative, was the mode getting it wrong. `--real-jdk` answered correctly.

## Why strict mode was worse than Compatible

`java/util/Set.of` is registered **twice**, for all ten fixed arities:

| | registrar | kind | census |
|---|---|---|---|
| first | `native-builtins/src/lib.rs:30769…` | `Intrinsic` | `owns_slot: false` |
| second | `native-collections/src/lib.rs:16806…` | `SyntheticStub` | `owns_slot: true, overwrote: intrinsic` |

`register()` is last-write-wins, so in **Compatible** the `SyntheticStub` owns
the slot and produces the correct `ImmutableCollections$Set12`. The `Intrinsic`
never dispatches.

Under **`--jdk-only`** the `SyntheticStub` is dropped at registration — exactly
as designed, and `register_factory_natives`' own comment says so: *"Under
`--jdk-only` these are now dropped at registration … and the real bytecode
runs."* The first half is true. The second is not. Dropping the stub did not
uncover real bytecode; it uncovered **the older registration**, which contract
§1.4 keeps precisely because it is tagged `Intrinsic` ("concrete bytecode wins
over any registered native, *except* for a reviewed `NativeKind::Intrinsic`").

So the refusal was laundered into a worse answer than the one it refused — the
`W7-20` shape, one level up: not a wrong value, but a wrong *winner* after the
right registration was declined.

## The tag was false

The census tag above the block read:

> census-tag: faithful Set.of fixed-arity factories — **spec-exact immutable
> collections replicating real JDK bytecode** → Intrinsic.

Every body was `build_hashset_from_args` → `make_hashset_with_elements`: a
plain mutable `java.util.HashSet`. Not spec-exact, not immutable, not
`ImmutableCollections`. `Intrinsic` is the one kind strict mode may not drop,
so a stub wearing it is the most expensive possible mis-tag — and `kind_stated`
was true on these rows, i.e. the census reports them as *adjudicated by a
human*.

## The fix

Delete all ten. Not retag — delete: they own no slot in Compatible and there is
nothing left for them to do in strict.

* **Compatible is byte-for-byte unchanged**, which contract §5/§10 requires and
  the census predicted (`owns_slot: false` ⇒ unreachable). Verified by diffing
  the probe transcript before and after: identical.
* **Strict now runs real `java.util.Set.of` bytecode** and is byte-identical to
  HotSpot on all 89 rows — including the seven checks `--real-jdk` still fails.

The comment that justified them claimed `MethodHandles$Lookup.<clinit>` needs
the 2-arg form and `ClassFileDumper.<clinit>` the 8-arg one. That was a real
boot dependency when written and is discharged by the real bytecode: both modes
boot, and the regression corpus runs.

## What this leaves — the next removal, already measured

**`--real-jdk` is still wrong on this surface, in the fabricated-success
direction**, because the `native-collections` `SyntheticStub` factories still
own the slot there. Seven rows where the spec mandates failure and CratonVM
invents success, plus two shape divergences:

| observation | HotSpot | `--real-jdk` |
|---|---|---|
| `List.of("a", null)` | NPE | `[a, null]` |
| `Set.of("a","a")` | IAE `duplicate element: a` | `[a]` |
| `Set.of("a", null)` | NPE | `[null, a]` |
| `Map.of("a","1","a","2")` | IAE `duplicate key: a` | `{a=2}` |
| `Map.of(null,"1")` | NPE | `{null=1}` |
| `List.copyOf(asList("a",null))` | NPE | `[a, null]` |
| `List.copyOf(immutable) == src` | `true` | `false` |
| `subList.getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` |

Retiring those is a *deliberate* Compatible-mode change (they own the slot), so
it is a separate change with its own evidence — unlike this one, which was free.

**Sibling population, same shape, not taken here.** The schema-4 census has
exactly **16** triples where an `Intrinsic` is superseded by a `SyntheticStub`.
Ten were these. The other six are `java/util/logging/Handler`
(`phases_early.rs` intrinsic under a `reflect_annotations.rs` stub) and are
unmeasured; they sit in the JUL surface that is under active repair.

## Verification

| run | result |
|---|---|
| `ImmutableCollectionsDifferentialProbe`, `--jdk-only` vs HotSpot | **identical** (was: 8 divergences + 13 fabrication warnings) |
| same probe, `--real-jdk` before vs after | **identical** |
| `regression-suite/run.sh` `--real-jdk` | 42 passed, 0 failed |
| `regression-suite/run.sh` `--jdk-only` | 69 passed, 1 failed (`RJdkLogging`) |
| `stub_ratchet` | 8 passed |

**`RJdkLogging` is pre-existing and not this change's.** It fails in
`--real-jdk` too — where this change is provably byte-identical — with a
*different* assertion (`useParentHandlers=false must stop the walk` there,
`Formatter.formatMessage must substitute` under strict). It is a vector dev
added after the 2026-08-07 corpus baseline (`ea1da837a`), in the JUL surface
with live sibling branches.

**The kind-map ratchet is also red on dev already**, on 9
`cratonvm/internal/LinkedListSnapshotListItr` rows that lost `kind_stated` when
`6ae3ca634` re-minted them through the VM-internal door. The baseline is frozen
at `b240353e1`, many merges earlier. Deliberately **not** re-frozen here:
folding another change's drift into this change's note is what a slack-free
ratchet exists to prevent, and the row that moved is not one this change
touches.
