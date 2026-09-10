import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.ListIterator;
import java.util.Set;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CopyOnWriteArraySet;

/**
 * L5 -- `CopyOnWriteArrayList` / `CopyOnWriteArraySet` contract edges.
 *
 * These two carry 38 of lane 5's shadows and are the lane's cheapest wave:
 * plain value semantics over an array snapshot, no VM-filled state. What makes
 * them worth a probe anyway is that the interesting behaviour is all at the
 * perimeter, exactly as the ops page predicts -- of the first 48 defects found
 * this way, not one was a wrong answer to an ordinary call.
 *
 * The three rules a hand-written shim gets wrong here:
 *
 *   1. **snapshot isolation** -- an iterator obtained BEFORE a mutation must
 *      not see it, and must not throw `ConcurrentModificationException`
 *      either. A shim that iterates the live array does both wrong at once;
 *   2. **the mutating iterator methods are refusals**, not no-ops:
 *      `remove`/`set`/`add` on the iterator throw `UnsupportedOperationException`;
 *   3. `addIfAbsent` / `addAllAbsent` decide by `equals`, and the SET's `add`
 *      likewise -- while its `equals` is `Set` equality, order-independent,
 *      against any `Set`.
 *
 * Nothing here prints an identity hash, a capacity, an address, or an
 * iteration order that either VM may choose independently: a
 * `CopyOnWriteArraySet`'s order IS specified (insertion order over the
 * snapshot array), which is why it may be printed.
 */
public class L5CowSweep {
    static int rows;

    static void say(String label, Object value) {
        rows++;
        System.out.println(label + " " + value);
    }

    /** Name of the throwable, plus whether it carried a message at all. */
    static String thrown(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? " msg=null" : " msg=yes");
        }
    }

    static void listBasics() {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
        say("emptyNew", l + " size=" + l.size() + " empty=" + l.isEmpty());
        say("emptyIteratorHasNext", l.iterator().hasNext());
        l.addAll(Arrays.asList("a", "b", "c"));
        say("afterAddAll", l.toString());
        say("get", l.get(0) + l.get(2));
        say("indexOf", l.indexOf("b") + "/" + l.indexOf("z"));
        say("lastIndexOf", l.lastIndexOf("b") + "/" + l.lastIndexOf("z"));
        say("contains", l.contains("b") + "/" + l.contains("z"));
        say("containsAll", l.containsAll(Arrays.asList("a", "c")) + "/"
                + l.containsAll(Arrays.asList("a", "z")));
        say("set", l.set(1, "B") + " -> " + l);
        say("addAt", "" + l);
        l.add(1, "x");
        say("afterAddAt", l.toString());
        say("removeObj", l.remove("x") + " -> " + l);
        say("removeIdx", l.remove(0) + " -> " + l);
        say("addIfAbsentNew", l.addIfAbsent("q") + " -> " + l);
        say("addIfAbsentDup", l.addIfAbsent("q") + " -> " + l);
        say("addAllAbsent", l.addAllAbsent(Arrays.asList("q", "r", "r", "s")) + " -> " + l);
        say("equalsSelfCopy", l.equals(new CopyOnWriteArrayList<>(l)));
        say("equalsArrayList", l.equals(new ArrayList<>(l)));
        say("hashCodeMatchesList", l.hashCode() == new ArrayList<>(l).hashCode());
        say("subList", l.subList(0, 2).toString());
        say("toArray", Arrays.toString(l.toArray()));
        say("toArrayShort", Arrays.toString(l.toArray(new String[0])));
        String[] big = new String[l.size() + 2];
        Arrays.fill(big, "F");
        say("toArrayLongTailNulled", Arrays.toString(l.toArray(big)));
        say("clone", l.clone().toString());
        say("streamCount", l.stream().count());
        say("sorted", sortedCopy(l));
        say("replaceAll", replaced(l));
        say("removeIf", removedIf(l));
    }

    static String sortedCopy(CopyOnWriteArrayList<String> src) {
        CopyOnWriteArrayList<String> c = new CopyOnWriteArrayList<>(src);
        c.sort(Collections.reverseOrder());
        return c.toString();
    }

    static String replaced(CopyOnWriteArrayList<String> src) {
        CopyOnWriteArrayList<String> c = new CopyOnWriteArrayList<>(src);
        c.replaceAll(s -> s + "!");
        return c.toString();
    }

    static String removedIf(CopyOnWriteArrayList<String> src) {
        CopyOnWriteArrayList<String> c = new CopyOnWriteArrayList<>(src);
        boolean changed = c.removeIf(s -> s.startsWith("q"));
        return changed + " -> " + c;
    }

    static void listSnapshot() {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>(Arrays.asList("a", "b", "c"));
        Iterator<String> before = l.iterator();
        l.add("d");
        l.remove("a");
        StringBuilder seen = new StringBuilder();
        while (before.hasNext()) {
            seen.append(before.next());
        }
        say("iteratorSnapshotIsolated", seen + " liveNow=" + l);

        Iterator<String> mid = l.iterator();
        mid.next();
        l.clear();
        StringBuilder rest = new StringBuilder();
        while (mid.hasNext()) {
            rest.append(mid.next());
        }
        say("iteratorSurvivesClear", rest.toString());

        CopyOnWriteArrayList<String> m = new CopyOnWriteArrayList<>(Arrays.asList("a", "b"));
        Iterator<String> it = m.iterator();
        it.next();
        say("iteratorRemoveRefuses", thrown(it::remove));
        ListIterator<String> li = m.listIterator();
        li.next();
        say("listIteratorSetRefuses", thrown(() -> li.set("z")));
        say("listIteratorAddRefuses", thrown(() -> li.add("z")));
        ListIterator<String> back = m.listIterator(m.size());
        say("listIteratorBackwards", back.previous() + back.previous()
                + " hasPrevious=" + back.hasPrevious());

        Iterator<String> spent = new CopyOnWriteArrayList<String>().iterator();
        say("emptyIteratorNext", thrown(spent::next));
    }

    static void listRefusals() {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>(Arrays.asList("a", "b"));
        say("getNegative", thrown(() -> l.get(-1)));
        say("getPastEnd", thrown(() -> l.get(2)));
        say("setPastEnd", thrown(() -> l.set(2, "z")));
        say("addAtPastEnd", thrown(() -> l.add(3, "z")));
        say("removeNegative", thrown(() -> l.remove(-1)));
        say("subListBadRange", thrown(() -> l.subList(1, 0)));
        say("subListPastEnd", thrown(() -> l.subList(0, 9)));
        say("nullAccepted", l.add(null) + " -> " + l + " containsNull=" + l.contains(null));
        say("indexOfNull", l.indexOf(null));
        say("ctorNullCollection", thrown(() -> new CopyOnWriteArrayList<String>((List<String>) null)));
        say("addAllNull", thrown(() -> l.addAll(null)));
        say("removeIfNull", thrown(() -> l.removeIf(null)));
        say("forEachNull", thrown(() -> l.forEach(null)));
        say("sortNullComparatorOk", sortNull());
    }

    static String sortNull() {
        try {
            CopyOnWriteArrayList<String> c =
                    new CopyOnWriteArrayList<>(Arrays.asList("b", "a"));
            c.sort(null);
            return c.toString();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void setBasics() {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        say("setEmpty", s + " size=" + s.size() + " empty=" + s.isEmpty());
        say("setAddNew", "" + s.add("a") + s.add("b") + s.add("a") + " -> " + s);
        say("setAddAll", s.addAll(Arrays.asList("b", "c", "c")) + " -> " + s);
        say("setInsertionOrder", s.toString());
        say("setRemove", s.remove("b") + "/" + s.remove("zz") + " -> " + s);
        say("setContains", s.contains("a") + "/" + s.contains("b"));
        say("setEqualsLinkedHashSet", s.equals(new LinkedHashSet<>(s)));
        Set<String> reversed = new LinkedHashSet<>();
        List<String> tmp = new ArrayList<>(s);
        Collections.reverse(tmp);
        reversed.addAll(tmp);
        say("setEqualsIsOrderFree", s.equals(reversed));
        say("setHashCodeIsSetHash", s.hashCode() == new LinkedHashSet<>(s).hashCode());
        say("setEqualsList", s.equals(new ArrayList<>(s)));
        say("setToArray", Arrays.toString(s.toArray()));
        say("setNullAccepted", s.add(null) + " -> " + s);
        say("setRetainAll", retain(s));
        say("setRemoveAll", removeAll(s));
        CopyOnWriteArraySet<String> snap = new CopyOnWriteArraySet<>(Arrays.asList("a", "b"));
        Iterator<String> it = snap.iterator();
        snap.add("c");
        StringBuilder sb = new StringBuilder();
        while (it.hasNext()) {
            sb.append(it.next());
        }
        say("setIteratorSnapshotIsolated", sb + " liveNow=" + snap);
        Iterator<String> it2 = snap.iterator();
        it2.next();
        say("setIteratorRemoveRefuses", thrown(it2::remove));
        say("setCtorNull", thrown(() -> new CopyOnWriteArraySet<String>(null)));
        say("setAddAllNull", thrown(() -> snap.addAll(null)));
    }

    static String retain(CopyOnWriteArraySet<String> src) {
        CopyOnWriteArraySet<String> c = new CopyOnWriteArraySet<>(src);
        boolean changed = c.retainAll(Arrays.asList("a"));
        return changed + " -> " + c;
    }

    static String removeAll(CopyOnWriteArraySet<String> src) {
        CopyOnWriteArraySet<String> c = new CopyOnWriteArraySet<>(src);
        boolean changed = c.removeAll(Arrays.asList("a"));
        return changed + " -> " + c;
    }

    public static void main(String[] args) {
        listBasics();
        listSnapshot();
        listRefusals();
        setBasics();
        System.out.println("rows " + rows);
        System.out.println("DONE L5CowSweep");
    }
}
