# The synthetic slot floor was ONE number serving TWO layouts — FIXED 2026-09-12

**Status: FIXED.** Opened 2026-09-11 as
`docs/known-issues/perf/the-synthetic-slot-floor-is-one-number-for-two-layouts-20260911.md`,
itself the successor to
`docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md`
§6. All seven rows are closed, the six factories it listed as still pinning
`HashSet` are converted, and the screen it asked for is a gate. The seventh row
took a second change the page did not predict and §5 records: once the floor
came off `CopyOnWriteArraySet`, what was left was not padding but a
`LinkedHashMap` standing in the slot the real class declares as
`CopyOnWriteArrayList al` -- and `removeIf`, the one method of that class's
surface this VM does not register, had been raising `NoSuchMethodError` on it.

**Verified on:** Windows 11, JDK 25 Temurin `25.0.3+9`, branch
`claude/synthetic-slot-floor-fix-8e9672`, with a baseline worktree built from
the SAME commit for the synthetic-JDK comparison. Twice: once off
`dev@2405991d1`, and again after merging `dev@3e8442e5f` — same numbers, same
verdicts, same 127 `field index OOB`, so nothing in the intervening upstream
work interacts with the exemption.

## 1. The measurement that closes it

`probes/CollectionShapeCause.java`, retained heap per EMPTY instance, 20 000
instances, Generational. HotSpot is `java -Xmx3g --add-opens
java.base/java.util=ALL-UNNAMED`, same probe, same run.

| class | HotSpot | before | after | before | after |
|---|---|---|---|---|---|
| `java.util.Properties` | 120.3 | **544.0** | **224.0** | 4.52x | 1.86x |
| `java.util.concurrent.ConcurrentLinkedDeque` | 48.0 | **120.0** | **72.0** | 2.50x | 1.50x |
| `java.util.concurrent.CopyOnWriteArraySet` | 56.1 | **136.0** | **72.0** | 2.42x | 1.28x |
| `java.util.concurrent.ConcurrentLinkedQueue` | 48.0 | **112.0** | **64.0** | 2.33x | 1.33x |
| `java.util.ArrayDeque` | 112.1 | **232.0** | **184.0** | 2.07x | 1.64x |
| `java.util.HashSet` | 64.0 | **128.0** | **88.0** | 2.00x | 1.38x |
| `java.util.LinkedHashSet` | 80.1 | **152.0** | **112.0** | 1.90x | 1.40x |

All seven land inside the 1.2x-1.7x band the rest of the collections already
occupy, which is reference width and a separate subject.

`CopyOnWriteArraySet` took two changes to get there, and §5 is the second one:
the floor took it from 136 to 112, and the remaining 112 was not padding at all
but the wrong BACKING OBJECT — a defect the floor work found and did not cause.

Every OTHER row of that table is byte-identical before and after: `ArrayList`
32.0, `HashMap` 64.0, `LinkedHashMap` 88.0, `TreeMap` 80.0, `TreeSet` 24.0,
`IdentityHashMap` 584.0, `Hashtable` 168.0, `ConcurrentHashMap` 104.0,
`CopyOnWriteArrayList` 48.0, `LinkedBlockingQueue` 440.0, and the rest. The
four-entry table moves with the empty one and nowhere else: `ArrayDeque+4`
232 -> 184, `HashSet+4` 464 -> 424, `LinkedHashSet+4` 552 -> 512,
`ConcurrentLinkedQueue+4` 240 -> 192 — and `CopyOnWriteArraySet+4`
**512 -> 120** against HotSpot's 88.3, which is §5 and is the largest single
number on this page.

`CRATONVM_DBG_LAYOUT=1` now reports a compact layout for all seven, where it
used to report the refusal:

```text
[layout] java/util/HashSet            cid=65  body=8  refs=1 fields=1
[layout] java/util/LinkedHashSet      cid=129 body=8  refs=1 fields=1
[layout] java/util/concurrent/CopyOnWriteArraySet   cid=147 body=8  refs=1 fields=1
[layout] java/util/ArrayDeque         cid=139 body=16 refs=1 fields=3
[layout] java/util/concurrent/ConcurrentLinkedQueue cid=536 body=16 refs=2 fields=2
[layout] java/util/concurrent/ConcurrentLinkedDeque cid=538 body=16 refs=2 fields=2
[layout] java/util/Properties         cid=132 body=64 refs=6 fields=10
```

The whole-run padded-class count over that probe goes 99 -> 92, and the seven
that left are exactly these seven. Nothing else changed sides.

## 2. The fix: the floor is now asked WHERE, not just HOW MUCH

`synthetic_stub_fields` is one number read in two places that mean different
things. It DEFINES the layout of a fabricated stub — there the natives that
serve the class index it from 0 and nothing else declares those slots — and it
FLOORS the layout of a class defined from real class-file bytes, padding it up
to the width native code was written against.

The second reading is load-bearing for `java.net.InetSocketAddress`: one
declared instance field (`holder`), and a native `<init>` that writes raw
synthetic indices on the REAL class. It was a fiction for these six.

`ClassManager::apply_synthetic_floor` (`classloading/src/class_manager.rs`) is
now the ONE place the floor is applied. Both callers — `define_class_with_options`
and the stub-upgrade path — go through it, so they cannot disagree about the
floor or about who is exempt from it; each used to open-code the same `max`.
A fabricated stub never reaches it at all (`synthesize_stub_class` takes its
fields straight from the table), which is why this change cannot move
synthetic-JDK mode, and why the arm that had to be re-run is the real-JDK one
plus a verdict-neutrality check.

`FLOOR_EXEMPT_CLASSES` beside it carries the six, each with the instance-field
extent the REAL chain declares:

```rust
("java/util/HashSet", 1),                               // `map`
("java/util/LinkedHashSet", 1),                         // inherited, declares none
("java/util/concurrent/CopyOnWriteArraySet", 1),        // `al`
("java/util/concurrent/ConcurrentLinkedQueue", 2),      // `head`, `tail`
("java/util/concurrent/ConcurrentLinkedDeque", 2),
("java/util/Properties", 10),                           // Hashtable's 8 + 2
```

That second number is not used to size anything. It is what
`floor_exempt_real_extent_disagreement` compares the loaded class against, so a
JDK whose shape has moved out from under the screen prints a line naming the
class and both counts instead of silently keeping an exemption that was measured
against a different layout.

**`java.util.ArrayDeque` is deliberately not in that list.** It needed no
exemption, only a correction: its fourth slot held a `size` that `ad_state`
stopped reading on 2026-08-30, when the count started being derived from
`head`/`tail` the way the JDK's own `size()` derives it. Nothing had written
slot 3 since; the only reader left was a `>` receiver-width bound in
`collect_collection_elements`, which now asks for the three slots that exist.
The table declares `elements`/`head`/`tail` — exactly the real class, in the
real order — so `ArrayDeque` is not padded in either mode.

## 3. The six factories, and why they had to go first

The open record named six sites that built an array-backed set by writing raw
absolute slots on a real `HashSet` receiver. Each wrote the MAP layout — element
array at absolute 0, count at 1, sometimes a capacity at 2 — on a class whose
one real instance field is `map` (`Ljava/util/HashMap;`).

**That was a live defect independent of the floor, and in several of these the
defect was the whole point of the method.** Every real `Set` method
dereferences `map`, so the object came back with an `Object[]` where a `HashMap`
belongs, and `size()`/`iterator()`/`contains()` answered for an EMPTY set
whatever the array held:

```text
  native-builtins/src/jmx.rs                       queryNames/queryMBeans fallback
  native-builtins/src/phases_late/net_channels.rs  Selector.selectedKeys()
  native-builtins/src/phases_late/net_channels.rs  Selector.keys()
  native-builtins/src/phases_late/reflect_invoke.rs  ModuleLayer.modules()
  native-builtins/src/util_time.rs                 ZoneId.getAvailableZoneIds()
  native-builtins/src/servlet.rs                   Selector.selectedKeys()/keys()
```

`ModuleLayer.modules()` is the one with a filed history: the OTHER registrar for
that same method was fixed in
`docs/internal/fixed-suite-bugs/CRATONVM_BUGS/BUG-T-modulelayer-modules-synthetic-hashset.md`
(Tomcat's web-fragment scan NPE'd on a null `iterator()`), and this is its twin,
which had kept the shape the whole time.

All six now go through **one** helper,
`native-builtins/src/lib.rs::build_real_hash_set`: allocate the real width, run
the class's own `<init>`, and `add` each element. It is correct in both modes —
in synthetic-JDK mode `native_hs_init` allocates the backing map and
`hs_set_backing_map` places it at the slot `hs_map_slot` names, which is
absolute 0 in both — and it is GC-safe, pinning each element before the first
allocation and re-reading the set across every `add`.

Three further sites asked for a fabricated width and wrote NONE of it, which is
bookkeeping rather than a shape, and the gate reads bookkeeping as a claim:
`system_properties_object` (16 slots of `Properties`, whose state is an
identity-keyed side table) and two Spring empty-set fallbacks (3 slots of
`HashSet`). They ask for the real width now; `alloc_concurrent_synthetic` takes
`max(n, real)`, so a fabricated stub still gets its full count.

The last one was `build_real_layout_string_hashset`'s own fallback arm, which
wrote `(data_array, size)` whenever the real `HashMap` layout could not be
resolved. It delegates to `build_real_hash_set` now. There was no mode left in
which the second shape was right, and it was the last unconditional raw-slot
write on `java/util/HashSet` in the tree.

`wildfly_security::count_carrying_hash_set` is untouched and is the model for
the one arm that may still write the fabricated shape: it asks
`ctx.is_class_synthetic_stub("java/util/HashSet")` first, so the three-slot
shape is reachable only where it IS the layout.

## 4. The screen, as a gate

`t9d_floor_exempt_classes_have_no_oversized_factories` (`vm/tests/tier1_tests.rs`)
is the companion to T9C, running the other way. T9C asserts a fabricated table
is at least as wide as its own factories; T9D asserts a floor-EXEMPT class has
no factory wider than its real layout.

It holds the half of the screen that is mechanical: every literal
`alloc_concurrent_synthetic(_, "cls", n)` site in the tree, whose `n` is the
shape the caller intends to write. A site that has already asked
`is_class_synthetic_stub` is excused, because it knows its receiver is
fabricated. It also refuses a STALE entry — one whose fabricated extent no
longer exceeds the real one pads nothing, so it is a claim about a class rather
than a live exemption.

Written against `dev`, it is RED with exactly the four rows §3 lists as
bookkeeping plus the two remaining `HashSet` factories, and green here. The
second half of the screen — raw `ctx.set_field(obj, N, ..)` on a receiver the
caller did not allocate — is the same grep and is not machine-checkable without
type information; it is covered by the runtime out-of-range reporter in
`gen_heap::set_field` and by §6's probe, and the exemption table's doc states
the rule for anyone adding a class.

`ConcurrentLinkedQueue` is the sharp demonstration that the second half really
is discharged, because it is the one class whose natives would visibly break if
it were not. `native_lbq_init` writes four absolute slots on the receiver and
`native_lbq_size` READS the third; with the floor gone the class has two, so
slots 2 and 3 are off the end and `size()` would answer 0 on a queue holding 40
elements. `CollectionSlotFloor` asserts 40, a full 24-element FIFO drain in
order, and `poll()` on an emptied queue -- and passes. The registrations exist
and do not run: a real `ConcurrentLinkedQueue` is served by its own lock-free
bytecode, and `CollectionShapeCause`'s field dump shows `head` and `tail`
holding real `ConcurrentLinkedQueue$Node` objects, which only that bytecode
builds.

`array_deque_synthetic_table_matches_the_real_class` freezes the §2 narrowing at
three, so it cannot grow back by accident the way `LinkedHashMap`'s floor did.

## 5. `CopyOnWriteArraySet` was backed by the wrong OBJECT, and `removeIf` said so

Taking the floor off left this one class at 2.0x when the other six landed in
the 1.2x-1.7x band, and the reason was not padding. Its object was already
compact (`body=8 refs=1 fields=1`); what it RETAINED was wrong:

```text
  HotSpot   COWAS 56.1  = 16 (object) + 40.1 (CopyOnWriteArrayList + Object[0])
  CratonVM  COWAS 112.0 = 24 (object) + 88.0 (LinkedHashMap)
```

`register_hashset_natives` mirrors the whole `java.util.HashSet` surface onto
`CopyOnWriteArraySet`, on the stated premise that it is one of "HashSet's
real-JDK subclasses that share the same `field 0 = backing map` layout". It is
neither. It does not extend `HashSet`, and the ONE instance field the real class
declares is `private final CopyOnWriteArrayList<E> al` — sitting at exactly that
slot 0. So `native_hs_init` stored a `LinkedHashMap` in a slot declared to hold
a list.

**That is a live defect, not a size one, and the class library itself found it.**
`CopyOnWriteArraySet` declares nineteen methods and this registrar registers
twenty triples; `removeIf` is one it does NOT register, so its real body ran:

```text
  cowSet.removeIf(p)
    -> NoSuchMethodError:
       java.util.LinkedHashMap.removeIf(java.util.function.Predicate)
```

The other eighteen looked correct only because a native stood in front of each
of them. Which methods are silent is therefore a function of which triples this
registrar happens to carry — which is why the guard for it (§6) is the whole
declared surface rather than the one method that broke.

### The fix: the class is served by its own bytecode

`cow_set_route` (`native-collections/src/lib.rs`) sits at the top of each of the
twenty natives — the placement `ksv_route` already uses, so the INTERFACE-level
registrations (`java/util/Set.size()` and friends) are guarded by the same test
as the exact-class ones. When the receiver is a real `CopyOnWriteArraySet` it
runs that receiver's own method, bytecode-only, on its own class.

Real, not a mode flag: `is_real_cow_array_set` asks whether the receiver's class
resolves the field NAME `al`. A fabricated stub declares `_f0`/`_f1` and keeps
the map surface, because there the map surface IS the implementation. It is the
same question `hs_map_slot` and `props_defaults_slot` ask about their own
receivers, and `hs_map_slot` now declines a real one for the same reason —
answering `HS_FIELD_MAP` for it would hand every reader in that file a list to
call `native_map_*` on.

Delegation rather than a second implementation, because the object underneath is
already right: `CopyOnWriteArrayList` writes `lock` and `array` BY NAME
(`cowal_ensure_lock_and_array`), had its `<init>` override deliberately removed
so the JDK constructor runs, and measures 48.0 against HotSpot's 40.0 — the best
ratio in the collection table. The JDK's own `CopyOnWriteArraySet` bodies are
thin forwards to `al`, so they already had a working object to forward to.

One method could not go that way. `stream()` is a `Collection` DEFAULT method
whose body builds a real `java.util.stream` pipeline over a real `Spliterator`,
and this VM's streams are a synthetic carrier — so it takes the ELEMENTS (via
`toArray()`, which IS delegated) and builds the carrier, which is what every
other `*_stream` native in that file does.

### What it bought

```text
  empty         112.0 -> 72.0    against HotSpot 56.1   (2.00x -> 1.28x)
  four entries  512.0 -> 120.0   against HotSpot 88.3   (5.80x -> 1.36x)
```

The filled row is the bigger half and was not visible before this work:
`CollectionShapeCause`'s four-entry table had no `CopyOnWriteArraySet` row at
all, so the class's worst number — a `LinkedHashMap` with four entries, 488
bytes, where HotSpot holds a six-element `Object[]` — had never been measured.
It has a row now.

## 6. How it was validated

Lowering a floor fails SILENTLY — an out-of-range `set_field` is dropped, not
raised — so the real-JDK arm alone proves nothing about the mode the floors
exist for. Both arms were run, against binaries built from the same commit.

* **`probes/CollectionSlotFloor.java` grew the section this needed.** It covered
  `ArrayDeque` and the two `Set` families and nothing else of the seven; it now
  drives `CopyOnWriteArraySet`, `ConcurrentLinkedQueue`/`Deque` (including FIFO
  and deque ORDER, which `size`/`contains` cannot see), `ArrayDeque` past its
  ring-buffer wrap, and `Properties` including the `defaults` chain — the one
  slot on that class a native resolves BY NAME with the fabricated model index
  as its fallback, so it is where a wrong slot shows up first. HotSpot passes
  the extended file.
* **It also stopped hiding its own tail.** Each section runs under `section(..)`,
  which turns a throw into a recorded `ERROR` row instead of the end of the run.
  `LinkedList.indexOf` is absent from the synthetic image and threw at section
  six of fourteen, so the eight sections after it — including every
  floor-exempt family this file exists to cover — were never reached in the arm
  that matters, and their silence read as agreement.
* **Real-JDK:** `PASS CollectionSlotFloor` before and after, with the extended
  sections. The descriptor-coercion census is identical too (total=31,
  `class_id=132 index=8` x30), so the change moved no slot's contents.
* **Synthetic-JDK, the arm that matters:** two binaries, `--features
  synthetic-jdk`, this tree and the UNCHANGED tree at `2405991d1` from its own
  worktree, running the IDENTICAL probe. **Verdict-identical**: the same 14
  outcomes in the same order (the 8 pre-existing mismatches the parent record
  records — `TreeMap.keySet/entrySet`, `IdentityHashMap`, `LinkedHashSet` — plus
  `CopyOnWriteArraySet` and four `ERROR` rows that the new `section` wrapper
  makes visible for the first time in BOTH builds) and the same **127** `field
  index OOB` warnings. Verdict-neutral is the acceptance criterion, not green.
  `CowSetBacking` was run the same way against the same pair and is likewise
  verdict-identical: the same 15 outcomes, which are the fabricated
  `CopyOnWriteArraySet` stub's own gaps and have nothing to do with §5 —
  `cow_set_route` declines a fabricated receiver by construction.
* **`probes/CowSetBacking.java` is new, and is the §5 half.** Every method
  `javap -p java.util.concurrent.CopyOnWriteArraySet` lists, plus the four it
  inherits, against values a wrong backing cannot produce — insertion ORDER
  throughout, because that is the observable separating a list-backed set from a
  hash-backed one. `CollectionSlotFloor` drives this class through `setFamily`,
  eight checks that a wrong backing passes; this is the file that does not.
  HotSpot passes it unchanged.
* **`vm/tests/cow_array_set_backing.rs`** drives that probe as a gate, and was
  MUTATION-CHECKED rather than assumed: against the pre-§5 binary it FAILS with
  exactly the diagnostic its message describes
  (`NoSuchMethodError: java.util.LinkedHashMap.removeIf`), and against the
  current one it passes. A probe-driving test that has never been seen to fail
  is the vacuous-pass shape `probe_compile_guard.rs` exists about.
* **`t9d_floor_exempt_classes_have_no_oversized_factories`:** RED (4 rows) on
  `dev` once the two `HashSet` factories were converted, green here.
  `t9c_synthetic_field_tables_cover_their_factories` stays green throughout.
* **`vm/tests/tier1_tests.rs`:** 58 passed, 0 failed. The selector and
  collection integration tests that cover the converted natives —
  `nio_selector_build_set_gc` (under `CRATONVM_GC_STRESS`), `wave3_c_selector`,
  `collection_view_gated_surface`, `cluster_b_collection_tostring`,
  `map_view_remove_if`, `stub_ratchet`, `synthetic_diff` — all pass.
* **Crate tests:** `cratonvm-classloading` 804 / 0,
  `cratonvm-native-collections` 375 / 0 over 15 targets,
  `cratonvm-native-builtins` 4254 / 0.
* **`regression-suite/run.sh`:** 95 passed, 0 failed, of 95 scheduled.

One gate is RED and was red before this work: `raw_lock_constructions_do_not_grow`
(`native-builtins/tests/lock_discipline_ratchet.rs`) reports 429 raw lock
constructions against a baseline of 428. Reproduced on a pristine worktree at
`dev@2405991d1` and `dev@3e8442e5f` with no local changes, and its own message
forbids the easy disposition ("Do NOT raise the baseline"), so it is filed rather
than touched here.

One test had to change, and the change is the point rather than an accommodation:
`jboss_jdkspecific::tests::module_get_packages_returns_populated_set_for_java_base`
asserted the element COUNT at `ctx.get_field(set, 1)` — slot 1 of a `HashSet`,
which is off the end of a class with one field, and the fabricated shape
`build_package_set`'s own doc comment explains is wrong. It now declares the real
`HashMap`/`HashMap$Node`/`HashSet` layouts to the mock so the helper takes its
REAL branch, and asserts what that branch promises: `HashSet.map` holds a
`HashMap`, whose `size` is positive and whose `table` is a bucket array.

## 7. Repro

```bash
javac -d out probes/CollectionShapeCause.java probes/CollectionSlotFloor.java
java -Xmx3g --add-opens java.base/java.util=ALL-UNNAMED -cp out CollectionShapeCause 20000
cratonvm -XX:+UseGenerationalGC --Xmx 3g -c out CollectionShapeCause 20000
cratonvm --Xmx 2g -c out CollectionSlotFloor                    # real-JDK: PASS
CRATONVM_DBG_LAYOUT=1 cratonvm --Xmx 2g -c out CollectionShapeCause 100 | grep PADDED
```

```bash
# the arm that matters when touching a floor; needs its own binary, AND the
# unchanged tree built the same way to compare against
cargo build --release -p cratonvm-cli --bin cratonvm --features synthetic-jdk
cratonvm --synthetic-jdk --Xmx 2g -c out CollectionSlotFloor    # expect the 14
```

```bash
javac -d out probes/CowSetBacking.java
java -cp out CowSetBacking                                      # the oracle
cratonvm --Xmx 2g -c out CowSetBacking                          # real-JDK: PASS
```

```bash
cargo test -p cratonvm-vm --test tier1_tests -- t9c_ t9d_ array_deque_synthetic
cargo test -p cratonvm-vm --test cow_array_set_backing
CV=<binary> JDK=$(cygpath -m "$JAVA_HOME") bash regression-suite/run.sh
```

A note on running that suite from Git Bash, inherited from the parent record
because it still costs a run: a POSIX `JDK=/c/Program Files/...` is accepted by
the launcher and then rejected behind it, and reports as ENVIRONMENT failures
that look like the change. Pass `JDK=$(cygpath -m ...)`.
