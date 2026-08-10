# The census asks one class, on one platform — FIXED 2026-08-10

**Status: CLOSED.** The reading was fixed and the instruments shipped on
2026-08-05; the dispositions were taken the same week. This record closes the
five items that were left open, and one of them closed by being **disproved**
rather than done — see item 1.

Nothing here is a crash and no native's kind changed for a reason other than a
mislabel. The defect was always in what the measurement *means*.

## What the five open items became

| item | outcome |
|---|---|
| 1. Delete the 791 | **The list was unsafe as committed.** 277 rows are on classes this VM mints; deleting them would have removed working code. The gate is fixed, the rows are split out, and 179 of what survives is deleted. |
| 2. Restore what earlier waves deleted instead of gating | **Half the premise was wrong.** `14145d874` retagged; it deleted nothing. `21cfa930f`'s deletion cost nothing, because a second registration already carried the method — and that survivor is now gated rather than restored-as-a-duplicate. |
| 3. `Map.entry(..).setValue()` permissive under `--synthetic-jdk` | **Fixed.** `java/util/KeyValueHolder` exists, with the symmetry that blocked it in 2026-08-06 measured rather than assumed. |
| 4. The differential covers what it covers | **Widened** from 58 observables to ~300, across twelve `java.util`/`java.lang` families. It is RED on the first run and that is the point: two compatibility defects filed as `known-issues/jdk-only/W7-1-treemap-views-and-iterator-remove-contract.md`. |
| 5. `jdk-only-adjudicate.py` section 3 | already done; the script says so on every run. |

## Item 1 — the deletion list was unsafe, and the reason is this record's own rule

The *fifth image* section of this record states the rule plainly: **a synthetic
stub is CratonVM's own implementation, so a census of JDK images can only ever
report that the JDK does not have it. That is agreement, not evidence. Gate it,
never delete it.**

`scripts/jdk-only-dead-sweep.py` implemented that rule **per (class, method,
descriptor) triple** — through the `synthetic-stub` kind tag and a dispatch
filter that subtracts what three workloads reached. Both are triple-granular.
"This VM mints the class" is not.

What that cost, measured against the committed baseline: **277 of its 791 rows**
are on a class this tree fabricates. Among them:

* `java/util/HashMap$KeyItr.remove()V` — the write-through `remove()` that
  `RChmKeySetView` exercises and that
  `fixed-bugs/jdk-only-ensure-synthetic-class-deleted-FIXED-20260810.md` argues
  must not be traded away. The dispatch filter removed `hasNext` and `next`
  because the sampled workloads called them 1,209 times. It left `remove`
  because they never did.
* The whole `Atomic{Integer,Long,Reference}FieldUpdater$RustJvmImpl` surface —
  `addAndGet`, `compareAndSet`, `getAndSet`, `lazySet`, every method those
  updaters have. `alloc_impl` in `atomic_updater.rs` mints exactly those
  classes.
* `java/util/{ArrayDeque,LinkedList,TreeSet,TreeMap$Key,ServiceLoader}$Itr`,
  `java/lang/reflect/Proxy$Instance`, `java/util/IteratorEnumeration`, the
  `Predicate$$Lambda$*` shapes, `SSLSocketInputStream`/`OutputStream`, and the
  `SharedSecrets` owner singletons (`java/lang/System$1` and friends).

Every one of them carried `kind: bridge` — the ambient default — which is why
the tag-based gate never saw them. **That is the ambient-kind trap in its
sharpest form: a gate keyed on a tag nobody stated is a gate that fires on
nothing.**

### What changed

`scripts/jdk-only-dead-sweep.py` now answers the question three ways, and gates
on any of them:

1. **Class-level propagation.** A class ABSENT from every image, of which ANY
   triple is dispatched or tagged `synthetic-stub`, is a class this VM mints.
   Every row of it is gated. One dispatched method proves the class is live; a
   method the sample did not reach is not a dead method.
2. **`--minted`**, a census-independent answer for the classes no workload
   happened to touch: the list of class names the *source* passes to a
   fabrication funnel (`try_alloc_synthetic`,
   `try_alloc_concurrent_synthetic`, `try_ensure_synthetic_class`,
   `ensure_vm_internal_class`, …). A class this VM allocates and no image
   declares is minted whether or not this run reached it.
3. **`--pinned`**, for the rows where the class IS in every image and the method
   is in none. A method the JDK never declared can still be a deliberate
   completion of the synthetic surface — `StampedLock.isLocked()` is exactly
   that, `native-builtins/tests/registry_contracts.rs` pins it, and the census
   cannot see the statement of intent. Deleting it broke that test, which is how
   this rule was found.

The 277 are split into `scripts/baselines/jdk-only-dead-everywhere-GATED.tsv`
with the reason in its header. The candidate list is 512 rows, and the file says
what it is.

### What was deleted, and what was not

**179 registrations**, re-located by CONTENT rather than by the stale
`registered_by` line numbers, which the record already warned about. The
locator parses every `.register(` / `.register_with_kind(` call with a paren
matcher, resolves its first three arguments to string literals where the tree
makes that possible, and matches the triple. It refuses to guess: a call whose
class argument is a function parameter or a `format!` is reported, never
deleted.

`vm/src/runtime/instrument.rs` (20), `native-builtins/src/shared_secrets_bridge.rs`
(26 before gating), `native-io/src/direct_buffer.rs` (12) and
`native-builtins/src/jmx.rs` (21) are the largest clusters. Representative rows:
`java/util/zip/Deflater.initIDs`, `sun/nio/ch/SourceChannelImpl.read([BII)I`,
methods on constants-only types (`java/sql/Types`,
`java/nio/charset/StandardCharsets`, `java/nio/file/StandardOpenOption`) — names
no bytecode can resolve because no such member exists on any supported image.

**The `lang_string.rs` cluster was converted rather than deleted.** The record
described it correctly as "a covariant fan-out that registers each `append`
under three return descriptors, of which only some (class, descriptor) pairs
exist in any JDK". `register_string_builder_natives(registry, class)` is called
three times — `StringBuilder`, `StringBuffer`, `AbstractStringBuilder` — and
registered every method under all three covariant return spellings, so two
thirds of them were registrations no JDK declares and no bytecode can reach.
The descriptor is now derived from `class`: **28 covariant groups collapsed, 45
source registrations removed, ~113 census rows with them.** Converting the idiom
is what removes the whole family; deleting the sites would have removed only the
ones the sweep happened to catch.

**What is still not deleted, and why the number is honest:** 310 of the 512
candidates could not be located precisely, because their registration passes the
class name through a variable, a helper, or a table. They are left in the
baseline. A tool that guesses at those would eventually delete a live
registration, and this record exists because that mistake has been made three
times already.

## Item 2 — both halves of the premise, checked against the commits

**`14145d874` retagged. It deleted nothing.** Its diff is five
`register(..)` calls becoming `register_with_kind(.., NativeKind::SyntheticStub)`
— `ArrayList.subList`, `Collections.unmodifiableList`, `{List,Set,Map}.copyOf`.
All five are still that way. The item asked for a gate that was already there;
"removed five duplicate registrations" was a misreading of a commit whose
subject line says *duplicate registrations* and whose body says *retag*.

**`21cfa930f` deleted `native_map_entry`, and the method survived anyway** —
`phases_late.rs` registers `java.util.Map.entry` too, and wins by last-write, so
`--synthetic-jdk` never lost it. What the deletion actually cost was the thing
this record's own *Why "gate" and not "delete"* section predicts: it served one
mode. So the fix is not to restore a duplicate in `native-collections`; it is to
make the surviving registration correct (item 3) and **gate it**:
`Map.entry` is now `register_with_kind(.., SyntheticStub)` at both its sites, so
`--jdk-only` drops it and `java.base`'s own bytecode mints the real
`KeyValueHolder`, while `Compatible` and `--synthetic-jdk` keep a native that no
longer diverges. The kind is stated at BOTH registrations, not just the winner —
retagging only the surviving copy leaves the loser's kind claiming something
nothing uses, which is the same ambient-kind trap as item 1.

## Item 3 — `Map.entry` has a class of its own

The residual was left open deliberately and the reasoning was right: one
synthetic class name, `java/util/Map$Entry`, carried two contradictory
contracts. `Map.entry`'s entry is 2-field and must throw on `setValue`; the
entry-set views in `native-collections` and `properties_sidetable` mint the same
name with a third write-through `sourceMap` slot precisely so `setValue` writes
back into the map, which `entrySet()` iteration requires. An immutable
`setValue` could only ever win that last-write-wins race by breaking every
write-through, so registering one was refused — *a native whose correctness
depends on losing a race is not a fix*.

**A separate class removes the race instead of choosing a side of it**, which is
why HotSpot has `KeyValueHolder`. What landed:

* `synthetic_stub_fields` declares `java/util/KeyValueHolder` with the JDK's own
  two named slots, `key` then `value`. Undeclared, a bytecode `new` sizes the
  object from `num_total_fields`, allocates zero slots, and the heap's bounds
  guard drops both `set_field` writes into a WARN nobody reads — the defect that
  hid behind an unprobed constructor on 2026-08-06.
* `jdk_interfaces` links it to `java/util/Map$Entry` and **not** to
  `Serializable`: `Map.entry`'s return is specified as not serializable, unlike
  `AbstractMap$SimpleImmutableEntry`.
* `getKey` / `getValue` / a `setValue` that throws
  `UnsupportedOperationException`, plus `toString` / `hashCode` / `equals` from
  `register_entry_value_semantics`. All tagged `SyntheticStub`: real
  `java.util.KeyValueHolder` declares no `ACC_NATIVE` method, so §1.5 cannot
  call them bridges.
* `Map.entry(null, v)` raises `NullPointerException` before allocating, because
  `KeyValueHolder`'s constructor is two `Objects.requireNonNull` calls. The old
  native accepted nulls silently.
* The allocation pins its key and value across the mint. `alloc_synthetic` can
  complete a moving young GC, and the previous body read both arguments AFTER
  it — publishing pre-move addresses into a live object.

**The asymmetry that blocked this on 2026-08-06 is gone and was measured, not
assumed.** `SimpleEntry.equals(theKeyValueHolder)` answered false while the
reverse answered true because `SimpleEntry` was not a `Map.Entry` at all; that
was fixed the same day, and both `native_entry_equals` (native-collections) and
`register_entry_value_semantics` (native-builtins) open with an
`instanceof Map.Entry` test that a `jdk_interfaces`-linked `KeyValueHolder` now
passes in both directions.

## Item 4 — the differential, widened

`probes/ShadowDifferentialProbe.java` exercised `java.util`'s immutable
factories, `Map.entry`, and the views they hand out: a few dozen observables
against a census that counts ~1,600 inherited shadows. The honest reading of
"they match" was "the ones anybody looked at match".

It now covers twelve further families, each one the census names and nothing had
exercised against the bytecode it shadows: navigable maps and sets
(`headMap`/`tailMap`/`descendingMap` as VIEWS, not snapshots; `pollFirstEntry`),
access-order `LinkedHashMap`, deques and queues at both ends with both null
policies, iterator and list-iterator contracts (`remove` before `next`, `remove`
twice, `ConcurrentModificationException`), `java.util.Arrays`,
`java.util.Collections`' algorithms and unmodifiable/synchronized views,
`Optional`/`Objects`, the `java.lang.String` surface behind the sweep's largest
registration cluster, boxing/parsing/formatting, `StringBuilder`/`StringJoiner`,
streams and collectors, comparator combinators, and `BitSet`/`UUID`/`Base64`.

Every observable is deterministic across JVMs by construction — unspecified
iteration orders are sorted, no addresses or timings are printed, and every
expected-throw is reported as `t.getClass().getName()` rather than a message.

**It is RED, on the first run, in `--real-jdk` mode, and that is the item
closing rather than failing.** The item asked for the widening because
"widening that probe is the cheapest way to keep finding `Map.entry`-shaped
defects". It found two families immediately, both compatibility defects rather
than strict-mode ones, filed as
`known-issues/jdk-only/W7-1-treemap-views-and-iterator-remove-contract.md`:

* `TreeMap`'s navigable views are SNAPSHOTS — `headMap(..).remove(..)` does not
  reach the backing map — and `descendingMap()` / `descendingKeySet()` answer
  EMPTY rather than reversed. An empty view reads as a pass everywhere a caller
  only iterates, which is exactly the failure mode
  `probes/JdkOnlyCollectionViewProbe` was written to catch for `subList`; it
  does not cover `TreeMap`, and that is the gap this fell through.
* `Iterator.remove()` has no state machine: called before `next()` it does not
  throw `IllegalStateException` and **removes an element anyway**, so a loop
  guarded by that exception silently deletes one extra item per iteration.

**And one hazard in the probe itself, worth naming because it is the shape this
lane keeps meeting.** The first widened run stopped dead partway and printed
nothing after: `for (String s : l) { l.add(s); }` relies on the very
`ConcurrentModificationException` it is testing for to terminate, so on a VM
whose iterator does not throw it grows the list until the heap is gone and takes
the rest of the probe with it. A differential that cannot reach its next line
cannot report a difference, and a truncated transcript reads exactly like a
short clean run. Both cases are bounded now.

The pre-existing sections are unchanged by this — the factories, `Map.entry`,
the entry classes and the views the probe already covered still match HotSpot
byte for byte, including the `Map.entry(..).setValue` line item 3 fixed.

## Reproducing

```sh
# the census (all three instruments consume it; add --dump-class-origins for
# the interception join)
cratonvm --real-jdk --java-home "$JAVA_HOME" --explain-jdk-only \
    --dump-native-registry census.json -cp probes L5rProbe

# 1. inheritance
JAVA_HOME=<same image> sh scripts/jdk-only-inherited-decl.sh census.json

# 2. platform — unpack a Windows JDK of the SAME version, then
cratonvm --real-jdk --java-home <windows-jdk> --explain-jdk-only \
    --dump-native-registry census-win.json -cp probes L5rProbe
python3 scripts/jdk-only-platform-diff.py census.json census-win.json linux windows

# 3. abstract-method interception, including application classes
python3 scripts/jdk-only-interception.py --registry census.json \
    --classes classes.json --inherited undecl-out.tsv --only-user

# and the adjudication table with the bucket broken out
python3 scripts/jdk-only-adjudicate.py census.json --inherited undecl-out.tsv

# 4. the dead sweep, WITH the class-level gate
python3 scripts/jdk-only-dead-sweep.py --images reg-*.json --inherited inh.tsv \
    --dispatched reg-load.json reg-breadth.json reg-h2.json \
    --minted minted-classes.txt \
    --pinned native-builtins/tests/registry_contracts.rs --out dead.tsv
```

Both refuse rather than print zeroes when the census lacks
`--explain-jdk-only`, and the platform diff refuses if the two censuses do not
correspond row-for-row.
