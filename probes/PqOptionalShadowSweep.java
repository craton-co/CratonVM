import java.util.*;
import java.util.function.*;

/** L3 tail / `java.util.PriorityQueue` (13 rows) and `java.util.Optional` (20).
 *
 *  `PriorityQueue` is the one collection in this lane whose ITERATION order is
 *  explicitly unspecified while its POLL order is fully specified, so the probe
 *  never prints an iterator, a `toString` or a `toArray`: it drains. Anything
 *  else here would be a harness artefact rather than a measurement.
 *
 *  Its edges:
 *    * `new PriorityQueue<>(0)` is `IllegalArgumentException` — capacity must
 *      be at least 1, unlike every other collection in `java.util`, where 0 is
 *      legal. A shim that copies `HashMap`'s `< 0` test accepts it;
 *    * a null element is NPE and a non-`Comparable` element is
 *      `ClassCastException` — thrown on the FIRST comparison, so an empty queue
 *      accepts what a one-element queue refuses;
 *    * the constructor that takes a `SortedSet` or another `PriorityQueue`
 *      inherits its comparator; the one that takes a plain `Collection` does
 *      not. Same argument object, two different orders.
 *
 *  `Optional`'s whole surface is refusals: `of(null)` throws where
 *  `ofNullable(null)` is empty, `get`/`orElseThrow` on empty throw
 *  `NoSuchElementException`, a null mapper throws even on an EMPTY optional
 *  (the JDK checks the argument before testing presence), and `flatMap`
 *  returning null is NPE while `map` returning null is empty.
 */
public class PqOptionalShadowSweep {
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
    /** The only order a PriorityQueue guarantees. */
    static String drain(PriorityQueue<?> q) {
        StringBuilder b = new StringBuilder("[");
        Object o;
        while ((o = q.poll()) != null) { b.append(o); if (!q.isEmpty()) b.append(", "); }
        return b.append(']').toString();
    }

    static void priorityQueue() {
        PriorityQueue<Integer> q = new PriorityQueue<>();
        p("empty peek", q.peek());
        p("empty poll", q.poll());
        t("empty element", () -> q.element());
        t("empty remove()", () -> q.remove());
        p("empty size", q.size());
        p("empty isEmpty", q.isEmpty());
        p("empty contains", q.contains(1));

        p("offer returns true", q.offer(5));
        q.offer(1); q.offer(3); q.offer(1);
        p("size with a duplicate", q.size());
        p("peek is the minimum", q.peek());
        p("element is the minimum", q.element());
        p("contains", q.contains(3));
        p("contains absent", q.contains(99));
        p("drain order", drain(q));
        p("drained size", q.size());

        // nulls and non-Comparables
        PriorityQueue<Object> n = new PriorityQueue<>();
        t("offer null", () -> n.offer(null));
        t("add null", () -> n.add(null));
        p("contains null", n.contains(null));
        p("remove(Object) null", n.remove(null));
        t("addAll with a null element", () -> n.addAll(Arrays.asList("a", null)));
        t("addAll null", () -> n.addAll(null));
        PriorityQueue<Object> nc = new PriorityQueue<>();
        t("first non-Comparable", () -> nc.offer(new Object()));
        PriorityQueue<Object> nc2 = new PriorityQueue<>();
        nc2.offer("a");
        t("second non-Comparable", () -> nc2.offer(new Object()));
        t("mixed types", () -> nc2.offer(Integer.valueOf(1)));
        p("state after refusals", nc2.size());
        t("addAll self", () -> nc2.addAll(nc2));

        // capacity validation: 0 is illegal here and legal everywhere else
        t("new PriorityQueue(0)", () -> new PriorityQueue<Integer>(0));
        t("new PriorityQueue(-1)", () -> new PriorityQueue<Integer>(-1));
        t("new PriorityQueue(1)", () -> new PriorityQueue<Integer>(1));
        t("new PriorityQueue(0, cmp)",
            () -> new PriorityQueue<Integer>(0, Comparator.naturalOrder()));
        t("new PriorityQueue((Collection) null)",
            () -> new PriorityQueue<Integer>((Collection<Integer>) null));
        t("new PriorityQueue(null comparator)",
            () -> new PriorityQueue<Integer>(11, (Comparator<Integer>) null));

        // comparator inheritance depends on the STATIC type of the argument
        TreeSet<Integer> ss = new TreeSet<>(Comparator.reverseOrder());
        ss.addAll(Arrays.asList(1, 2, 3));
        p("ctor(SortedSet) inherits the comparator",
            drain(new PriorityQueue<>(ss)));
        p("ctor(Collection) does not",
            drain(new PriorityQueue<>((Collection<Integer>) ss)));
        PriorityQueue<Integer> rq = new PriorityQueue<>(Comparator.reverseOrder());
        rq.addAll(Arrays.asList(1, 2, 3));
        p("ctor(PriorityQueue) inherits the comparator",
            drain(new PriorityQueue<>(rq)));
        p("comparator() is null for natural", new PriorityQueue<Integer>().comparator());
        p("comparator() is not null when given",
            new PriorityQueue<Integer>(Comparator.reverseOrder()).comparator() != null);
        p("reverse comparator drain",
            drain(new PriorityQueue<>(new PriorityQueue<>(rq))));

        // remove(Object) removes ONE equal element and re-heapifies
        PriorityQueue<Integer> r = new PriorityQueue<>(Arrays.asList(5, 1, 3, 1, 9));
        p("remove present", r.remove(Integer.valueOf(1)));
        p("size after remove", r.size());
        p("drain after remove", drain(r));
        PriorityQueue<Integer> r2 = new PriorityQueue<>(Arrays.asList(5, 1, 3));
        p("remove absent", r2.remove(Integer.valueOf(99)));
        p("removeIf", r2.removeIf(x -> x == 3));
        p("drain after removeIf", drain(r2));
        PriorityQueue<Integer> r3 = new PriorityQueue<>(Arrays.asList(5, 1, 3, 7));
        p("retainAll", r3.retainAll(Arrays.asList(1, 7)));
        p("drain after retainAll", drain(r3));
        PriorityQueue<Integer> r4 = new PriorityQueue<>(Arrays.asList(5, 1, 3, 7));
        p("removeAll", r4.removeAll(Arrays.asList(1, 7)));
        p("drain after removeAll", drain(r4));
        PriorityQueue<Integer> r5 = new PriorityQueue<>(Arrays.asList(5, 1));
        r5.clear();
        p("clear", r5.size());
        p("clear then peek", r5.peek());

        // a heap that grows past its initial capacity keeps the invariant
        PriorityQueue<Integer> g = new PriorityQueue<>(1);
        for (int i = 50; i > 0; i--) g.offer(i);
        p("grown size", g.size());
        p("grown peek", g.peek());
        StringBuilder first = new StringBuilder();
        for (int i = 0; i < 5; i++) first.append(g.poll()).append(',');
        p("grown poll prefix", first.toString());

        // fail-fast
        PriorityQueue<Integer> ff = new PriorityQueue<>(Arrays.asList(1, 2, 3));
        t("fail fast on add during iteration", () -> { for (Integer x : ff) ff.add(9); });
        PriorityQueue<Integer> ir = new PriorityQueue<>(Arrays.asList(1, 2, 3));
        Iterator<Integer> it = ir.iterator();
        it.next(); it.remove();
        p("iterator remove size", ir.size());
        p("toArray length", new PriorityQueue<>(Arrays.asList(1, 2)).toArray().length);
        p("equals another queue (identity)", new PriorityQueue<>(Arrays.asList(1))
            .equals(new PriorityQueue<>(Arrays.asList(1))));
    }

    static void optional() {
        Optional<String> e = Optional.empty();
        Optional<String> v = Optional.of("x");
        p("empty isPresent", e.isPresent());
        p("empty isEmpty", e.isEmpty());
        p("present isPresent", v.isPresent());
        p("present get", v.get());
        t("empty get", () -> e.get());
        t("empty orElseThrow", () -> e.orElseThrow());
        t("empty orElseThrow(supplier)", () -> e.orElseThrow(IllegalStateException::new));
        t("empty orElseThrow(null supplier)", () -> e.orElseThrow((Supplier<RuntimeException>) null));
        t("present orElseThrow(null supplier)", () -> v.orElseThrow((Supplier<RuntimeException>) null));
        p("empty orElse", e.orElse("d"));
        p("present orElse", v.orElse("d"));
        p("empty orElse null", e.orElse(null));
        p("empty orElseGet", e.orElseGet(() -> "g"));
        p("present orElseGet does not call the supplier", v.orElseGet(() -> { throw new AssertionError(); }));
        t("empty orElseGet(null)", () -> e.orElseGet(null));
        t("present orElseGet(null)", () -> v.orElseGet(null));

        t("of(null)", () -> Optional.of(null));
        p("ofNullable(null) is empty", Optional.ofNullable(null).isPresent());
        p("ofNullable(x)", Optional.ofNullable("x").get());

        p("map", v.map(s -> s + "!").get());
        p("map to null is empty", v.map(s -> null).isPresent());
        p("map on empty", e.map(s -> "y").isPresent());
        t("map(null) on present", () -> v.map(null));
        // the JDK validates the argument BEFORE testing presence
        t("map(null) on empty", () -> e.map(null));
        p("flatMap", v.flatMap(s -> Optional.of(s + "!")).get());
        t("flatMap returning null", () -> v.flatMap(s -> null));
        p("flatMap on empty", e.flatMap(s -> Optional.of("y")).isPresent());
        t("flatMap(null) on empty", () -> e.flatMap(null));
        p("filter true", v.filter(s -> true).isPresent());
        p("filter false", v.filter(s -> false).isPresent());
        p("filter on empty", e.filter(s -> true).isPresent());
        t("filter(null) on empty", () -> e.filter(null));
        p("or on empty", e.or(() -> Optional.of("z")).get());
        p("or on present", v.or(() -> Optional.of("z")).get());
        t("or returning null", () -> e.or(() -> null));
        t("or(null) on present", () -> v.or(null));
        p("stream on present count", v.stream().count());
        p("stream on empty count", e.stream().count());

        StringBuilder sb = new StringBuilder();
        v.ifPresent(sb::append);
        e.ifPresent(sb::append);
        p("ifPresent", sb.toString());
        t("ifPresent(null) on empty", () -> e.ifPresent(null));
        StringBuilder sb2 = new StringBuilder();
        v.ifPresentOrElse(sb2::append, () -> sb2.append("none"));
        e.ifPresentOrElse(sb2::append, () -> sb2.append("none"));
        p("ifPresentOrElse", sb2.toString());
        t("ifPresentOrElse(null,null) on empty", () -> e.ifPresentOrElse(null, null));

        p("toString present", v.toString());
        p("toString empty", e.toString());
        p("equals same value", Optional.of("x").equals(Optional.of("x")));
        p("equals different value", Optional.of("x").equals(Optional.of("y")));
        p("empty equals empty", Optional.empty().equals(Optional.empty()));
        p("empty equals present", Optional.empty().equals(Optional.of("x")));
        p("equals a raw value", Optional.of("x").equals("x"));
        p("hashCode is the value's", Optional.of("x").hashCode() == "x".hashCode());
        p("empty hashCode", Optional.empty().hashCode());
        p("Optional.empty is a singleton", Optional.empty() == Optional.empty());

        // the primitive optionals
        p("OptionalInt empty isPresent", OptionalInt.empty().isPresent());
        t("OptionalInt empty getAsInt", () -> OptionalInt.empty().getAsInt());
        p("OptionalInt of", OptionalInt.of(3).getAsInt());
        p("OptionalInt orElse", OptionalInt.empty().orElse(7));
        p("OptionalInt toString", OptionalInt.of(3).toString());
        p("OptionalInt empty toString", OptionalInt.empty().toString());
        p("OptionalInt equals", OptionalInt.of(3).equals(OptionalInt.of(3)));
        p("OptionalInt hashCode", OptionalInt.of(3).hashCode() == Integer.hashCode(3));
        p("OptionalLong of", OptionalLong.of(3L).getAsLong());
        t("OptionalLong empty getAsLong", () -> OptionalLong.empty().getAsLong());
        p("OptionalLong toString", OptionalLong.of(3L).toString());
        p("OptionalDouble of", OptionalDouble.of(1.5).getAsDouble());
        t("OptionalDouble empty getAsDouble", () -> OptionalDouble.empty().getAsDouble());
        p("OptionalDouble toString", OptionalDouble.of(1.5).toString());
        p("OptionalDouble NaN equals itself", OptionalDouble.of(Double.NaN)
            .equals(OptionalDouble.of(Double.NaN)));
        p("OptionalDouble -0.0 vs 0.0", OptionalDouble.of(-0.0).equals(OptionalDouble.of(0.0)));
    }

    public static void main(String[] args) {
        priorityQueue();
        optional();
        System.out.println("ROWS " + rows);
        System.out.println("DONE PqOptionalShadowSweep");
    }
}
