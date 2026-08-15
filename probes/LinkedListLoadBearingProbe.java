import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.Iterator;
import java.util.LinkedList;
import java.util.List;
import java.util.ListIterator;
import java.util.Objects;
import java.util.concurrent.Callable;

/**
 * The paths `LinkedList`'s iterator carrier is load-bearing for.
 *
 * <p>Three of these are named in `native-collections/src/lib.rs` itself as the
 * application failures that shaped the current design, and they are exactly
 * what a change of carrier has to keep working:
 *
 * <ul>
 *   <li>Kafka `ConfigDef.testGroupInference` — `assertEquals(ArrayList,
 *       LinkedList)`. Real `AbstractList.equals` bytecode iterating through
 *       `listIterator()`; when the carrier's cursor write was mangled, `next()`
 *       never advanced and EVERY LinkedList-vs-other-List comparison answered
 *       false.</li>
 *   <li>Spring `processDeferredImportSelectors` — `List.sort` on a LinkedList.
 *       The default method sorts an array and writes back through
 *       `ListIterator.set`, so a `set` that does not reach the node is a sort
 *       that does nothing.</li>
 *   <li>Mockito `DefaultRegisteredInvocations.getAll()` — `new
 *       LinkedList<>(collection)` then iterate; the copy looked empty when the
 *       overlay and the real fields disagreed.</li>
 * </ul>
 *
 * <p>Plus the `AbstractList` defaults that reach `listIterator()` on a foreign
 * receiver (`hashCode`, `indexOf`, `lastIndexOf`, `subList`), the drain loops
 * (`Iterator.remove`, `removeIf`, `retainAll`, `removeAll`), and the identity
 * rows. Diff against HotSpot JDK 25.
 */
public class LinkedListLoadBearingProbe {

    static LinkedList<String> fresh() {
        return new LinkedList<>(Arrays.asList("a", "b", "c", "d"));
    }

    static void row(String name, Callable<Object> c) {
        String out;
        try {
            out = String.valueOf(c.call());
        } catch (Throwable t) {
            out = "THREW " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println("ROW " + name + " = " + out);
    }

    public static void main(String[] args) {
        // ---- identity ------------------------------------------------------
        row("iterator.class", () -> fresh().iterator().getClass().getName());
        row("listIterator.class", () -> fresh().listIterator().getClass().getName());
        row("listIterator(2).class", () -> fresh().listIterator(2).getClass().getName());
        row("iterator.interfaces",
                () -> Arrays.toString(fresh().iterator().getClass().getInterfaces()));
        row("listIterator.interfaces",
                () -> Arrays.toString(fresh().listIterator().getClass().getInterfaces()));
        row("iterator IS listIterator class", () -> {
            LinkedList<String> l = fresh();
            return l.iterator().getClass() == l.listIterator().getClass();
        });
        row("List.iterator via interface",
                () -> ((List<String>) fresh()).iterator().getClass().getName());
        row("iterator instanceof ListIterator", () -> fresh().iterator() instanceof ListIterator);
        row("descendingIterator.class", () -> fresh().descendingIterator().getClass().getName());

        // ---- kafka ConfigDef: AbstractList.equals across layouts -----------
        row("ArrayList.equals(LinkedList)", () -> new ArrayList<>(Arrays.asList("a", "b", "c", "d")).equals(fresh()));
        row("LinkedList.equals(ArrayList)", () -> fresh().equals(new ArrayList<>(Arrays.asList("a", "b", "c", "d"))));
        row("ArrayList.equals(LinkedList) unequal", () -> new ArrayList<>(Arrays.asList("a", "z")).equals(fresh()));
        row("List.of.equals(LinkedList)", () -> List.of("a", "b", "c", "d").equals(fresh()));
        row("hashCode matches ArrayList",
                () -> fresh().hashCode() == new ArrayList<>(Arrays.asList("a", "b", "c", "d")).hashCode());
        row("indexOf", () -> fresh().indexOf("c"));
        row("lastIndexOf", () -> fresh().lastIndexOf("c"));
        row("contains", () -> fresh().contains("d"));

        // ---- Spring processDeferredImportSelectors: List.sort --------------
        row("List.sort natural", () -> {
            LinkedList<String> l = new LinkedList<>(Arrays.asList("d", "b", "a", "c"));
            l.sort(null);
            return l.toString();
        });
        row("List.sort comparator", () -> {
            LinkedList<String> l = new LinkedList<>(Arrays.asList("d", "b", "a", "c"));
            l.sort(Comparator.reverseOrder());
            return l.toString();
        });
        row("Collections.sort", () -> {
            LinkedList<String> l = new LinkedList<>(Arrays.asList("d", "b", "a", "c"));
            Collections.sort(l);
            return l.toString();
        });
        row("Collections.reverse", () -> {
            LinkedList<String> l = fresh();
            Collections.reverse(l);
            return l.toString();
        });
        row("replaceAll", () -> {
            LinkedList<String> l = fresh();
            l.replaceAll(String::toUpperCase);
            return l.toString();
        });

        // ---- Mockito: copy-construct then iterate --------------------------
        row("new LinkedList<>(coll) size", () -> new LinkedList<>(Arrays.asList("x", "y", "z")).size());
        row("new LinkedList<>(coll) drain", () -> {
            StringBuilder sb = new StringBuilder();
            for (String s : new LinkedList<>(Arrays.asList("x", "y", "z"))) {
                sb.append(s);
            }
            return sb.toString();
        });
        row("new ArrayList<>(LinkedList)", () -> new ArrayList<>(fresh()).toString());
        row("toArray", () -> Arrays.toString(fresh().toArray()));
        row("toArray(T[])", () -> Arrays.toString(fresh().toArray(new String[0])));
        row("stream.toList", () -> fresh().stream().toList().toString());

        // ---- drain loops ---------------------------------------------------
        row("Iterator.remove one", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            it.remove();
            return l + "/" + l.size();
        });
        row("Iterator.remove all", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            while (it.hasNext()) {
                it.next();
                it.remove();
            }
            return l + "/" + l.size();
        });
        row("Iterator.remove alternating", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            boolean drop = true;
            while (it.hasNext()) {
                it.next();
                if (drop) {
                    it.remove();
                }
                drop = !drop;
            }
            return l + "/" + l.size();
        });
        row("Iterator.remove before next", () -> {
            fresh().iterator().remove();
            return "NO THROW";
        });
        row("Iterator.remove twice", () -> {
            Iterator<String> it = fresh().iterator();
            it.next();
            it.remove();
            it.remove();
            return "NO THROW";
        });
        row("removeIf", () -> {
            LinkedList<String> l = fresh();
            l.removeIf(s -> s.compareTo("b") <= 0);
            return l + "/" + l.size();
        });
        row("retainAll", () -> {
            LinkedList<String> l = fresh();
            l.retainAll(Arrays.asList("b", "d"));
            return l + "/" + l.size();
        });
        row("removeAll", () -> {
            LinkedList<String> l = fresh();
            l.removeAll(Arrays.asList("b", "d"));
            return l + "/" + l.size();
        });
        row("clear then add", () -> {
            LinkedList<String> l = fresh();
            l.clear();
            l.add("only");
            return l + "/" + l.size();
        });

        // ---- ListIterator mutators ----------------------------------------
        row("listIterator.set", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.set("Z");
            return l.toString();
        });
        row("listIterator.set after previous", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator(2);
            it.previous();
            it.set("Z");
            return l.toString();
        });
        row("listIterator.set before next", () -> {
            fresh().listIterator().set("Z");
            return "NO THROW";
        });
        row("listIterator.add", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.add("X");
            return l + " next=" + it.next();
        });
        row("listIterator.add at end", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            while (it.hasNext()) {
                it.next();
            }
            it.add("tail");
            return l + " hasNext=" + it.hasNext();
        });
        row("listIterator.add then set", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.add("X");
            it.set("Y");
            return "NO THROW " + l;
        });
        row("listIterator.remove", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.next();
            it.remove();
            return l.toString();
        });
        row("listIterator.remove after previous", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator(2);
            it.previous();
            it.remove();
            return l + " nextIndex=" + it.nextIndex();
        });
        row("listIterator full walk", () -> {
            ListIterator<String> it = fresh().listIterator();
            StringBuilder sb = new StringBuilder();
            while (it.hasNext()) {
                sb.append(it.nextIndex()).append(it.next());
            }
            while (it.hasPrevious()) {
                sb.append(it.previousIndex()).append(it.previous());
            }
            return sb.toString();
        });
        row("listIterator.next past end", () -> {
            ListIterator<String> it = new LinkedList<>(List.of("a")).listIterator();
            it.next();
            it.next();
            return "NO THROW";
        });
        row("listIterator.previous past start", () -> {
            fresh().listIterator().previous();
            return "NO THROW";
        });
        row("listIterator(size)", () -> {
            ListIterator<String> it = fresh().listIterator(4);
            return it.hasNext() + "/" + it.hasPrevious() + "/" + it.nextIndex();
        });
        row("listIterator(bad index)", () -> fresh().listIterator(9));

        // ---- concurrent modification ---------------------------------------
        row("CME on add during iteration", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            l.add("late");
            it.next();
            return "NO CME";
        });
        row("CME on remove during iteration", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            l.remove("d");
            it.next();
            return "NO CME";
        });
        row("no CME via iterator.remove", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            it.remove();
            it.next();
            return "NO CME";
        });

        // ---- views and serialization ---------------------------------------
        row("subList", () -> fresh().subList(1, 3).toString());
        row("subList.iterator drain", () -> {
            StringBuilder sb = new StringBuilder();
            for (String s : fresh().subList(1, 3)) {
                sb.append(s);
            }
            return sb.toString();
        });
        row("unmodifiableList over LinkedList", () -> {
            StringBuilder sb = new StringBuilder();
            for (String s : Collections.unmodifiableList(fresh())) {
                sb.append(s);
            }
            return sb.toString();
        });
        row("Deque push/pop", () -> {
            LinkedList<String> l = fresh();
            l.push("head");
            return l.pop() + "/" + l.peek() + "/" + l.pollLast();
        });
        row("serialization roundtrip", () -> {
            try {
                ByteArrayOutputStream bo = new ByteArrayOutputStream();
                try (ObjectOutputStream oo = new ObjectOutputStream(bo)) {
                    oo.writeObject(fresh());
                }
                try (ObjectInputStream oi =
                        new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray()))) {
                    Object r = oi.readObject();
                    return r + "/" + ((List<?>) r).size();
                }
            } catch (Exception e) {
                return "THREW " + e.getClass().getName();
            }
        });
        row("equals after iterator.remove", () -> {
            LinkedList<String> l = fresh();
            Iterator<String> it = l.iterator();
            it.next();
            it.remove();
            return l.equals(new ArrayList<>(Arrays.asList("b", "c", "d")));
        });
        row("size after listIterator.remove", () -> {
            LinkedList<String> l = fresh();
            ListIterator<String> it = l.listIterator();
            it.next();
            it.remove();
            return l.size() + "/" + Objects.toString(l);
        });

        System.out.println("PROBE DONE");
    }
}
