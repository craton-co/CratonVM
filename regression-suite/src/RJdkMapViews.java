import java.util.ArrayList;
import java.util.Collection;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.NoSuchElementException;

/**
 * JDK-only corpus: the two collection paths whose native implementations own
 * state that real {@code java.util} bytecode can also write.
 *
 * <p><b>Why this vector is not covered by {@code RJdkCollections}.</b> That
 * vector exercises a list, a map, an iterator and a stream, and every one of
 * those calls lands on a native that owns the whole operation. The two shapes
 * here are different in kind: each is a place where a CratonVM native and real
 * JDK bytecode both claim the same state, and the divergence only appears when
 * the workload crosses between them. A probe that stays on one side measures
 * its own reach and reads as a pass — the lesson
 * {@code P2-COLLECTIONS-SHADOWS-20260812.md} §8.2 paid for with a vacuous
 * {@code ArrayDeque} control.
 *
 * <p><b>Section 1 — {@code Map.values()} is an {@code AbstractCollection}.</b>
 * On a real JVM {@code values()} returns {@code HashMap$Values} /
 * {@code LinkedHashMap$LinkedValues} / {@code TreeMap$Values}, all of which
 * extend {@code java.util.AbstractCollection} and therefore override
 * <em>neither</em> {@code equals} nor {@code hashCode}. Both are identity
 * operations. CratonVM hands back an {@code ArrayList}-shaped carrier, whose
 * {@code AbstractList} contract is content-based — so without the
 * {@code al_is_values_view} arms in {@code native_al_equals} /
 * {@code native_al_hash_code}, two unrelated maps' value collections compare
 * EQUAL and a view compares equal to a plain list of the same elements.
 * Contrast {@code keySet()}, whose real class extends {@code AbstractSet},
 * which DOES override both — the content answer is correct there, and this
 * vector asserts that too so a fix cannot over-reach.
 *
 * <p><b>Section 2 — {@code LinkedList} methods that real bytecode mutates.</b>
 * {@code ll_get} reads a process-global overlay FIRST and falls back to the
 * real {@code first}/{@code last}/{@code size} only on a miss, while
 * {@code ll_set} mirrors only overlay-to-heap. Every natively constructed list
 * has an overlay from its constructor, so any real-bytecode write to those
 * fields is invisible to every native reader for the rest of the object's
 * life. Four public methods used to run exactly that bytecode:
 * {@code pollFirst()} and {@code pollLast()} (real {@code unlinkFirst} /
 * {@code unlinkLast}), {@code addAll(int, Collection)}, and
 * {@code descendingIterator()} — whose bytecode constructs a real node-live
 * {@code LinkedList$ListItr} directly rather than going through the registered
 * {@code listIterator(int)}. Each block below MUTATES through one of those and
 * then READS back through a native, which is the only ordering that can see
 * the disagreement.
 *
 * <p>Determinism: every printed container is insertion-ordered, and no identity
 * hash is ever printed — only the boolean that says it is one.
 */
public class RJdkMapViews {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    private static Map<String, String> lhm(String... kv) {
        Map<String, String> m = new LinkedHashMap<>();
        for (int i = 0; i < kv.length; i += 2) {
            m.put(kv[i], kv[i + 1]);
        }
        return m;
    }

    // ---------------------------------------------------------------- 1a
    // AbstractCollection has no equals/hashCode override.
    static void valuesIdentitySemantics() {
        Map<String, String> m1 = lhm("a", "1", "b", "2", "c", "3");
        Map<String, String> m2 = lhm("a", "1", "b", "2", "c", "3");
        Collection<String> v1 = m1.values();
        Collection<String> v2 = m2.values();

        check(v1.equals(v1), "a values view equals ITSELF (Object.equals identity)");
        check(!v1.equals(v2),
                "two content-equal values views must NOT be equal: AbstractCollection "
                        + "does not override equals");
        check(!v2.equals(v1), "…and not in the other direction either");

        List<String> copy = new ArrayList<>(v1);
        check(copy.size() == 3, "the copy really does hold the same elements");
        check(!v1.equals(copy), "a values view is never equal to a List of the same elements");
        check(!copy.equals(v1), "…nor a List to the view: a view is not a List");

        check(v1.hashCode() == System.identityHashCode(v1),
                "a values view hashes by IDENTITY (Object.hashCode)");
        check(v1.hashCode() == v1.hashCode(), "and that hash is stable across calls");

        // Empty views are the sharpest case: content equality would make every
        // empty view equal to every other and to an empty list.
        Collection<String> e1 = lhm().values();
        Collection<String> e2 = lhm().values();
        check(e1.isEmpty(), "an empty map has an empty values view");
        check(!e1.equals(e2), "two empty values views are still not equal");
        check(!e1.equals(new ArrayList<String>()), "nor is an empty view equal to an empty list");

        // The contrast that keeps the fix from over-reaching: keySet()'s real
        // class extends AbstractSet, which DOES override equals/hashCode.
        check(m1.keySet().equals(m2.keySet()), "keySet IS content-equal (AbstractSet)");
        check(m1.keySet().hashCode() == m2.keySet().hashCode(), "…and content-hashed");
        check(m1.entrySet().equals(m2.entrySet()), "entrySet IS content-equal (AbstractSet)");

        System.out.println("CK RJdkMapViews valuesIdentity ok");
    }

    // ---------------------------------------------------------------- 1b
    // The view is LIVE in both directions. Asserted here so the identity fix
    // above cannot be landed by quietly turning the view into a dead snapshot.
    static void valuesLiveness() {
        Map<String, String> m = lhm("a", "1", "b", "2", "c", "3");
        Collection<String> v = m.values();
        check(v.size() == 3, "view size at capture");
        check(v.toString().equals("[1, 2, 3]"), "view content at capture");

        m.put("d", "4");
        check(v.size() == 4, "map -> view: a later put is visible through the view");
        check(v.contains("4"), "map -> view: contains sees the new value");
        m.remove("a");
        check(v.size() == 3, "map -> view: a later remove is visible");
        check(!v.contains("1"), "map -> view: contains no longer sees the removed value");
        check(v.toString().equals("[2, 3, 4]"), "map -> view: order and content after both");

        check(v.remove("2"), "view -> map: view.remove reports it removed something");
        check(!m.containsKey("b"), "view -> map: the key whose value was removed is gone");
        check(m.size() == 2, "view -> map: the map shrank");

        Iterator<String> it = v.iterator();
        check(it.hasNext(), "view iterator has elements");
        String first = it.next();
        it.remove();
        check(!m.containsValue(first), "view -> map: Iterator.remove wrote through");
        check(m.size() == 1, "view -> map: the map shrank again");

        v.clear();
        check(m.isEmpty(), "view -> map: clear() emptied the source map");

        System.out.println("CK RJdkMapViews valuesLiveness ok");
    }

    // ---------------------------------------------------------------- 2a
    // pollFirst()/pollLast(): real bytecode is `unlinkFirst`/`unlinkLast`,
    // which write the real size/first/last. Every assertion after the poll
    // reads back through a native, which is where a stale overlay shows.
    static void linkedListPollEnds() {
        LinkedList<String> l = new LinkedList<>(List.of("a", "b", "c", "d"));
        check(l.size() == 4, "size after construction");

        check("a".equals(l.pollFirst()), "pollFirst returns the head");
        check(l.size() == 3, "pollFirst decremented the size the NATIVES read");
        check(l.toString().equals("[b, c, d]"), "pollFirst unlinked exactly one node");
        check("b".equals(l.getFirst()), "the new head is visible to a native reader");

        check("d".equals(l.pollLast()), "pollLast returns the tail");
        check(l.size() == 2, "pollLast decremented the size the NATIVES read");
        check(l.toString().equals("[b, c]"), "pollLast unlinked exactly one node");
        check("c".equals(l.getLast()), "the new tail is visible to a native reader");

        // Drain to empty through the same two doors, then keep using the list:
        // a size that drifted by even one turns the next iteration into a walk
        // off the end of the chain.
        check("b".equals(l.pollFirst()), "drain 1");
        check("c".equals(l.pollLast()), "drain 2");
        check(l.isEmpty(), "the list is empty after draining through poll*");
        check(l.size() == 0, "and its size agrees");
        check(l.pollFirst() == null, "pollFirst on empty answers null, not an exception");
        check(l.pollLast() == null, "pollLast on empty answers null, not an exception");
        boolean threw = false;
        try {
            l.removeFirst();
        } catch (NoSuchElementException expected) {
            threw = true;
        }
        check(threw, "removeFirst on empty still throws, unlike pollFirst");

        // Reuse after the drain: a leaked head/tail pointer surfaces here.
        l.add("z");
        check(l.size() == 1, "size after reuse");
        check(l.toString().equals("[z]"), "content after reuse");
        check("z".equals(l.pollFirst()), "the reused element comes back out");
        check(l.isEmpty(), "and the list is empty again");

        System.out.println("CK RJdkMapViews pollEnds ok");
    }

    // ---------------------------------------------------------------- 2b
    // addAll(int, Collection): the indexed half of the addAll pair.
    static void linkedListAddAllAt() {
        LinkedList<String> l = new LinkedList<>(List.of("a", "d"));
        check(l.addAll(1, List.of("b", "c")), "addAll(int,…) reports it added");
        check(l.size() == 4, "size after a middle insert");
        check(l.toString().equals("[a, b, c, d]"), "the elements went in at the index, in order");
        check("b".equals(l.get(1)), "get(1) after the insert");
        check("c".equals(l.get(2)), "get(2) after the insert");
        check(l.indexOf("d") == 3, "the old tail shifted right");

        check(l.addAll(0, List.of("x")), "insert at the head");
        check(l.toString().equals("[x, a, b, c, d]"), "head insert landed at 0");
        check(l.addAll(5, List.of("y")), "insert at size == append");
        check(l.toString().equals("[x, a, b, c, d, y]"), "tail insert landed at the end");
        check("y".equals(l.getLast()), "and the tail pointer the natives read moved");
        check(!l.addAll(2, List.of()), "an empty source adds nothing and reports false");
        check(l.size() == 6, "…and does not change the size");

        boolean threw = false;
        try {
            l.addAll(99, List.of("boom"));
        } catch (IndexOutOfBoundsException expected) {
            threw = true;
        }
        check(threw, "an out-of-range index throws");
        check(l.size() == 6, "…and nothing was inserted before it threw");
        check(l.toString().equals("[x, a, b, c, d, y]"), "…the list is untouched");

        System.out.println("CK RJdkMapViews addAllAt ok");
    }

    // ---------------------------------------------------------------- 2c
    // descendingIterator(): its bytecode constructs a real node-live
    // LinkedList$ListItr directly, so remove() ran real LinkedList.unlink.
    static void linkedListDescendingIterator() {
        LinkedList<String> l = new LinkedList<>(List.of("p", "q", "r"));
        StringBuilder sb = new StringBuilder();
        for (Iterator<String> it = l.descendingIterator(); it.hasNext(); ) {
            sb.append(it.next());
        }
        check(sb.toString().equals("rqp"), "descendingIterator walks back to front");
        check(l.size() == 3, "a pure walk mutates nothing");

        Iterator<String> it = l.descendingIterator();
        check("r".equals(it.next()), "first element of the descending walk");
        it.remove();
        check(l.size() == 2, "descendingIterator().remove() decremented the NATIVE size");
        check(l.toString().equals("[p, q]"), "…and unlinked the right element");
        check(!l.contains("r"), "…and a native contains agrees");
        check("q".equals(l.getLast()), "…and the tail pointer moved");

        // Keep using the list afterwards: this is where a size that drifted by
        // one used to walk off the end of the chain with an NPE on Node.item.
        l.addLast("s");
        check(l.toString().equals("[p, q, s]"), "the list is still usable after the removal");
        check(l.size() == 3, "and its size is right");
        StringBuilder sb2 = new StringBuilder();
        for (String s : l) {
            sb2.append(s);
        }
        check(sb2.toString().equals("pqs"), "a forward iteration after a descending removal");

        System.out.println("CK RJdkMapViews descendingIterator ok");
    }

    public static void main(String[] args) {
        valuesIdentitySemantics();
        valuesLiveness();
        linkedListPollEnds();
        linkedListAddAllAt();
        linkedListDescendingIterator();
        System.out.println("CK RJdkMapViews checks=" + checks);
        System.out.println("PASS RJdkMapViews (" + checks + " checks)");
    }
}
