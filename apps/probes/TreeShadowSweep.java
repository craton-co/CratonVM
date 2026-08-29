import java.util.*;

/** L3 / `java.util.TreeMap` (41 rows) and `java.util.TreeSet` (31 rows).
 *
 *  A sorted map is the one collection whose contract is mostly about the
 *  ORDER RELATION rather than the container, and that is exactly what a native
 *  written from memory reproduces badly:
 *
 *    * natural ordering throws NPE on a null key; a null-TOLERANT comparator
 *      does not. Same call, opposite answers, decided by a field;
 *    * a key whose class does not implement `Comparable` is a
 *      ClassCastException, thrown by the FIRST comparison — so an empty map
 *      accepts it and a one-entry map does not;
 *    * `firstKey` on empty is `NoSuchElementException` while `firstEntry` is
 *      `null` — the same question asked twice with two different refusals;
 *    * `subMap`/`headMap`/`tailMap` are live views with a RANGE, and a write
 *      outside the range is `IllegalArgumentException`, not a silent accept;
 *    * `ceiling`/`floor`/`higher`/`lower` differ only in strictness and at the
 *      two ends, which is where an off-by-one lives.
 *
 *  DETERMINISM: a TreeMap's iteration order is fully specified, so unlike the
 *  hash containers everything here can be printed in encounter order.
 */
public class TreeShadowSweep {
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

    static TreeMap<String, Integer> m5() {
        TreeMap<String, Integer> m = new TreeMap<>();
        m.put("b", 2); m.put("d", 4); m.put("a", 1); m.put("e", 5); m.put("c", 3);
        return m;
    }

    // ------------------------------------------------------------------
    // 1. ordering, and the two ways it can refuse
    // ------------------------------------------------------------------
    static void ordering() {
        TreeMap<String, Integer> m = m5();
        p("sorted toString", m.toString());
        p("keySet order", m.keySet().toString());
        p("values order", m.values().toString());
        p("entrySet order", m.entrySet().toString());
        p("descendingMap", m.descendingMap().toString());
        p("descendingKeySet", m.descendingKeySet().toString());
        p("navigableKeySet", m.navigableKeySet().toString());
        p("comparator is null for natural", m.comparator());
        p("size", m.size());

        // natural ordering: null key is NPE everywhere, including on an EMPTY
        // map, because the compare happens before any lookup.
        TreeMap<String, Integer> e = new TreeMap<>();
        t("empty natural put null key", () -> e.put(null, 1));
        t("empty natural get null key", () -> e.get(null));
        t("empty natural containsKey null", () -> e.containsKey(null));
        t("empty natural remove null", () -> e.remove(null));
        t("populated natural put null key", () -> m.put(null, 1));
        t("populated natural get null key", () -> m.get(null));
        p("null VALUE is legal", m.put("nv", null));
        p("get null value", m.get("nv"));
        p("containsKey with null value", m.containsKey("nv"));
        p("containsValue null", m.containsValue(null));
        m.remove("nv");

        // a null-tolerant comparator makes every one of those legal
        Comparator<String> nullsFirst = Comparator.nullsFirst(Comparator.<String>naturalOrder());
        TreeMap<String, Integer> nt = new TreeMap<>(nullsFirst);
        nt.put("b", 2);
        t("nullsFirst put null key", () -> nt.put(null, 0));
        p("nullsFirst get null key", nt.get(null));
        p("nullsFirst containsKey null", nt.containsKey(null));
        p("nullsFirst order", nt.toString());
        p("nullsFirst firstKey", nt.firstKey());
        p("comparator is not null", nt.comparator() != null);
        p("nullsFirst remove null", nt.remove(null));
        p("nullsFirst after remove", nt.toString());

        // a reverse comparator
        TreeMap<String, Integer> rv = new TreeMap<>(Comparator.reverseOrder());
        rv.put("a", 1); rv.put("c", 3); rv.put("b", 2);
        p("reverse order", rv.toString());
        p("reverse firstKey", rv.firstKey());
        p("reverse lastKey", rv.lastKey());
        p("reverse headMap", rv.headMap("b").toString());
        p("reverse ceilingKey", rv.ceilingKey("b"));

        // a non-Comparable key. An EMPTY map takes the first one without a
        // comparison in some implementations and the JDK still refuses it.
        TreeMap<Object, Object> nc = new TreeMap<>();
        t("empty put non-Comparable", () -> nc.put(new Object(), "x"));
        TreeMap<Object, Object> nc2 = new TreeMap<>();
        nc2.put("a", 1);
        t("populated put non-Comparable", () -> nc2.put(new Object(), "x"));
        t("mixed-type put", () -> nc2.put(Integer.valueOf(1), "x"));
        t("get wrong type", () -> nc2.get(Integer.valueOf(1)));
        p("state after refusals", nc2.toString());
    }

    // ------------------------------------------------------------------
    // 2. the ends: first/last/poll/ceiling/floor/higher/lower
    // ------------------------------------------------------------------
    static void ends() {
        TreeMap<String, Integer> e = new TreeMap<>();
        t("empty firstKey", () -> e.firstKey());
        t("empty lastKey", () -> e.lastKey());
        p("empty firstEntry", e.firstEntry());
        p("empty lastEntry", e.lastEntry());
        p("empty pollFirstEntry", e.pollFirstEntry());
        p("empty pollLastEntry", e.pollLastEntry());
        p("empty ceilingKey", e.ceilingKey("a"));
        p("empty floorEntry", e.floorEntry("a"));
        p("empty headMap", e.headMap("a").toString());

        TreeMap<String, Integer> m = m5();
        p("firstKey", m.firstKey());
        p("lastKey", m.lastKey());
        p("firstEntry", m.firstEntry());
        p("lastEntry", m.lastEntry());

        // ceiling >= k, higher > k, floor <= k, lower < k. The interesting
        // rows are AT a present key and PAST both ends.
        for (String k : new String[] { "0", "a", "bb", "c", "e", "z" }) {
            p("ceilingKey " + k, m.ceilingKey(k));
            p("floorKey " + k, m.floorKey(k));
            p("higherKey " + k, m.higherKey(k));
            p("lowerKey " + k, m.lowerKey(k));
        }
        p("ceilingEntry c", m.ceilingEntry("c"));
        p("floorEntry bb", m.floorEntry("bb"));
        // The navigation family and `getEntry` disagree about a null key, and
        // they disagree only when the container is EMPTY: `getEntry` opens with
        // an explicit null check, `getCeilingEntry` and its siblings refuse as a
        // side effect of the first comparison and so answer null for an empty
        // map. Both halves, on all four pairs.
        t("ceilingKey null", () -> m.ceilingKey(null));
        t("floorKey null", () -> m.floorKey(null));
        t("higherKey null", () -> m.higherKey(null));
        t("lowerKey null", () -> m.lowerKey(null));
        t("ceilingEntry null", () -> m.ceilingEntry(null));
        t("floorEntry null", () -> m.floorEntry(null));
        t("higherEntry null", () -> m.higherEntry(null));
        t("lowerEntry null", () -> m.lowerEntry(null));
        TreeMap<String, Integer> en = new TreeMap<>();
        t("EMPTY ceilingKey null", () -> en.ceilingKey(null));
        t("EMPTY floorKey null", () -> en.floorKey(null));
        t("EMPTY higherKey null", () -> en.higherKey(null));
        t("EMPTY lowerKey null", () -> en.lowerKey(null));
        t("EMPTY get null", () -> en.get(null));
        // A non-Comparable key reaches the same comparison.
        TreeMap<Object, Object> mm = new TreeMap<>();
        mm.put("a", 1);
        t("ceilingKey non-Comparable", () -> mm.ceilingKey(new Object()));
        t("EMPTY ceilingKey non-Comparable",
            () -> new TreeMap<Object, Object>().ceilingKey(new Object()));
        // A null-tolerant comparator makes every one of them legal.
        TreeMap<String, Integer> nt2 = new TreeMap<>(
            Comparator.nullsFirst(Comparator.<String>naturalOrder()));
        nt2.put("b", 2);
        t("nullsFirst ceilingKey null", () -> nt2.ceilingKey(null));
        t("nullsFirst higherKey null", () -> nt2.higherKey(null));

        TreeSet<String> ns = new TreeSet<>(Arrays.asList("a", "c"));
        t("set ceiling null", () -> ns.ceiling(null));
        t("set floor null", () -> ns.floor(null));
        t("set higher null", () -> ns.higher(null));
        t("set lower null", () -> ns.lower(null));
        t("EMPTY set ceiling null", () -> new TreeSet<String>().ceiling(null));

        TreeMap<String, Integer> q = m5();
        p("pollFirstEntry", q.pollFirstEntry());
        p("pollLastEntry", q.pollLastEntry());
        p("after polls", q.toString());
        p("after polls size", q.size());

        // A polled entry is DETACHED: setValue on it must not write back.
        Map.Entry<String, Integer> pe = q.pollFirstEntry();
        t("polled entry setValue", () -> pe.setValue(99));
        p("polled entry did not write back", q.toString());
        // firstEntry's entry is also an immutable snapshot in the JDK.
        Map.Entry<String, Integer> fe = q.firstEntry();
        t("firstEntry setValue", () -> fe.setValue(99));
        p("firstEntry setValue effect", q.toString());
    }

    // ------------------------------------------------------------------
    // 3. the range views
    // ------------------------------------------------------------------
    static void ranges() {
        TreeMap<String, Integer> m = m5();
        p("headMap exclusive", m.headMap("c").toString());
        p("headMap inclusive", m.headMap("c", true).toString());
        p("tailMap inclusive default", m.tailMap("c").toString());
        p("tailMap exclusive", m.tailMap("c", false).toString());
        p("subMap default", m.subMap("b", "d").toString());
        p("subMap both inclusive", m.subMap("b", true, "d", true).toString());
        p("subMap both exclusive", m.subMap("b", false, "d", false).toString());
        p("subMap empty range", m.subMap("b", true, "b", false).toString());
        t("subMap from > to", () -> m.subMap("d", "b"));
        t("subMap equal exclusive/inclusive mix", () -> m.subMap("b", false, "b", true));
        t("headMap null", () -> m.headMap(null));

        // A view is LIVE both ways.
        SortedMap<String, Integer> sub = m.subMap("b", "d");
        p("view size", sub.size());
        m.put("bb", 22);
        p("view saw a source write", sub.toString());
        sub.put("cc", 33);
        p("source saw a view write", m.toString());
        t("view write out of range", () -> sub.put("z", 1));
        t("view write below range", () -> sub.put("a", 1));
        p("view firstKey", sub.firstKey());
        p("view lastKey", sub.lastKey());
        p("view containsKey outside", sub.containsKey("e"));
        p("view get outside", sub.get("e"));
        p("view remove outside", sub.remove("e"));
        p("source still has e", m.containsKey("e"));
        sub.remove("bb");
        p("view remove wrote through", m.containsKey("bb"));

        // a view OF a view narrows, and cannot widen
        NavigableMap<String, Integer> nsub = m.subMap("b", true, "d", true);
        p("sub of sub", nsub.subMap("c", true, "d", true).toString());
        t("sub of sub widening", () -> nsub.subMap("a", true, "d", true));
        p("descending view", nsub.descendingMap().toString());
        p("view headMap", nsub.headMap("c", false).toString());

        // clearing a view removes exactly the range from the source
        TreeMap<String, Integer> c = m5();
        c.subMap("b", "d").clear();
        p("view clear", c.toString());

        // TreeSet ranges
        TreeSet<String> s = new TreeSet<>(Arrays.asList("a", "b", "c", "d", "e"));
        p("headSet exclusive", s.headSet("c").toString());
        p("headSet inclusive", s.headSet("c", true).toString());
        p("tailSet inclusive default", s.tailSet("c").toString());
        p("subSet", s.subSet("b", "d").toString());
        t("subSet from > to", () -> s.subSet("d", "b"));
        SortedSet<String> ss = s.subSet("b", "d");
        t("set view add out of range", () -> ss.add("z"));
        p("set view add in range", ss.add("bb"));
        p("source saw set view add", s.toString());
        p("descendingSet", s.descendingSet().toString());
        p("descendingIterator first", s.descendingIterator().next());
    }

    // ------------------------------------------------------------------
    // 4. TreeSet's own surface
    // ------------------------------------------------------------------
    static void treeSet() {
        TreeSet<String> e = new TreeSet<>();
        t("empty first", () -> e.first());
        t("empty last", () -> e.last());
        p("empty pollFirst", e.pollFirst());
        p("empty pollLast", e.pollLast());
        p("empty ceiling", e.ceiling("a"));
        p("empty isEmpty", e.isEmpty());
        t("empty add null natural", () -> e.add(null));
        t("empty contains null natural", () -> e.contains(null));
        t("empty remove null natural", () -> e.remove(null));

        TreeSet<String> s = new TreeSet<>();
        p("add new", s.add("b"));
        p("add duplicate", s.add("b"));
        p("size after duplicate", s.size());
        s.add("a"); s.add("c");
        p("order", s.toString());
        p("first", s.first());
        p("last", s.last());
        p("ceiling present", s.ceiling("b"));
        p("higher present", s.higher("b"));
        p("floor present", s.floor("b"));
        p("lower present", s.lower("b"));
        p("ceiling past end", s.ceiling("z"));
        p("lower past start", s.lower("0"));
        p("contains", s.contains("a"));
        p("remove present", s.remove("a"));
        p("remove absent", s.remove("a"));
        p("pollFirst", s.pollFirst());
        p("pollLast", s.pollLast());
        p("after polls", s.toString());

        // constructors
        TreeSet<String> fromColl = new TreeSet<>(Arrays.asList("c", "a", "b", "a"));
        p("ctor(Collection) sorts and dedups", fromColl.toString());
        t("ctor(Collection) null", () -> new TreeSet<String>((Collection<String>) null));
        t("ctor(Collection) with null element", () -> new TreeSet<>(Arrays.asList("a", null)));
        TreeSet<String> rvs = new TreeSet<>(Comparator.reverseOrder());
        rvs.addAll(Arrays.asList("a", "b", "c"));
        p("ctor(Comparator)", rvs.toString());
        TreeSet<String> fromSorted = new TreeSet<>(rvs);
        p("ctor(SortedSet) keeps comparator", fromSorted.toString());
        p("ctor(SortedSet) comparator copied", fromSorted.comparator() != null);
        TreeSet<String> fromPlain = new TreeSet<>((Collection<String>) rvs);
        p("ctor(Collection) drops comparator", fromPlain.toString());

        // addAll/retainAll/removeAll on a sorted set
        TreeSet<String> ops = new TreeSet<>(Arrays.asList("a", "b", "c", "d"));
        p("retainAll", ops.retainAll(Arrays.asList("b", "d", "z")));
        p("after retainAll", ops.toString());
        p("removeAll", ops.removeAll(Arrays.asList("b")));
        p("after removeAll", ops.toString());
        p("addAll returns true", ops.addAll(Arrays.asList("a", "d")));
        p("after addAll", ops.toString());
        p("addAll no change returns false", ops.addAll(Arrays.asList("a")));
        t("addAll null", () -> ops.addAll(null));
        p("containsAll", ops.containsAll(Arrays.asList("a", "d")));

        // iterator remove and the fail-fast contract
        TreeSet<String> it = new TreeSet<>(Arrays.asList("a", "b", "c"));
        Iterator<String> i = it.iterator();
        i.next();
        i.remove();
        p("iterator remove", it.toString());
        t("iterator remove twice", () -> i.remove());
        TreeSet<String> ff = new TreeSet<>(Arrays.asList("a", "b", "c"));
        t("fail fast on add during iteration", () -> {
            for (String x : ff) { ff.add("z" + x); }
        });
        TreeSet<String> ff2 = new TreeSet<>(Arrays.asList("a", "b", "c"));
        t("fail fast on remove during iteration", () -> {
            for (String x : ff2) { ff2.remove("a"); }
        });

        p("toArray", Arrays.toString(new TreeSet<>(Arrays.asList("c", "a")).toArray()));
        p("toArray typed", Arrays.toString(
            new TreeSet<>(Arrays.asList("c", "a")).toArray(new String[0])));
        p("clone", ((TreeSet<String>) new TreeSet<>(Arrays.asList("c", "a")).clone()).toString());
        p("clone is independent", cloneIndependent());
        p("clone keeps the comparator", cloneComparator());
        p("clone of empty", ((TreeSet<String>) new TreeSet<String>().clone()).toString());
        p("equals a HashSet", new TreeSet<>(Arrays.asList("a", "b"))
            .equals(new HashSet<>(Arrays.asList("b", "a"))));
        p("hashCode agrees with HashSet",
            new TreeSet<>(Arrays.asList("a", "b")).hashCode()
                == new HashSet<>(Arrays.asList("b", "a")).hashCode());
    }

    // ------------------------------------------------------------------
    // 5. TreeMap's Map surface and its constructors
    // ------------------------------------------------------------------
    static void mapSurface() {
        TreeMap<String, Integer> m = m5();
        p("put returns old", m.put("a", 11));
        p("put new returns null", m.put("f", 6));
        p("remove returns old", m.remove("f"));
        p("remove absent returns null", m.remove("f"));
        p("getOrDefault present", m.getOrDefault("a", 0));
        p("getOrDefault absent", m.getOrDefault("zz", 0));
        p("putIfAbsent present", m.putIfAbsent("a", 99));
        p("putIfAbsent absent", m.putIfAbsent("g", 7));
        p("computeIfAbsent", m.computeIfAbsent("h", k -> 8));
        p("computeIfAbsent null result", m.computeIfAbsent("i", k -> null));
        p("containsKey after null compute", m.containsKey("i"));
        p("merge", m.merge("a", 1, Integer::sum));
        p("replace", m.replace("a", 100));
        p("state", m.toString());

        // A mapper that structurally modifies the same map is a CME.
        TreeMap<String, Integer> cme = new TreeMap<>();
        cme.put("x", 1);
        t("computeIfAbsent mutating mapper", () -> cme.computeIfAbsent("y", k -> {
            cme.put("z", 9); return 2;
        }));

        p("headMap of empty after clear", new TreeMap<String, Integer>().headMap("a").isEmpty());

        // constructors
        Map<String, Integer> src = new LinkedHashMap<>();
        src.put("c", 3); src.put("a", 1);
        p("ctor(Map) sorts", new TreeMap<>(src).toString());
        t("ctor(Map) null", () -> new TreeMap<String, Integer>((Map<String, Integer>) null));
        t("ctor(Map) with null key", () -> {
            Map<String, Integer> bad = new HashMap<>();
            bad.put(null, 1);
            new TreeMap<>(bad);
        });
        TreeMap<String, Integer> rv = new TreeMap<>(Comparator.reverseOrder());
        rv.putAll(src);
        p("ctor(Comparator) then putAll", rv.toString());
        p("ctor(SortedMap) keeps comparator", new TreeMap<>((SortedMap<String, Integer>) rv).toString());
        p("ctor(Map) drops comparator", new TreeMap<>((Map<String, Integer>) rv).toString());
        p("clone", ((TreeMap<String, Integer>) m5().clone()).toString());
        p("equals a HashMap", m5().equals(new HashMap<>(m5())));
        p("hashCode agrees", m5().hashCode() == new HashMap<>(m5()).hashCode());

        // entrySet entry setValue DOES write through on a live entrySet
        TreeMap<String, Integer> ev = m5();
        for (Map.Entry<String, Integer> en : ev.entrySet()) {
            if (en.getKey().equals("a")) en.setValue(111);
        }
        p("entrySet setValue writes through", ev.get("a"));
        // keySet remove writes through
        TreeMap<String, Integer> ks = m5();
        p("keySet remove", ks.keySet().remove("a"));
        p("keySet remove wrote through", ks.toString());
        // values remove writes through
        TreeMap<String, Integer> vs = m5();
        p("values remove", vs.values().remove(Integer.valueOf(1)));
        p("values remove wrote through", vs.toString());
        t("keySet add unsupported", () -> ks.keySet().add("q"));

        // forEach / replaceAll in encounter order
        StringBuilder sb = new StringBuilder();
        m5().forEach((k, v) -> sb.append(k).append('=').append(v).append(';'));
        p("forEach order", sb.toString());
        TreeMap<String, Integer> ra = m5();
        ra.replaceAll((k, v) -> v * 10);
        p("replaceAll", ra.toString());
    }

    static String cloneIndependent() {
        TreeSet<String> a = new TreeSet<>(Arrays.asList("b", "a"));
        @SuppressWarnings("unchecked")
        TreeSet<String> b = (TreeSet<String>) a.clone();
        b.add("c");
        b.remove("a");
        return a + "/" + b;
    }
    static String cloneComparator() {
        TreeSet<String> a = new TreeSet<>(Comparator.reverseOrder());
        a.addAll(Arrays.asList("a", "b", "c"));
        @SuppressWarnings("unchecked")
        TreeSet<String> b = (TreeSet<String>) a.clone();
        b.add("d");
        return b + "/" + (b.comparator() != null);
    }
    public static void main(String[] args) {
        ordering();
        ends();
        ranges();
        treeSet();
        mapSurface();
        System.out.println("ROWS " + rows);
        System.out.println("DONE TreeShadowSweep");
    }
}
