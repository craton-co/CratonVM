import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.ListIterator;

/**
 * Exercises ListIterator over CratonVM's unmodifiable-list views:
 *  - List.of(...).listIterator()                   (immutable snapshot)
 *  - Collections.unmodifiableList(...).listIterator(int)  (wrapper view)
 * Verifies forward/backward iteration, index tracking, and that the
 * mutating operations throw UnsupportedOperationException.
 */
public class ListIteratorTest {
    public static void main(String[] args) {
        // --- List.of(...).listIterator() ----------------------------------
        List<String> a = List.of("a", "b", "c");
        ListIterator<String> it = a.listIterator();
        StringBuilder fwd = new StringBuilder();
        while (it.hasNext()) {
            fwd.append(it.nextIndex()).append(':').append(it.next()).append(' ');
        }
        System.out.println("forward=" + fwd.toString().trim());

        StringBuilder bwd = new StringBuilder();
        while (it.hasPrevious()) {
            bwd.append(it.previousIndex()).append(':').append(it.previous()).append(' ');
        }
        System.out.println("backward=" + bwd.toString().trim());

        // --- Collections.unmodifiableList(...).listIterator(int) ----------
        List<String> u = Collections.unmodifiableList(
                new ArrayList<>(Arrays.asList("x", "y", "z", "w")));
        ListIterator<String> it2 = u.listIterator(1);
        System.out.println("at1.nextIndex=" + it2.nextIndex()
                + " previousIndex=" + it2.previousIndex());
        System.out.println("at1.next=" + it2.next());
        System.out.println("at1.next=" + it2.next());
        System.out.println("at1.previous=" + it2.previous());

        // --- mutators must throw ------------------------------------------
        ListIterator<String> it3 = a.listIterator();
        it3.next();
        System.out.println("set throws=" + throws3(it3, 0));
        System.out.println("remove throws=" + throws3(it3, 1));
        System.out.println("add throws=" + throws3(it3, 2));

        // --- enhanced-for over List.of (uses iterator()) ------------------
        StringBuilder ef = new StringBuilder();
        for (String s : List.of("p", "q")) {
            ef.append(s);
        }
        System.out.println("enhancedFor=" + ef);

        System.out.println("DONE");
    }

    /** op: 0=set, 1=remove, 2=add. Returns true iff UnsupportedOperationException. */
    static boolean throws3(ListIterator<String> it, int op) {
        try {
            switch (op) {
                case 0 -> it.set("Z");
                case 1 -> it.remove();
                case 2 -> it.add("Z");
            }
            return false;
        } catch (UnsupportedOperationException e) {
            return true;
        }
    }
}
