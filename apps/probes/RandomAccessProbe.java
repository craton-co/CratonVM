import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.LinkedList;
import java.util.List;
import java.util.RandomAccess;

/**
 * `instanceof` and `Class.isInstance` must answer the same question about the
 * same object, and both must agree with `getClass()`.
 *
 * The three doors disagreed for exactly one pair. `Collections.unmodifiableList`
 * has TWO display classes on HotSpot and this VM mints both from one internal
 * stamp, choosing between them per instance by asking whether the BACKING list
 * implements `RandomAccess`. `getClass()`'s authority resolved that interface to
 * a ClassId and walked the interface DAG; the opcodes' display arm asked the
 * same question through `is_subclass_of_by_name`, the exception-`catch_type`
 * fallback, which walks only the superclass chain and so can never match an
 * interface. It answered `false` for every list, so:
 *
 *   getClass()                          java.util.Collections$UnmodifiableRandomAccessList
 *   RandomAccess.class.isInstance(v)    true
 *   v instanceof RandomAccess           false      <- and `(RandomAccess) v` threw
 *
 * The `LinkedList` row is the control that makes the fix falsifiable: the
 * answer there must stay `false`, and a fix that simply admitted the marker
 * would turn it green too. `Collections.binarySearch`, `reverse`, `shuffle` and
 * `fill` all branch on this marker to choose indexed access over an iterator.
 */
public final class RandomAccessProbe {
    public static void main(String[] args) {
        row("unmodifiableList(ArrayList)", Collections.unmodifiableList(new ArrayList<>(List.of("a", "b"))));
        row("unmodifiableList(LinkedList)", Collections.unmodifiableList(new LinkedList<>(List.of("a", "b"))));
        row("unmodifiableList(Arrays.asList)", Collections.unmodifiableList(Arrays.asList("a", "b")));
        row("unmodifiableList(List.of)", Collections.unmodifiableList(List.of("a", "b")));
        row("unmodifiableCollection(ArrayList)", Collections.unmodifiableCollection(new ArrayList<>(List.of("a"))));
        // Controls: nothing below goes through the stamp.
        row("ArrayList", new ArrayList<>(List.of("a")));
        row("LinkedList", new LinkedList<>(List.of("a")));
        row("List.of", List.of("a", "b"));
        row("Arrays.asList", Arrays.asList("a", "b"));
        row("singletonList", Collections.singletonList("a"));
        row("emptyList", Collections.emptyList());
        row("subList", new ArrayList<>(List.of("a", "b", "c")).subList(0, 2));
    }

    static void row(String label, Object v) {
        boolean op = v instanceof RandomAccess;
        boolean refl = RandomAccess.class.isInstance(v);
        String cast;
        try {
            RandomAccess unused = (RandomAccess) v;
            cast = "ok";
        } catch (ClassCastException e) {
            cast = "CCE";
        }
        System.out.println(label
                + " | instanceof=" + op
                + " isInstance=" + refl
                + " checkcast=" + cast
                + " agree=" + (op == refl && (op == cast.equals("ok")))
                + " class=" + v.getClass().getName());
    }
}
