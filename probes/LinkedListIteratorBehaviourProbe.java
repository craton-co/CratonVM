import java.util.Arrays;
import java.util.Iterator;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;

/**
 * Behaviour, not just identity: what a `LinkedList` iterator's mutators actually
 * do.
 *
 * <p>`ListItrInterfaceProbe` covers this area and reads only `getClass()` and
 * `getInterfaces()`, which framed the gap as cosmetic. It is not: until
 * 2026-08-15 `listIterator().add` and `.remove` threw
 * `UnsupportedOperationException` where HotSpot writes through — the same
 * "iterators over a copy" defect `SubListBehaviourProbe` was written for, one
 * collection over, and enough to make every `AbstractList` default that drains
 * a `LinkedList` through its list-iterator throw.
 *
 * <p>Diff against HotSpot JDK 25. The rows that still differ are the carrier
 * identity (`LinkedList$ListItr` vs `cratonvm.internal.LinkedListSnapshotListItr`,
 * and `LinkedList$Itr` — a class the real JDK does not declare) and the missing
 * `ConcurrentModificationException`.
 */
public class LinkedListIteratorBehaviourProbe {

    static LinkedList<String> fresh() {
        return new LinkedList<>(Arrays.asList("a", "b", "c", "d"));
    }

    static void row(String name, java.util.concurrent.Callable<Object> c) {
        String out;
        try {
            Object r = c.call();
            out = String.valueOf(r);
        } catch (Throwable t) {
            out = "THREW " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println("ROW " + name + " = " + out);
    }

    public static void main(String[] args) {
        row("iterator.class", () -> fresh().iterator().getClass().getName());
        row("listIterator.class", () -> fresh().listIterator().getClass().getName());
        row("iterator.interfaces",
                () -> Arrays.toString(fresh().iterator().getClass().getInterfaces()));
        row("listIterator.interfaces",
                () -> Arrays.toString(fresh().listIterator().getClass().getInterfaces()));
        row("iterator IS listIterator class", () -> {
            LinkedList<String> l = fresh();
            return l.iterator().getClass() == l.listIterator().getClass();
        });

        row("iterator.remove writes through", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            it.remove();
            return l.toString() + "/" + l.size();
        });
        row("iterator.remove all", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            while (it.hasNext()) {
                it.next();
                it.remove();
            }
            return l.toString() + "/" + l.size();
        });
        row("iterator.remove before next", () -> {
            LinkedList<String> l = fresh();
            l.iterator().remove();
            return "NO THROW";
        });
        row("listIterator.set writes through", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.set("Z");
            return l.toString();
        });
        row("listIterator.add writes through", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.add("X");
            return l.toString() + " next=" + it.next();
        });
        row("listIterator.remove writes through", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.next();
            it.remove();
            return l.toString();
        });
        row("listIterator.previous", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator(2);
            return it.previous() + "," + it.previousIndex() + "," + it.nextIndex();
        });
        row("listIterator sees later add (live view)", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            l.add("late");
            try {
                it.next();
                return "NO CME";
            } catch (java.util.ConcurrentModificationException e) {
                return "CME";
            }
        });
        row("descendingIterator.class",
                () -> fresh().descendingIterator().getClass().getName());
        row("List.iterator via interface",
                () -> ((List<String>) fresh()).iterator().getClass().getName());
        System.out.println("PROBE DONE");
    }
}
