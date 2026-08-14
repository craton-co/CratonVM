# `arrayList.subList(a, b).getClass()` is `cratonvm.internal.ArrayListSubList`

**Status:** OPEN (2026-08-13). One row. It is what is left of
[the collection-view carrier residuals](../internal/fixed-suite-bugs/netty/collection-view-carrier-residuals-FIXED-20260813.md),
whose other eleven rows closed the same day — that record's *"the one row that
stayed open"* section carries the full measurement and this page is the short
form.

| expression | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `arrayList.subList(0, 2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` |

`probes/ViewClassProbe` is otherwise byte-identical to HotSpot (29 of 30 lines),
and `probes/MapViewBehaviourProbe` is 194/194. Nothing about the view's
BEHAVIOUR diverges — reads, `set` write-through, nested `subList`, structural
mutation through the view and the comodification checks all match, on both
`--real-jdk` and `--jdk-only`. This is the class name and only the class name.

## It is not the layout, and the layout half is already done

The long-standing explanation — a sublist view "has its own native field layout"
and therefore cannot wear the real class — is wrong, and was retired with the
rest of that page: the JDK class declares `root`/`parent`/`offset`/`size` on top
of `AbstractList.modCount`, and this VM's five fields sit past them
(`asl_base`), exactly as a `values()` view's sit past `HashMap$Values.this$0`.
Built that way, `--real-jdk` measures byte-clean.

## What blocks it is receiver ownership, in `--jdk-only`

Registering the `native_asl_*` family on `java/util/ArrayList$SubList` is what
makes a view of that class work — and it also hands the family every `SubList`
**java.base's own bytecode** built. `--jdk-only` builds exactly those, because
there `ArrayList.subList` runs its own body. Those objects have the class's
declared width and none of this VM's fields, so `asl_base` lands on 0,
`asl_state` reads `modCount` as `parent`, and every native answers empty:

```text
sublist.mid         HotSpot [b|c]/2   --jdk-only []/0
sublist.size        3                 0
sublist.of-sublist  [b|c]/2           null
Pattern.split       1|2|3             null|null|null
String.split        6|7               null|null
```

Mirroring this VM's state into the JDK's five fields was tried and changed none
of those 14 half-lines: it fixes the JDK's bodies on OUR objects, not ours on
THEIRS.

## The fix, when someone takes it

A receiver test, not another carrier:

* `asl_base` answers `None` when
  `object_num_fields < class_num_total_fields + ASL_NUM_FIELDS`;
* every `native_asl_*` delegates through `invoke_virtual_bytecode_only` on that
  answer instead of reporting zero;
* then re-add `java/util/ArrayList$SubList` to `register_al_sublist_natives`,
  to both force-native gates, and to `class_manager`'s `jdk_superclass` /
  `jdk_interfaces` arms.

`ArrayList.subList` is on a path every Java program touches, and the failure
mode here is a SILENT empty list, so this wants its own change with its own
`--jdk-only` run rather than a rider on something else. `ASL_REAL_CLASS` in
`native-collections/src/lib.rs` carries the same note beside the code.

## Repro

```bash
javac -d . probes/ViewClassProbe.java probes/JdkOnlyCollectionViewProbe.java
java ViewClassProbe > hs.txt
cratonvm --java-home <jdk25> -cp . ViewClassProbe | diff hs.txt -

# the arm that must stay clean through any retry
java JdkOnlyCollectionViewProbe > hs-jo.txt
for m in --real-jdk --jdk-only; do
  cratonvm $m --java-home <jdk25> -cp . JdkOnlyCollectionViewProbe \
    | grep -v '^\[cratonvm\]' | diff hs-jo.txt -
done
```
