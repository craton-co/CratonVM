import java.util.ArrayDeque;
import java.util.Deque;
import java.util.Iterator;

/**
 * The `ArrayDeque` for-each path, in isolation.
 *
 * In `--synthetic-jdk` mode a deque has no `iterator()` bytecode and no
 * registration, so a for-each resolves through the
 * `java/util/Collection.iterator()` interface native -- `native_al_iterator`,
 * which reads slot 0 as `elementData` and slot 1 (`head`) as `size`. The two
 * failure modes follow from that one misreading, and which one you get depends
 * on which end you pushed from.
 */
public class ArrayDequeIter {
    static int failures = 0;

    public static void main(String[] args) {
        check("addLast size", "2", run(false, "size"));
        check("addLast toString", "[p, q]", run(false, "toString"));
        check("addLast foreach", "[p, q]", run(false, "foreach"));
        check("addFirst size", "2", run(true, "size"));
        check("addFirst toString", "[x, y]", run(true, "toString"));
        check("addFirst foreach", "[x, y]", run(true, "foreach"));
        System.out.println(failures == 0 ? "PASS ArrayDequeIter"
                                         : "FAIL ArrayDequeIter (" + failures + ")");
        if (failures != 0) System.exit(1);
    }

    static String run(boolean front, String what) {
        try {
            Deque<String> d = new ArrayDeque<>();
            if (front) { d.addFirst("y"); d.addFirst("x"); }
            else       { d.addLast("p");  d.addLast("q");  }
            if (what.equals("size")) return String.valueOf(d.size());
            if (what.equals("toString")) return d.toString();
            StringBuilder sb = new StringBuilder("[");
            Iterator<String> it = d.iterator();
            boolean first = true;
            while (it.hasNext()) {
                if (!first) sb.append(", ");
                sb.append(it.next());
                first = false;
            }
            return sb.append("]").toString();
        } catch (Throwable t) {
            return "THREW " + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            System.out.println("  MISMATCH " + what + ": expected <" + expected + "> got <" + actual + ">");
            failures++;
        }
    }
}
