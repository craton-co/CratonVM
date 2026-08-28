import java.util.*;

/** L3 tail / `java.util.LinkedHashMap` (14 rows) with its four view classes
 *  (`LinkedKeySet` 9, `LinkedValues` 8, `LinkedEntrySet` 7, three iterators),
 *  and `LinkedHashSet` (5).
 *
 *  A `LinkedHashMap` is a `HashMap` plus ONE extra invariant — the encounter
 *  order — and every registration in this family exists to serve it. So the
 *  edges are exactly the places where that order is decided rather than
 *  observed:
 *
 *    * re-`put`ting an existing key does NOT move it; `remove` then `put` does.
 *      An implementation backed by an insertion-ordered map that re-inserts on
 *      update gets this wrong and no single-write test can see it;
 *    * ACCESS order (`new LinkedHashMap<>(c, f, true)`) makes a plain `get`
 *      structurally reorder the map, so a read is a write — including
 *      `getOrDefault`, which the JDK deliberately does NOT count as an access
 *      while `get` does. Those two answering the same way is the bug;
 *    * `removeEldestEntry` turns the map into an LRU cache, and it is consulted
 *      after EVERY `put`, including one that overwrites;
 *    * the JDK 21 `SequencedMap`/`SequencedSet` doors — `putFirst`, `putLast`,
 *      `firstEntry`, `pollFirstEntry`, `reversed`, `addFirst`, `getLast` — are
 *      recent, and a `--jdk-only` VM that fabricates any of them will answer a
 *      plausible wrong thing rather than refuse.
 *
 *  DETERMINISM: encounter order is the specification here, so it is printed
 *  directly and never sorted.
 */
public class LinkedSequencedShadowSweep {
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
    static LinkedHashMap<String, Integer> m4() {
        LinkedHashMap<String, Integer> m = new LinkedHashMap<>();
        m.put("d", 4); m.put("b", 2); m.put("a", 1); m.put("c", 3);
        return m;
    }

    // ------------------------------------------------------------------
    // 1. insertion order and what does or does not disturb it
    // ------------------------------------------------------------------
    static void insertionOrder() {
        LinkedHashMap<String, Integer> m = m4();
        p("insertion order toString", m.toString());
        p("keySet order", m.keySet().toString());
        p("values order", m.values().toString());
        p("entrySet order", m.entrySet().toString());
        StringBuilder sb = new StringBuilder();
        m.forEach((k, v) -> sb.append(k).append('=').append(v).append(';'));
        p("forEach order", sb.toString());

        // an UPDATE keeps the original position
        m.put("d", 44);
        p("update keeps position", m.toString());
        // remove-then-put moves to the end
        m.remove("d");
        m.put("d", 4);
        p("remove then put moves to the end", m.toString());
        // putIfAbsent on a present key is not an insertion
        m.putIfAbsent("b", 99);
        p("putIfAbsent present keeps position", m.toString());
        // merge/compute on a present key keep position too
        m.merge("b", 1, Integer::sum);
        p("merge present keeps position", m.toString());
        m.compute("a", (k, v) -> v + 100);
        p("compute present keeps position", m.toString());
        m.computeIfAbsent("z", k -> 26);
        p("computeIfAbsent appends", m.toString());
        m.replace("b", 222);
        p("replace keeps position", m.toString());
        m.replaceAll((k, v) -> v * 2);
        p("replaceAll keeps order", m.toString());

        // null key and null value are legal, and the null key keeps its place
        LinkedHashMap<String, Integer> n = new LinkedHashMap<>();
        n.put("a", 1); n.put(null, 0); n.put("b", 2);
        p("null key in order", n.toString());
        p("get null key", n.get(null));
        p("containsKey null", n.containsKey(null));
        n.put("nv", null);
        p("null value", n.toString());
        p("containsValue null", n.containsValue(null));

        // constructors
        t("new LinkedHashMap(-1)", () -> new LinkedHashMap<String, Integer>(-1));
        t("new LinkedHashMap(16, -1f)", () -> new LinkedHashMap<String, Integer>(16, -1f));
        t("new LinkedHashMap(16, NaN)", () -> new LinkedHashMap<String, Integer>(16, Float.NaN));
        t("new LinkedHashMap(16, 0.75f, true)",
            () -> new LinkedHashMap<String, Integer>(16, 0.75f, true));
        t("new LinkedHashMap(null)", () -> new LinkedHashMap<String, Integer>(null));
        LinkedHashMap<String, Integer> src = m4();
        p("ctor(Map) copies order", new LinkedHashMap<>(src).toString());
        LinkedHashMap<String, Integer> pa = new LinkedHashMap<>();
        pa.putAll(src);
        p("putAll copies order", pa.toString());

        // LinkedHashSet
        LinkedHashSet<String> s = new LinkedHashSet<>(Arrays.asList("c", "a", "b", "a"));
        p("set insertion order and dedup", s.toString());
        p("re-add does not move", reAddSet());
        p("set add returns false for duplicate", s.add("a"));
        p("set remove then add moves to end", removeAddSet());
        p("set allows null", s.add(null));
        p("set order with null", s.toString());
        p("set contains null", s.contains(null));
        p("set equals a HashSet", new LinkedHashSet<>(Arrays.asList("a", "b"))
            .equals(new HashSet<>(Arrays.asList("b", "a"))));
        p("set hashCode agrees", new LinkedHashSet<>(Arrays.asList("a", "b")).hashCode()
            == new HashSet<>(Arrays.asList("b", "a")).hashCode());
        t("new LinkedHashSet(-1)", () -> new LinkedHashSet<String>(-1));
        t("new LinkedHashSet(null)", () -> new LinkedHashSet<String>((Collection<String>) null));
    }
    static String reAddSet() {
        LinkedHashSet<String> s = new LinkedHashSet<>(Arrays.asList("a", "b", "c"));
        s.add("a");
        return s.toString();
    }
    static String removeAddSet() {
        LinkedHashSet<String> s = new LinkedHashSet<>(Arrays.asList("a", "b", "c"));
        s.remove("a"); s.add("a");
        return s.toString();
    }

    // ------------------------------------------------------------------
    // 2. access order — where a read becomes a write
    // ------------------------------------------------------------------
    static void accessOrder() {
        LinkedHashMap<String, Integer> a = new LinkedHashMap<>(16, 0.75f, true);
        a.put("a", 1); a.put("b", 2); a.put("c", 3);
        p("access order initial", a.toString());
        a.get("a");
        p("get moved the entry to the end", a.toString());
        a.get("zz");
        p("a miss does not reorder", a.toString());
        // getOrDefault is documented NOT to count as an access.
        a.getOrDefault("b", 0);
        p("getOrDefault does not reorder", a.toString());
        a.put("b", 22);
        p("put counts as an access", a.toString());
        a.putIfAbsent("c", 99);
        p("putIfAbsent on a present key counts as an access", a.toString());
        a.containsKey("a");
        p("containsKey does not reorder", a.toString());
        a.merge("a", 1, Integer::sum);
        p("merge counts as an access", a.toString());
        a.replace("c", 33);
        p("replace counts as an access", a.toString());
        a.compute("a", (k, v) -> v);
        p("compute counts as an access", a.toString());
        a.forEach((k, v) -> { });
        p("forEach does not reorder", a.toString());
        // iterating an access-ordered map is not an access either
        StringBuilder sb = new StringBuilder();
        for (String k : a.keySet()) sb.append(k);
        p("iteration does not reorder", a.toString());
        p("iteration saw", sb.toString());

        // insertion-ordered maps ignore all of that
        LinkedHashMap<String, Integer> i = new LinkedHashMap<>(16, 0.75f, false);
        i.put("a", 1); i.put("b", 2);
        i.get("a");
        p("insertion order ignores get", i.toString());

        // an LRU cache: removeEldestEntry is consulted after every put
        LinkedHashMap<String, Integer> lru = new LinkedHashMap<String, Integer>(16, 0.75f, true) {
            protected boolean removeEldestEntry(Map.Entry<String, Integer> eldest) {
                return size() > 3;
            }
        };
        lru.put("a", 1); lru.put("b", 2); lru.put("c", 3);
        p("lru at capacity", lru.toString());
        lru.put("d", 4);
        p("lru evicted the eldest", lru.toString());
        lru.get("b");
        lru.put("e", 5);
        p("lru evicted the least recently used", lru.toString());
        lru.put("b", 22);
        p("lru overwrite still consults removeEldestEntry", lru.toString());
        p("lru size", lru.size());
    }

    // ------------------------------------------------------------------
    // 3. the views, and their write-through
    // ------------------------------------------------------------------
    static void views() {
        LinkedHashMap<String, Integer> m = m4();
        Set<String> ks = m.keySet();
        p("keySet is live", ks.toString());
        m.put("e", 5);
        p("keySet saw the write", ks.toString());
        p("keySet remove", ks.remove("d"));
        p("keySet remove wrote through", m.toString());
        t("keySet add unsupported", () -> ks.add("q"));
        p("keySet contains", ks.contains("a"));
        p("keySet size", ks.size());

        LinkedHashMap<String, Integer> m2 = m4();
        Collection<Integer> vs = m2.values();
        p("values order", vs.toString());
        p("values remove first match", vs.remove(Integer.valueOf(4)));
        p("values remove wrote through", m2.toString());
        p("values contains", vs.contains(Integer.valueOf(2)));

        LinkedHashMap<String, Integer> m3 = m4();
        Set<Map.Entry<String, Integer>> es = m3.entrySet();
        p("entrySet order", es.toString());
        for (Map.Entry<String, Integer> e : es) if (e.getKey().equals("b")) e.setValue(22);
        p("entry setValue wrote through", m3.toString());
        p("entrySet removeIf", es.removeIf(e -> e.getValue() == 22));
        p("entrySet removeIf wrote through", m3.toString());

        // iterator remove and fail-fast
        LinkedHashMap<String, Integer> it = m4();
        Iterator<String> i = it.keySet().iterator();
        i.next(); i.remove();
        p("keySet iterator remove", it.toString());
        LinkedHashMap<String, Integer> ff = m4();
        t("fail fast on put during iteration", () -> {
            for (String k : ff.keySet()) ff.put("z" + k, 0);
        });
        LinkedHashMap<String, Integer> ff2 = m4();
        t("fail fast on remove during iteration", () -> {
            for (String k : ff2.keySet()) ff2.remove("a");
        });
        // an access on an ACCESS-ORDERED map during iteration is structural
        LinkedHashMap<String, Integer> ao = new LinkedHashMap<>(16, 0.75f, true);
        ao.put("a", 1); ao.put("b", 2); ao.put("c", 3);
        t("access-order get during iteration is a CME", () -> {
            for (String k : ao.keySet()) ao.get("a");
        });

        LinkedHashSet<String> s = new LinkedHashSet<>(Arrays.asList("a", "b", "c"));
        Iterator<String> si = s.iterator();
        si.next(); si.remove();
        p("set iterator remove", s.toString());
        t("set fail fast", () -> { for (String x : s) s.add("z"); });
    }

    // ------------------------------------------------------------------
    // 4. the JDK 21 SequencedMap / SequencedSet doors
    // ------------------------------------------------------------------
    static void sequenced() {
        LinkedHashMap<String, Integer> m = m4();
        tv("firstEntry", () -> m.firstEntry());
        tv("lastEntry", () -> m.lastEntry());
        tv("reversed", () -> m.reversed().toString());
        tv("sequencedKeySet reversed", () -> m.sequencedKeySet().reversed().toString());
        tv("sequencedValues reversed", () -> m.sequencedValues().reversed().toString());
        tv("sequencedEntrySet first", () -> m.sequencedEntrySet().iterator().next());
        LinkedHashMap<String, Integer> pf = m4();
        tv("putFirst new key", () -> { pf.putFirst("z", 26); return pf.toString(); });
        tv("putFirst existing key moves it", () -> { pf.putFirst("b", 2); return pf.toString(); });
        LinkedHashMap<String, Integer> pl = m4();
        tv("putLast existing key moves it", () -> { pl.putLast("d", 4); return pl.toString(); });
        LinkedHashMap<String, Integer> po = m4();
        tv("pollFirstEntry", () -> po.pollFirstEntry());
        tv("pollLastEntry", () -> po.pollLastEntry());
        tv("after polls", () -> po.toString());
        LinkedHashMap<String, Integer> e = new LinkedHashMap<>();
        tv("empty firstEntry", () -> e.firstEntry());
        tv("empty pollFirstEntry", () -> e.pollFirstEntry());
        tv("reversed of empty", () -> e.reversed().toString());
        tv("reversed is a live view", () -> {
            LinkedHashMap<String, Integer> r = m4();
            Map<String, Integer> rev = r.reversed();
            r.put("e", 5);
            return rev.toString();
        });

        LinkedHashSet<String> s = new LinkedHashSet<>(Arrays.asList("a", "b", "c"));
        tv("set getFirst", () -> s.getFirst());
        tv("set getLast", () -> s.getLast());
        tv("set reversed", () -> s.reversed().toString());
        tv("set addFirst", () -> { LinkedHashSet<String> x =
                new LinkedHashSet<>(Arrays.asList("a", "b")); x.addFirst("z"); return x.toString(); });
        tv("set addFirst existing moves it", () -> { LinkedHashSet<String> x =
                new LinkedHashSet<>(Arrays.asList("a", "b")); x.addFirst("b"); return x.toString(); });
        tv("set addLast existing moves it", () -> { LinkedHashSet<String> x =
                new LinkedHashSet<>(Arrays.asList("a", "b")); x.addLast("a"); return x.toString(); });
        tv("set removeFirst", () -> { LinkedHashSet<String> x =
                new LinkedHashSet<>(Arrays.asList("a", "b")); return x.removeFirst() + " " + x; });
        tv("set removeLast", () -> { LinkedHashSet<String> x =
                new LinkedHashSet<>(Arrays.asList("a", "b")); return x.removeLast() + " " + x; });
        LinkedHashSet<String> es = new LinkedHashSet<>();
        tv("empty set getFirst", () -> es.getFirst());
        tv("empty set removeFirst", () -> es.removeFirst());
    }

    public static void main(String[] args) {
        insertionOrder();
        accessOrder();
        views();
        sequenced();
        System.out.println("ROWS " + rows);
        System.out.println("DONE LinkedSequencedShadowSweep");
    }
}
