// H9-1 N4: pin `HashSet.PRESENT` identity in the corpus.
//
// H9-1-hashset-owns-no-state-20260820-RETIRED-20260921.md
//
// Real `HashSet.remove(Object)` is `map.remove(o) == PRESENT` -- an IDENTITY
// test against the one `static final Object PRESENT` instance `HashSet`
// declares. Before commit `11798a8a2`, several VM producers wrote a
// non-`PRESENT` marker (the element itself, or `Value::Object(None)`) into
// the backing map's value slot, so `remove()` deleted the element from the
// map (size dropped) while reporting `false` -- a leak that reports success,
// the opposite-polarity twin of a leak that reports failure. Lane H0's
// INDEPENDENT VERIFICATION section measured this directly on the pristine
// (pre-fix) binary: `new HashSet<>(Arrays.asList(...))` and
// `Collectors.toSet()` both scored `false` on a `remove()` that HotSpot scores
// `true`, while `Set.of`-copy, `keySet()` and
// `Collectors.toCollection(HashSet::new)` already carried the real marker.
// No vector asked this question before this file -- it existed only as a
// hand-run probe.
//
// Every producer below is one the VM's own native surface builds (not a
// user's own `HashSet` subclass), and each is exercised the same way: remove
// one present element through the REAL `HashSet.remove` bytecode inherited
// path (i.e. ordinary `Set.remove`, not a native override on a subclass), and
// check both the boolean return AND the resulting size -- the size alone
// cannot tell "the marker is right" from "the element was never in the set to
// begin with".

import java.util.Arrays;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class RHashSetPresentMarker {

    static int checks = 0;
    static int fails = 0;

    static void check(String name, boolean actual, boolean expected) {
        checks++;
        System.out.println("CK RHashSetPresentMarker " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RHashSetPresentMarker FAILED " + name + " expected=" + expected);
        }
    }

    static void checkInt(String name, int actual, int expected) {
        checks++;
        System.out.println("CK RHashSetPresentMarker " + name + "=" + actual);
        if (actual != expected) {
            fails++;
            System.out.println("CK RHashSetPresentMarker FAILED " + name + " expected=" + expected);
        }
    }

    // remove() must report true AND the size must drop by exactly one -- the
    // pairing the identity-marker defect breaks (element gone, `false`
    // reported) as well as the case nothing here should ever produce (element
    // still present, `true` reported, size unchanged).
    static void removeOne(String label, Set<String> s, String target) {
        int before = s.size();
        boolean removed = s.remove(target);
        int after = s.size();
        check(label + ".removed", removed, true);
        checkInt(label + ".sizeAfter", after, before - 1);
        check(label + ".goneFromContains", s.contains(target), false);
    }

    public static void main(String[] args) {
        removeOne("newHashSetFromList",
            new HashSet<>(Arrays.asList("a", "b", "c", "d")), "b");

        removeOne("newHashSetFromSetOf",
            new HashSet<>(Set.of("a", "b", "c", "d")), "b");

        removeOne("collectorsToSet",
            Stream.of("a", "b", "c", "d").collect(Collectors.toSet()), "b");

        removeOne("collectorsToCollectionHashSet",
            Stream.of("a", "b", "c", "d")
                .collect(Collectors.toCollection(HashSet::new)), "b");

        Map<String, Integer> m = new java.util.HashMap<>();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        m.put("d", 4);
        removeOne("mapKeySet", m.keySet(), "b");

        removeOne("hashSetAddLoop", buildByAdd("a", "b", "c", "d"), "b");

        // Four or more args routes `Set.of` through `build_hashset_from_args`
        // per H9-1 -- copying it into a fresh HashSet is the shape §1
        // describes as the "make_hashset_with_elements real-layout branch".
        removeOne("newHashSetFromFourArgSetOf",
            new HashSet<>(Set.of("a", "b", "c", "d", "e")), "b");

        System.out.println("CK RHashSetPresentMarker fails=" + fails);
        System.out.println("CK RHashSetPresentMarker checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " HashSet.PRESENT identity checks failed");
        }
        System.out.println("PASS RHashSetPresentMarker (" + checks + " checks)");
    }

    static HashSet<String> buildByAdd(String... items) {
        HashSet<String> s = new HashSet<>();
        for (String it : items) {
            s.add(it);
        }
        return s;
    }
}
