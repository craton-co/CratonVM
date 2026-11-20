package cratonvm;

import java.util.AbstractMap;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.stream.Collectors;
import java.util.stream.Stream;

/**
 * Differential exercise program for the synthetic native overlay.
 *
 * CratonVM ships hand-written native implementations ("synthetic stubs") for a
 * number of java.util / java.lang methods. Some of those stubs historically
 * returned values that DIVERGE from what the real JDK bytecode produces:
 *   - java.util.Collections.disjoint  (returned wrong booleans)
 *   - java.util.stream.Collectors.toMap with a merge function
 *     (dropped / overwrote the colliding value instead of merging)
 *   - String.format("%s", x) for boxed args (returned null-on-boxed)
 *
 * This program exercises each of those methods plus a few control cases that
 * must ALWAYS agree, printing every observable result on its own line prefixed
 * "r:" exactly like IntrinsicDiff.java. The harness (synthetic_diff.rs) runs
 * this program twice — once through the synthetic overlay (CRATONVM_REAL unset)
 * and once forcing real JDK bytecode (CRATONVM_REAL=all) — and asserts the
 * full sequence of r: lines is byte-for-byte identical. A divergence means a
 * synthetic stub returned a different value than the real method.
 *
 * The final line is `SYNTHETIC_DIFF_OK <n>` where <n> is the number of r:
 * lines emitted, so the harness can confirm the program ran to completion.
 *
 * Plain JDK only, no third-party deps; compiled by the legacy pass of
 * vm/build.rs (no `// JAVA21+` marker).
 */
public class SyntheticDiff {

    /** Number of r: observation lines emitted so far. */
    private static int recCount = 0;

    /** Record one named observation: print it and bump the counter. */
    private static void rec(String label, String value) {
        System.out.println("r:" + label + "=" + value);
        recCount++;
    }

    private static void rec(String label, boolean value) {
        rec(label, value ? "true" : "false");
    }

    public static void main(String[] args) {
        collectionsDisjoint();
        collectorsToMapMerge();
        stringFormatPercentS();
        controlCases();

        System.out.println("SYNTHETIC_DIFF_OK " + recCount);
    }

    // -------------------------------------------------------------------
    // java.util.Collections.disjoint
    //   real: true iff the two collections share NO element.
    // -------------------------------------------------------------------
    private static void collectionsDisjoint() {
        Set<Integer> a = new HashSet<>(Arrays.asList(1, 2, 3));
        Set<Integer> b = new HashSet<>(Arrays.asList(4, 5, 6));
        Set<Integer> c = new HashSet<>(Arrays.asList(3, 7, 8)); // shares 3 with a
        Set<Integer> empty = new HashSet<>();
        Set<Integer> one = new HashSet<>(Arrays.asList(42));
        Set<Integer> one2 = new HashSet<>(Arrays.asList(42));
        Set<Integer> oneOther = new HashSet<>(Arrays.asList(99));

        // disjoint sets -> true
        rec("Collections.disjoint.disjoint", java.util.Collections.disjoint(a, b));
        // overlapping sets -> false
        rec("Collections.disjoint.overlap", java.util.Collections.disjoint(a, c));
        // one empty -> true (nothing in common)
        rec("Collections.disjoint.emptyLeft", java.util.Collections.disjoint(empty, a));
        rec("Collections.disjoint.emptyRight", java.util.Collections.disjoint(a, empty));
        rec("Collections.disjoint.bothEmpty", java.util.Collections.disjoint(empty, empty));
        // equal singletons -> false (they share the single element)
        rec("Collections.disjoint.equalSingletons", java.util.Collections.disjoint(one, one2));
        // distinct singletons -> true
        rec("Collections.disjoint.distinctSingletons", java.util.Collections.disjoint(one, oneOther));
        // self-disjoint of a non-empty set -> false
        rec("Collections.disjoint.self", java.util.Collections.disjoint(a, a));
    }

    // -------------------------------------------------------------------
    // java.util.stream.Collectors.toMap with a DUPLICATE key + merge fn.
    //   real: the merge function (Integer::sum) is applied to colliding
    //   values; a wrong stub historically dropped/overwrote or threw.
    // -------------------------------------------------------------------
    private static void collectorsToMapMerge() {
        // Keys "a" and "b" collide; values must be summed via Integer::sum.
        //   a: 1 + 10 + 100 = 111
        //   b: 2 + 20       = 22
        //   c: 3            = 3
        Stream<Map.Entry<String, Integer>> entries = Stream.of(
                new AbstractMap.SimpleEntry<>("a", 1),
                new AbstractMap.SimpleEntry<>("b", 2),
                new AbstractMap.SimpleEntry<>("a", 10),
                new AbstractMap.SimpleEntry<>("c", 3),
                new AbstractMap.SimpleEntry<>("b", 20),
                new AbstractMap.SimpleEntry<>("a", 100));

        Map<String, Integer> merged = entries.collect(Collectors.toMap(
                Map.Entry::getKey,
                Map.Entry::getValue,
                Integer::sum));

        // Sort into a TreeMap so toString() is deterministic regardless of the
        // (unspecified) iteration order of the map toMap produced.
        Map<String, Integer> sorted = new TreeMap<>(merged);

        rec("Collectors.toMap.merge.a", String.valueOf(merged.get("a")));
        rec("Collectors.toMap.merge.b", String.valueOf(merged.get("b")));
        rec("Collectors.toMap.merge.c", String.valueOf(merged.get("c")));
        rec("Collectors.toMap.merge.size", String.valueOf(merged.size()));
        rec("Collectors.toMap.merge.sorted", sorted.toString());

        // A second case using a non-commutative merge (keep first) so a stub
        // that silently overwrites is also caught.
        Map<String, Integer> firstWins = Stream.of(
                new AbstractMap.SimpleEntry<>("k", 1),
                new AbstractMap.SimpleEntry<>("k", 2),
                new AbstractMap.SimpleEntry<>("k", 3))
                .collect(Collectors.toMap(
                        Map.Entry::getKey,
                        Map.Entry::getValue,
                        (first, second) -> first,
                        LinkedHashMap::new));
        rec("Collectors.toMap.firstWins.k", String.valueOf(firstWins.get("k")));
        rec("Collectors.toMap.firstWins.size", String.valueOf(firstWins.size()));
    }

    // -------------------------------------------------------------------
    // String.format("%s", x)
    //   real: "7", "true", "null", the Long's decimal, the double's toString.
    //   wrong stub historically returned null when x was a boxed value.
    // -------------------------------------------------------------------
    private static void stringFormatPercentS() {
        Integer boxedInt = Integer.valueOf(7);
        Boolean boxedBool = Boolean.TRUE;
        Object nullObj = null;
        Long boxedLong = Long.valueOf(9000000000L);
        Double boxedDouble = Double.valueOf(3.5);

        rec("String.format.%s.int", String.format("%s", boxedInt));
        rec("String.format.%s.bool", String.format("%s", boxedBool));
        rec("String.format.%s.null", String.format("%s", nullObj));
        rec("String.format.%s.long", String.format("%s", boxedLong));
        rec("String.format.%s.double", String.format("%s", boxedDouble));
        // a plain String and a char argument too, for breadth.
        rec("String.format.%s.string", String.format("%s", "hello"));
        rec("String.format.%s.char", String.format("%s", Character.valueOf('Z')));
    }

    // -------------------------------------------------------------------
    // Control cases — these must ALWAYS agree (proves equality works and
    // the harness is not vacuously passing on an all-divergent program).
    // -------------------------------------------------------------------
    private static void controlCases() {
        rec("control.String.length", String.valueOf("abc".length()));
        rec("control.Math.abs", String.valueOf(Math.abs(-5)));
        rec("control.Integer.parseInt", String.valueOf(Integer.parseInt("42")));
        rec("control.String.concat", "x".concat("y"));
        rec("control.Boolean.parse", Boolean.parseBoolean("true"));
    }
}
