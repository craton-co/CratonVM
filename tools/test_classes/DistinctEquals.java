import java.util.*;
import java.util.stream.*;

/** Repro for SC-aot-runtimehints-resource-count: Stream.distinct() must dedup
 *  via the element's real equals()/hashCode(), not shallow identity. */
public class DistinctEquals {
    record K(String p) {}                  // equals/hashCode by field

    static final class V {                 // explicit equals/hashCode override
        final String s;
        V(String s) { this.s = s; }
        @Override public boolean equals(Object o) { return (o instanceof V) && ((V) o).s.equals(s); }
        @Override public int hashCode() { return s.hashCode(); }
        @Override public String toString() { return "V(" + s + ")"; }
    }

    static int pass = 0, fail = 0;
    static void check(String name, long got, long want) {
        boolean ok = got == want;
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + name + " => " + got + " (want " + want + ")");
    }

    public static void main(String[] a) {
        List<K> in = List.of(new K("/"), new K("com"), new K("/"), new K("x"));
        check("record distinct count", in.stream().distinct().count(), 3);

        List<V> inV = List.of(new V("/"), new V("com"), new V("/"), new V("x"), new V("com"));
        check("class distinct count", inV.stream().distinct().count(), 3);

        // distinct().toList() content + order preserved (first occurrence)
        List<V> out = inV.stream().distinct().collect(Collectors.toList());
        check("class distinct toList size", out.size(), 3);
        System.out.println("    distinct = " + out);

        // Strings still dedup (regression guard)
        check("string distinct count",
                Stream.of("a", "b", "a", "c", "b").distinct().count(), 3);

        // Integers (boxed) dedup (regression guard)
        check("integer distinct count",
                Stream.of(1, 2, 1, 3, 2, 1).distinct().count(), 3);

        System.out.println("RESULT pass=" + pass + " fail=" + fail);
    }
}
