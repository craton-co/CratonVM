import java.util.*;

/** L3 / `java.util.ArrayDeque` (33 rows) and `java.util.LinkedList` (31 rows).
 *
 *  These two implement the SAME two interfaces (`Deque`, `Queue`) with opposite
 *  null policies, which is the single edge most likely to be shared by one
 *  native body:
 *
 *    * `ArrayDeque` refuses a null ELEMENT with NPE on every insertion —
 *      `add`, `addFirst/Last`, `offer*`, `push`, `addAll`. A deque that accepts
 *      one is broken, and its own `contains(null)`/`remove(null)` must still
 *      answer `false` rather than throw;
 *    * `LinkedList` permits nulls everywhere, so `poll()` returning null is
 *      genuinely ambiguous there and `indexOf(null)` has to work.
 *
 *  The second shared edge is the throwing/returning pair. `element`,
 *  `getFirst`, `removeFirst`, `pop`, `remove()` all throw
 *  `NoSuchElementException` on empty; `peek`, `peekFirst`, `poll`, `pollFirst`
 *  all answer `null` for the same state. That is one decision asked twice, and
 *  an implementation that routes both through one helper gets half of it wrong.
 *
 *  Third: `LinkedList`'s index bounds. `add(int,E)` accepts `index == size` and
 *  `get(int)` does not — the same number is legal for one and out of range for
 *  the other.
 *
 *  DETERMINISM: both containers have fully specified encounter order.
 */
public class DequeListShadowSweep {
    static int rows = 0;
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    interface ThrowingRun { void run() throws Throwable; }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface Call { Object call() throws Throwable; }
    static void tv(String tag, Call r) {
        try { p(tag, "ok " + String.valueOf(r.call())); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    // ------------------------------------------------------------------
    // 1. ArrayDeque — the null refusal, on every door
    // ------------------------------------------------------------------
    static void dequeNulls() {
        ArrayDeque<String> d = new ArrayDeque<>();
        t("ad add null", () -> d.add(null));
        t("ad addFirst null", () -> d.addFirst(null));
        t("ad addLast null", () -> d.addLast(null));
        t("ad offer null", () -> d.offer(null));
        t("ad offerFirst null", () -> d.offerFirst(null));
        t("ad offerLast null", () -> d.offerLast(null));
        t("ad push null", () -> d.push(null));
        t("ad addAll with a null element", () -> d.addAll(Arrays.asList("a", null)));
        t("ad addAll null collection", () -> d.addAll(null));
        p("ad still empty after every refusal", d.size());
        p("ad toString after refusals", d.toString());

        // ... but the QUERY side takes null and answers false.
        d.add("a");
        p("ad contains null", d.contains(null));
        p("ad remove(Object) null", d.remove(null));
        p("ad removeFirstOccurrence null", d.removeFirstOccurrence(null));
        p("ad removeLastOccurrence null", d.removeLastOccurrence(null));
        p("ad unchanged", d.toString());

        // a partially-successful addAll: the JDK adds until it meets the null
        ArrayDeque<String> pa = new ArrayDeque<>();
        t("ad addAll partial", () -> pa.addAll(Arrays.asList("a", "b", null, "c")));
        p("ad addAll partial state", pa.toString());

        t("ad ctor(Collection) null", () -> new ArrayDeque<String>((Collection<String>) null));
        t("ad ctor(Collection) with null element",
            () -> new ArrayDeque<>(Arrays.asList("a", (String) null)));
        t("ad ctor(-1)", () -> new ArrayDeque<String>(-1));
        t("ad ctor(0)", () -> new ArrayDeque<String>(0));
        tv("ad ctor(0) usable", () -> { ArrayDeque<String> z = new ArrayDeque<>(0);
                                        z.add("a"); return z.toString(); });
    }

    // ------------------------------------------------------------------
    // 2. the throwing/returning pair on an empty container
    // ------------------------------------------------------------------
    static void emptyPair() {
        ArrayDeque<String> d = new ArrayDeque<>();
        t("ad empty getFirst", () -> d.getFirst());
        t("ad empty getLast", () -> d.getLast());
        t("ad empty element", () -> d.element());
        t("ad empty removeFirst", () -> d.removeFirst());
        t("ad empty removeLast", () -> d.removeLast());
        t("ad empty remove()", () -> d.remove());
        t("ad empty pop", () -> d.pop());
        p("ad empty peek", d.peek());
        p("ad empty peekFirst", d.peekFirst());
        p("ad empty peekLast", d.peekLast());
        p("ad empty poll", d.poll());
        p("ad empty pollFirst", d.pollFirst());
        p("ad empty pollLast", d.pollLast());
        p("ad empty isEmpty", d.isEmpty());
        p("ad empty iterator hasNext", d.iterator().hasNext());
        t("ad empty iterator next", () -> d.iterator().next());
        p("ad empty descendingIterator hasNext", d.descendingIterator().hasNext());

        LinkedList<String> l = new LinkedList<>();
        t("ll empty getFirst", () -> l.getFirst());
        t("ll empty getLast", () -> l.getLast());
        t("ll empty element", () -> l.element());
        t("ll empty removeFirst", () -> l.removeFirst());
        t("ll empty removeLast", () -> l.removeLast());
        t("ll empty remove()", () -> l.remove());
        t("ll empty pop", () -> l.pop());
        p("ll empty peek", l.peek());
        p("ll empty peekFirst", l.peekFirst());
        p("ll empty poll", l.poll());
        p("ll empty pollLast", l.pollLast());
        t("ll empty get(0)", () -> l.get(0));
        t("ll empty remove(0)", () -> l.remove(0));
        t("ll empty set(0)", () -> l.set(0, "x"));
    }

    // ------------------------------------------------------------------
    // 3. ArrayDeque as a stack and as a queue
    // ------------------------------------------------------------------
    static void dequeOps() {
        ArrayDeque<String> d = new ArrayDeque<>();
        p("add returns true", d.add("b"));
        d.addFirst("a");
        d.addLast("c");
        p("order", d.toString());
        p("peekFirst", d.peekFirst());
        p("peekLast", d.peekLast());
        p("getFirst", d.getFirst());
        p("getLast", d.getLast());
        p("element is head", d.element());
        p("size", d.size());
        p("contains", d.contains("b"));
        p("indexOf-like via contains absent", d.contains("z"));

        // push/pop are addFirst/removeFirst: a stack grows at the HEAD, so
        // toString of a deque used as a stack is in reverse push order.
        ArrayDeque<String> st = new ArrayDeque<>();
        st.push("1"); st.push("2"); st.push("3");
        p("stack toString", st.toString());
        p("pop", st.pop());
        p("stack after pop", st.toString());

        // offer/poll are the queue view: FIFO.
        ArrayDeque<String> q = new ArrayDeque<>();
        q.offer("1"); q.offer("2");
        p("offer/poll is FIFO", q.poll());

        ArrayDeque<String> occ = new ArrayDeque<>(Arrays.asList("a", "b", "a", "c", "a"));
        p("removeFirstOccurrence", occ.removeFirstOccurrence("a"));
        p("after removeFirstOccurrence", occ.toString());
        p("removeLastOccurrence", occ.removeLastOccurrence("a"));
        p("after removeLastOccurrence", occ.toString());
        p("removeFirstOccurrence absent", occ.removeFirstOccurrence("z"));
        p("remove(Object) removes first", occ.remove("a"));
        p("after remove(Object)", occ.toString());

        p("descendingIterator order", iterToString(
            new ArrayDeque<>(Arrays.asList("a", "b", "c")).descendingIterator()));
        p("toArray", Arrays.toString(new ArrayDeque<>(Arrays.asList("a", "b")).toArray()));
        p("toArray typed", Arrays.toString(
            new ArrayDeque<>(Arrays.asList("a", "b")).toArray(new String[0])));
        p("clone", new ArrayDeque<>(Arrays.asList("a", "b")).clone().toString());

        // An ArrayDeque is NOT a List: it inherits Object.equals.
        p("equals another deque with same content",
            new ArrayDeque<>(Arrays.asList("a")).equals(new ArrayDeque<>(Arrays.asList("a"))));

        // fail-fast
        ArrayDeque<String> ff = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        t("ad fail fast on add during iteration", () -> {
            for (String x : ff) ff.add("z");
        });
        ArrayDeque<String> ff2 = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        t("ad fail fast on remove during iteration", () -> {
            for (String x : ff2) ff2.remove("a");
        });
        ArrayDeque<String> ir = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        Iterator<String> it = ir.iterator();
        it.next();
        it.remove();
        p("ad iterator remove", ir.toString());
        t("ad iterator remove twice", () -> it.remove());

        // clear / retainAll / removeAll / removeIf
        ArrayDeque<String> ops = new ArrayDeque<>(Arrays.asList("a", "b", "c", "d"));
        p("removeIf", ops.removeIf(x -> x.equals("b")));
        p("after removeIf", ops.toString());
        p("retainAll", ops.retainAll(Arrays.asList("a", "c")));
        p("after retainAll", ops.toString());
        p("removeAll", ops.removeAll(Arrays.asList("a")));
        p("after removeAll", ops.toString());
        ops.clear();
        p("after clear", ops.toString());
        p("after clear isEmpty", ops.isEmpty());

        // growth past the initial ring buffer, and wrap-around: add at both
        // ends past the default capacity of 16.
        ArrayDeque<Integer> gr = new ArrayDeque<>();
        for (int i = 0; i < 20; i++) { gr.addLast(i); gr.addFirst(-i); }
        p("grow size", gr.size());
        p("grow first", gr.peekFirst());
        p("grow last", gr.peekLast());
        StringBuilder sb = new StringBuilder();
        for (Integer i : gr) sb.append(i).append(',');
        p("grow order", sb.toString());
        p("grow toArray length", gr.toArray().length);
    }

    static String iterToString(Iterator<?> it) {
        StringBuilder b = new StringBuilder("[");
        while (it.hasNext()) { b.append(it.next()); if (it.hasNext()) b.append(", "); }
        return b.append(']').toString();
    }

    // ------------------------------------------------------------------
    // 4. LinkedList — index bounds and the null-permitting policy
    // ------------------------------------------------------------------
    static void listOps() {
        LinkedList<String> l = new LinkedList<>(Arrays.asList("a", "b", "c"));
        p("get 0", l.get(0));
        p("get last", l.get(2));
        t("get -1", () -> l.get(-1));
        t("get size", () -> l.get(3));
        t("set -1", () -> l.set(-1, "x"));
        t("set size", () -> l.set(3, "x"));
        p("set returns old", l.set(1, "B"));
        p("after set", l.toString());
        // add(int,E) accepts index == size; get(int) does not.
        t("add at size", () -> l.add(3, "d"));
        p("after add at size", l.toString());
        t("add at size+1", () -> l.add(5, "e"));
        t("add at -1", () -> l.add(-1, "e"));
        t("remove(int) size", () -> l.remove(4));
        p("remove(int) returns old", l.remove(0));
        p("after remove(int)", l.toString());
        t("addAll at size", () -> l.addAll(3, Arrays.asList("x")));
        t("addAll at size+1", () -> l.addAll(9, Arrays.asList("x")));
        t("addAll null at valid index", () -> l.addAll(0, null));
        p("state", l.toString());
        t("listIterator(-1)", () -> l.listIterator(-1));
        t("listIterator(size)", () -> l.listIterator(l.size()));
        t("listIterator(size+1)", () -> l.listIterator(l.size() + 1));

        // nulls are ordinary elements
        LinkedList<String> n = new LinkedList<>();
        p("ll add null", n.add(null));
        n.add("a");
        n.add(null);
        p("ll toString with nulls", n.toString());
        p("ll size with nulls", n.size());
        p("ll contains null", n.contains(null));
        p("ll indexOf null", n.indexOf(null));
        p("ll lastIndexOf null", n.lastIndexOf(null));
        p("ll indexOf absent", n.indexOf("zz"));
        p("ll remove(Object) null", n.remove(null));
        p("ll after remove null", n.toString());
        p("ll get null element", n.get(1));
        p("ll poll returns a stored null", new LinkedList<>(Arrays.asList((String) null)).poll());
        p("ll addFirst null", addFirstNull());
        p("ll offer null", new LinkedList<String>().offer(null));

        // deque doors on a LinkedList
        LinkedList<String> d = new LinkedList<>();
        d.addFirst("b"); d.addLast("c"); d.push("a");
        p("ll as deque", d.toString());
        p("ll pop", d.pop());
        p("ll peekLast", d.peekLast());
        p("ll pollLast", d.pollLast());
        p("ll descendingIterator", iterToString(
            new LinkedList<>(Arrays.asList("a", "b", "c")).descendingIterator()));
        p("ll removeFirstOccurrence", new LinkedList<>(Arrays.asList("a", "b", "a"))
            .removeFirstOccurrence("a"));
        p("ll removeLastOccurrence", new LinkedList<>(Arrays.asList("a", "b", "a"))
            .removeLastOccurrence("a"));

        // subList is a view with its own bounds
        LinkedList<String> s = new LinkedList<>(Arrays.asList("a", "b", "c", "d"));
        p("subList", s.subList(1, 3).toString());
        p("subList empty", s.subList(2, 2).toString());
        t("subList from > to", () -> s.subList(3, 1));
        t("subList past end", () -> s.subList(0, 9));
        t("subList negative", () -> s.subList(-1, 2));
        List<String> sv = s.subList(1, 3);
        sv.set(0, "B");
        p("subList write through", s.toString());

        // listIterator add/set/remove
        LinkedList<String> li = new LinkedList<>(Arrays.asList("a", "b", "c"));
        ListIterator<String> lit = li.listIterator();
        t("listIterator set before next", () -> lit.set("x"));
        t("listIterator remove before next", () -> lit.remove());
        lit.next();
        lit.set("A");
        lit.add("A2");
        p("listIterator after set+add", li.toString());
        p("listIterator nextIndex", lit.nextIndex());
        p("listIterator previousIndex", lit.previousIndex());
        p("listIterator hasPrevious", lit.hasPrevious());
        p("listIterator previous", lit.previous());
        t("listIterator remove after add", () -> lit.remove());

        // fail-fast
        LinkedList<String> ff = new LinkedList<>(Arrays.asList("a", "b", "c"));
        t("ll fail fast", () -> { for (String x : ff) ff.add("z"); });

        // equals/hashCode against another List type
        p("ll equals ArrayList", new LinkedList<>(Arrays.asList("a", "b"))
            .equals(new ArrayList<>(Arrays.asList("a", "b"))));
        p("ll hashCode agrees", new LinkedList<>(Arrays.asList("a", "b")).hashCode()
            == new ArrayList<>(Arrays.asList("a", "b")).hashCode());
        p("ll equals a deque (must be false)",
            new LinkedList<>(Arrays.asList("a")).equals(new ArrayDeque<>(Arrays.asList("a"))));
        p("ll clone", new LinkedList<>(Arrays.asList("a", "b")).clone().toString());
        p("ll toArray", Arrays.toString(new LinkedList<>(Arrays.asList("a", "b")).toArray()));
        p("ll toArray typed short", Arrays.toString(
            new LinkedList<>(Arrays.asList("a", "b")).toArray(new String[0])));
        String[] big = new String[4];
        Arrays.fill(big, "Z");
        new LinkedList<>(Arrays.asList("a", "b")).toArray(big);
        p("ll toArray typed long nulls the slot after", Arrays.toString(big));
        t("ll toArray null array", () -> new LinkedList<>(Arrays.asList("a")).toArray((String[]) null));
        t("ll ctor null", () -> new LinkedList<String>((Collection<String>) null));

        LinkedList<String> ops = new LinkedList<>(Arrays.asList("a", "b", "c", "d"));
        p("ll removeIf", ops.removeIf(x -> x.equals("b")));
        p("ll after removeIf", ops.toString());
        p("ll retainAll", ops.retainAll(Arrays.asList("a", "c")));
        p("ll after retainAll", ops.toString());
        p("ll removeAll", ops.removeAll(Arrays.asList("a")));
        p("ll after removeAll", ops.toString());
        ops.replaceAll(x -> x + "!");
        p("ll replaceAll", ops.toString());
        ops.addAll(Arrays.asList("b!", "a!"));
        ops.sort(Comparator.naturalOrder());
        p("ll sort", ops.toString());
        p("ll indexOf after sort", ops.indexOf("a!"));
    }

    static boolean addFirstNull() {
        LinkedList<String> x = new LinkedList<>();
        x.addFirst(null);
        return x.size() == 1 && x.get(0) == null;
    }

    public static void main(String[] args) {
        dequeNulls();
        emptyPair();
        dequeOps();
        listOps();
        System.out.println("ROWS " + rows);
        System.out.println("DONE DequeListShadowSweep");
    }
}
