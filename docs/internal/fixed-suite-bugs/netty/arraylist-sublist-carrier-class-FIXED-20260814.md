# FIXED — `arrayList.subList()` wears `java.util.ArrayList$SubList`, behind a receiver-ownership test

**Status:** ✅ FIXED 2026-08-14. Retires
`docs/known-issues/arraylist-sublist-carrier-class-20260813.md`, whose analysis
was right and whose prescription worked as written. The residual it did *not*
predict — the view's ITERATOR mutators — is re-filed as
[sublist iterator mutators](../../../known-issues/sublist-iterator-mutators-20260814.md).

| expression | HotSpot JDK 25 | before | after |
| --- | --- | --- | --- |
| `arrayList.subList(0, 2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` | ✅ |

`probes/ViewClassProbe` is now **30 of 30 lines** — byte-identical to HotSpot —
on both `--real-jdk` and `--jdk-only`.

## The prescription, and the measurement that says it was necessary

The page prescribed a receiver test rather than another carrier: `asl_base`
answering `None` when `object_num_fields < class_num_total_fields +
ASL_NUM_FIELDS`, and every `native_asl_*` delegating through
`invoke_virtual_bytecode_only` on that answer. That is `asl_base_checked` and
`asl_delegate_foreign`, and it is the whole difference between a shippable
carrier and the one withdrawn on 2026-08-13.

**Negative control** — the same carrier built with `asl_base_checked` forced to
answer `Some`, i.e. exactly the previous attempt:

| probe / arm | with the test | control, no test |
| --- | ---: | ---: |
| `JdkOnlyCollectionViewProbe`, `--jdk-only` | 0 | **16** |
| `SubListBehaviourProbe`, `--jdk-only` | 0 | **112** |
| `JdkOnlyCollectionViewProbe`, `--real-jdk` | 0 | 0 |
| `SubListBehaviourProbe`, `--real-jdk` | 14 | 12 |

The `--real-jdk` column is why the previous attempt looked finished: it cannot
see this at all. The second oracle is the entire signal.

Two halves, and both are needed:

* **Ours wearing their class.** `alloc_asl_view` mints under
  `ArrayList$SubList` at `class_num_total_fields + ASL_NUM_FIELDS` slots, with
  the §5 policy gate asked FIRST and asked about the INTERNAL name — a sublist
  view is a compatibility stand-in whatever class it wears, so `--jdk-only`
  must go on refusing it. The five native fields sit past the JDK's declared
  `root`/`parent`/`offset`/`size`/`modCount`.
* **Theirs reaching our natives.** `asl_base_checked` answers `None` for any
  receiver narrower than that, and `asl_delegate_foreign` — one guard at the top
  of the eight hand-written natives and at the top of both shared delegates,
  which is every registration — hands it to `invoke_virtual_bytecode_only`.

## The probe this needed, and the seven defects it found

`probes/JdkOnlyCollectionViewProbe` covered six sublist lines and
`probes/MapViewBehaviourProbe` covers **none** — it is about map views. So the
retired page's claim that *"nothing about the view's BEHAVIOUR diverges — reads,
`set` write-through, nested `subList`, structural mutation through the view and
the comodification checks all match"* rested on six lines. It is not true.

`probes/SubListBehaviourProbe` (new, 90 lines against the host JDK) says what
actually diverged, all of it pre-existing and none of it about the class name:

| row | before | after |
| --- | --- | --- |
| `write.replaceAll` | silent no-op | ✅ |
| `write.fill` (`Collections.fill`) | silent no-op | ✅ |
| `write.swap` (`Collections.swap`) | silent no-op | ✅ |
| `struct.removeAll` | `AbstractMethodError` | ✅ |
| `struct.retainAll` | `AbstractMethodError` | ✅ |
| `struct.addAllAt` (`addAll(int, c)`) | `AbstractMethodError` | ✅ |
| `read.class` / `nested.class` | wrong carrier | ✅ |
| `iter.*` (7 rows) | UOE / silent no-op | re-filed |

**`Collections.fill`/`swap` were the mode-independent ones.** Both natives read
the receiver through `al_state` — the *ArrayList* layout — and a sublist view, a
`LinkedList`, or any application `List` answers `(None, 0)` there, so the loop
swapped nothing and reported success. That failed on `--jdk-only` too, because
this native is reached whatever runs `Collections.swap`. Both now fall back to
the receiver's own `get`/`set`, which is what the JDK's body does.

**A carrier is only as complete as the surface registered on it.** The
`AbstractMethodError` rows were already broken through an interface-level
native; the carrier made them worse, reaching the JDK's real `SubList` bodies
and NPE-ing on the null `root` this VM never fills. `removeAll`, `retainAll`,
`replaceAll`, `addAll(int,·)`, `parallelStream` and the seven `SequencedCollection`
methods are now registered on both carriers and named in both force-native
gates. That is the same lesson `register_set_view_carrier_natives` records —
here it arrived as a defect first.

## What stayed open

Seven rows, all sublist iterator MUTATION, all pre-existing and independent of
the carrier: `Iterator.remove()` raises `UnsupportedOperationException` where
HotSpot removes, and `ListIterator.set`/`remove`/`add` are **silent no-ops**.
`native_asl_iterator` hands back a snapshot `Arrays$ArrayItr` and `listIterator`
a snapshot `ArrayList$ListItr`; neither writes back. Closing it needs a live
iterator over the view — see the re-filed page, which carries the design and the
existing machinery (`snapshot_itr_backing_table`) it should reuse.

## Measured

Azure host 2, Linux, `origin/dev` `c017029b3`. Every probe diffed against the
HotSpot JDK 25 run of the same probe in the same directory, **stdout only** —
merging the VM's stderr tracing makes a clean run read as hundreds of diverging
lines, which is how the first baseline here was misread.

| probe | arm | before | after |
| --- | --- | ---: | ---: |
| `ViewClassProbe` (30 lines) | `--real-jdk` | 2 | **0** |
| `ViewClassProbe` | `--jdk-only` | 0 | **0** |
| `MapViewBehaviourProbe` (194) | both | 0 | **0** |
| `JdkOnlyCollectionViewProbe` (38) | both | 0 | **0** |
| `SubListBehaviourProbe` (90) | `--real-jdk` | 18 | **14** — the 7 iterator rows |
| `SubListBehaviourProbe` | `--jdk-only` | 4 | **0** |

## Repro

```bash
javac -d /tmp/p probes/ViewClassProbe.java probes/MapViewBehaviourProbe.java \
      probes/JdkOnlyCollectionViewProbe.java probes/SubListBehaviourProbe.java
for p in ViewClassProbe MapViewBehaviourProbe JdkOnlyCollectionViewProbe SubListBehaviourProbe; do
  java -cp /tmp/p $p > /tmp/hs-$p.txt 2>/dev/null
  for m in --real-jdk --jdk-only; do
    cratonvm $m --java-home <jdk25> -cp /tmp/p $p 2>/dev/null \
      | grep -v '^\[cratonvm\]' | diff /tmp/hs-$p.txt -
  done
done
```
