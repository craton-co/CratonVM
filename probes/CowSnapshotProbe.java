import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CopyOnWriteArraySet;

/**
 * A copy-on-write collection's iterator is a SNAPSHOT of the array as it stood
 * when the iterator was created. Its documented contract is that it never
 * throws {@code ConcurrentModificationException} and never reflects later
 * additions, removals or changes — which is precisely why callers are allowed
 * to mutate the collection while walking it.
 *
 * EhCache's `AbstractTreeNode.clean()` does exactly that:
 *
 * <pre>
 *   for (AbstractTreeNode child : getChildren())   // CopyOnWriteArraySet, wrapped
 *       removeChild(child);                        // mutates the backing set
 * </pre>
 *
 * On CratonVM that throws CME, which fails `Eh107CacheManager.close()` and
 * takes all 82 methods of `JCacheEhCacheApiTests` +
 * `JCacheEhCacheAnnotationTests` with it. Every row here is a contract fact,
 * identical on any correct JVM.
 */
public class CowSnapshotProbe {

    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    /** Walk `it`, mutating via `mutate` on the first step. */
    static String walkWhileMutating(Iterator<String> it, Runnable mutate) {
        try {
            int n = 0;
            while (it.hasNext()) {
                it.next();
                n++;
                if (n == 1) {
                    mutate.run();
                }
            }
            return "saw " + n;
        }
        catch (Throwable t) {
            return "THREW " + t.getClass().getSimpleName();
        }
    }

    static CopyOnWriteArraySet<String> set() {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        Collections.addAll(s, "a", "b", "c", "d");
        return s;
    }

    static CopyOnWriteArrayList<String> list() {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
        Collections.addAll(l, "a", "b", "c", "d");
        return l;
    }

    public static void main(String[] args) {
        // ---- S: CopyOnWriteArraySet, iterated directly --------------------
        CopyOnWriteArraySet<String> s1 = set();
        p("S01 remove during iteration", walkWhileMutating(s1.iterator(), () -> s1.remove("c")));
        CopyOnWriteArraySet<String> s2 = set();
        p("S02 add during iteration", walkWhileMutating(s2.iterator(), () -> s2.add("z")));
        CopyOnWriteArraySet<String> s3 = set();
        p("S03 clear during iteration", walkWhileMutating(s3.iterator(), s3::clear));
        CopyOnWriteArraySet<String> s4 = set();
        p("S04 removeAll during iteration",
                walkWhileMutating(s4.iterator(), () -> s4.removeAll(new ArrayList<>(s4))));

        // ---- U: the EXACT EhCache shape — wrapped in unmodifiableSet ------
        // `getChildren()` returns `Collections.unmodifiableSet(children)`, and
        // the caller removes from `children` itself while walking the wrapper.
        CopyOnWriteArraySet<String> backing = set();
        Set<String> view = Collections.unmodifiableSet(backing);
        p("U01 remove from BACKING while walking the VIEW",
                walkWhileMutating(view.iterator(), () -> backing.remove("c")));
        CopyOnWriteArraySet<String> backing2 = set();
        Set<String> view2 = Collections.unmodifiableSet(backing2);
        p("U02 remove EVERY element while walking the view",
                walkWhileMutating(view2.iterator(), () -> {
                    for (String e : new ArrayList<>(backing2)) {
                        backing2.remove(e);
                    }
                }));
        p("U03 backing emptied", backing2.size());

        // ---- L: the same contract on CopyOnWriteArrayList -----------------
        CopyOnWriteArrayList<String> l1 = list();
        p("L01 remove during iteration", walkWhileMutating(l1.iterator(), () -> l1.remove("c")));
        CopyOnWriteArrayList<String> l2 = list();
        p("L02 view: remove from backing",
                walkWhileMutating(Collections.unmodifiableList(l2).iterator(), () -> l2.remove("c")));
        CopyOnWriteArrayList<String> l3 = list();
        p("L03 set() during iteration", walkWhileMutating(l3.iterator(), () -> l3.set(2, "Z")));
        // The COW LIST iterator's own remove() contract — the property a set
        // iterator built by delegating to a COW list would inherit.
        CopyOnWriteArrayList<String> l4 = list();
        Iterator<String> li = l4.iterator();
        li.next();
        try {
            li.remove();
            p("L04 COW list iterator.remove()", "no-throw");
        }
        catch (Throwable t) {
            p("L04 COW list iterator.remove()", "THREW " + t.getClass().getSimpleName());
        }

        // ---- N: the snapshot must NOT see later changes -------------------
        // Guarded like every other block: an unguarded row that throws takes
        // the REST of the probe with it, and the rows it skips are the
        // CONTROLS — so the run would report a fix it never tested.
        CopyOnWriteArraySet<String> s5 = set();
        try {
            Iterator<String> snap = s5.iterator();
            s5.clear();
            s5.add("late");
            int n = 0;
            while (snap.hasNext()) {
                snap.next();
                n++;
            }
            p("N01 snapshot still yields the original 4", n);
        }
        catch (Throwable t) {
            p("N01 snapshot still yields the original 4", "THREW " + t.getClass().getSimpleName());
        }
        p("N02 collection itself now holds 1", s5.size());

        // ---- R: iterator.remove() is UNSUPPORTED on a snapshot ------------
        CopyOnWriteArraySet<String> s6 = set();
        Iterator<String> ri = s6.iterator();
        ri.next();
        try {
            ri.remove();
            p("R01 iterator.remove()", "no-throw");
        }
        catch (Throwable t) {
            p("R01 iterator.remove()", "THREW " + t.getClass().getSimpleName());
        }

        // ---- C: the CONTROL — a plain HashSet MUST still throw ------------
        // A VM that simply stopped raising CME everywhere would pass every row
        // above and still be wrong.
        Set<String> plain = new java.util.HashSet<>();
        Collections.addAll(plain, "a", "b", "c", "d");
        p("C01 HashSet remove during iteration MUST throw",
                walkWhileMutating(plain.iterator(), () -> plain.remove("c")));
        List<String> plainList = new ArrayList<>(List.of("a", "b", "c", "d"));
        p("C02 ArrayList remove during iteration MUST throw",
                walkWhileMutating(plainList.iterator(), () -> plainList.remove("c")));

        // ---- M: INFORMATIONAL, not a contract row --------------------------
        // ConcurrentHashMap's views are WEAKLY CONSISTENT: they may or may not
        // reflect a removal made after the iterator was created. `saw 3` and
        // `saw 4` are both legal, so a difference here is NOT a defect and must
        // not be "fixed". Kept because a reader who sees it diverge should be
        // told that in the probe rather than go hunting.
        Map<String, String> chm = new ConcurrentHashMap<>();
        for (String k : new String[] {"a", "b", "c", "d"}) {
            chm.put(k, k);
        }
        p("M01 CHM keySet remove during iteration (either answer is legal)",
                walkWhileMutating(chm.keySet().iterator(), () -> chm.remove("c")));
    }
}
