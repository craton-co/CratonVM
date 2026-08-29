import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.*;

/** L6 — the `java.util.concurrent.ConcurrentHashMap` triples the
 *  `--jdk-only-report` marks `outcome=native-won`.
 *
 *  Method: ask the CONTRACT EDGES. The edge that dominates this family is the
 *  one the campaign has now hit three times: **CHM refuses null keys AND null
 *  values, and `HashMap` — right next door in the same registrar file —
 *  accepts both.** Every null-shaped row below exists because a shim that
 *  shares code with the `HashMap` family inherits the wrong answer for free.
 *
 *  DETERMINISM: hash order is unspecified, so every collection read is sorted
 *  before printing and no iteration order is asserted anywhere. Nothing prints
 *  a capacity, a table length, a timing or a thread name.
 *
 *  HARD CAPS: `keys()`/`elements()` are drained with a guard, because the
 *  failure this family is known to produce is an enumeration that never says
 *  `false` (`rjdkenumerations-is-red-on-dev-from-the-chm-values-cursor`), and
 *  an uncapped drain would hang the probe instead of reporting. Every iterator
 *  loop is capped for the same reason.
 */
public class ChmShadowSweep {
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
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    /** `t()` with a WATCHDOG, for the rows whose failure mode is a call that
     *  never returns rather than a wrong answer. The worker is a daemon so a
     *  wedged row cannot keep the VM alive at exit, and the bound is 20 s
     *  against a JDK that answers these in microseconds -- so `DID NOT RETURN`
     *  is a statement about the call, not about this host's load. */
    static void tw(String tag, ThrowingRun r) {
        final java.util.concurrent.atomic.AtomicReference<String> out =
            new java.util.concurrent.atomic.AtomicReference<>("DID NOT RETURN");
        Thread w = new Thread(() -> {
            try { r.run(); out.set("no-throw"); }
            catch (Throwable e) { out.set("THREW " + e.getClass().getName()); }
        });
        w.setDaemon(true);
        w.start();
        try { w.join(20000); } catch (InterruptedException ignored) { }
        p(tag, out.get());
    }

    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        int guard = 0;
        for (Object o : c) {
            l.add(String.valueOf(o));
            if (++guard > 4096) return "NEVER TERMINATED";
        }
        Collections.sort(l);
        return l.toString();
    }
    static String drain(Enumeration<?> e) { return drain(e, 64); }
    /** The cap has to EXCEED the map: a cap below the real element count
     *  reports a perfectly good enumeration as non-terminating, which is a
     *  probe defect that reads exactly like a VM one. The 800-entry map in
     *  `concurrentWriters` found this the honest way. */
    static String drain(Enumeration<?> e, int cap) {
        try {
            List<String> got = new ArrayList<>();
            int guard = 0;
            while (e.hasMoreElements()) {
                got.add(String.valueOf(e.nextElement()));
                if (++guard > cap) return "NEVER TERMINATED";
            }
            Collections.sort(got);
            return got.toString();
        } catch (Throwable x) {
            return "THREW " + x.getClass().getName();
        }
    }
    static ConcurrentHashMap<String, Integer> abc() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        m.put("a", 1); m.put("b", 2); m.put("c", 3);
        return m;
    }

    // ---- <init>: three guards before anything else -------------------------
    static void construction() {
        p("new CHM isEmpty", new ConcurrentHashMap<>().isEmpty());
        p("new CHM size", new ConcurrentHashMap<>().size());
        p("new CHM mappingCount", new ConcurrentHashMap<>().mappingCount());
        t("new CHM(0)", () -> new ConcurrentHashMap<>(0));
        t("new CHM(-1)", () -> new ConcurrentHashMap<>(-1));
        t("new CHM(Integer.MIN_VALUE)", () -> new ConcurrentHashMap<>(Integer.MIN_VALUE));
        t("new CHM(16, 0f)", () -> new ConcurrentHashMap<>(16, 0f));
        t("new CHM(16, -1f)", () -> new ConcurrentHashMap<>(16, -1f));
        // NaN has to be spelled out: every comparison against NaN is false, so
        // a `<= 0` test does not catch it and the threshold becomes NaN.
        t("new CHM(16, NaN)", () -> new ConcurrentHashMap<>(16, Float.NaN));
        t("new CHM(16, 0.75f, 0)", () -> new ConcurrentHashMap<>(16, 0.75f, 0));
        t("new CHM(16, 0.75f, -1)", () -> new ConcurrentHashMap<>(16, 0.75f, -1));
        t("new CHM(-1, 0.75f, 1)", () -> new ConcurrentHashMap<>(-1, 0.75f, 1));
        t("new CHM((Map) null)", () -> new ConcurrentHashMap<Object, Object>(null));
        Map<String, Integer> src = new LinkedHashMap<>();
        src.put("x", 1); src.put("y", 2);
        p("new CHM(Map) contents", sorted(new ConcurrentHashMap<>(src).entrySet()));
        Map<String, Integer> withNullValue = new LinkedHashMap<>();
        withNullValue.put("k", null);
        t("new CHM(Map with a null value)", () -> new ConcurrentHashMap<>(withNullValue));
        Map<String, Integer> withNullKey = new LinkedHashMap<>();
        withNullKey.put(null, 1);
        t("new CHM(Map with a null key)", () -> new ConcurrentHashMap<>(withNullKey));
    }

    // ---- the null axis: THE trap of this family -----------------------------
    static void nullAxis() {
        ConcurrentHashMap<String, Integer> m = abc();
        t("put(null, 1)", () -> m.put(null, 1));
        t("put(\"k\", null)", () -> m.put("k", null));
        t("put(null, null)", () -> m.put(null, null));
        t("putIfAbsent(null, 1)", () -> m.putIfAbsent(null, 1));
        t("putIfAbsent(\"k\", null)", () -> m.putIfAbsent("k", null));
        t("get(null)", () -> m.get(null));
        t("getOrDefault(null, 9)", () -> m.getOrDefault(null, 9));
        t("containsKey(null)", () -> m.containsKey(null));
        t("containsValue(null)", () -> m.containsValue(null));
        t("contains(null)", () -> m.contains(null));
        t("remove(null)", () -> m.remove(null));
        t("remove(null, 1)", () -> m.remove(null, 1));
        t("remove(\"a\", null)", () -> m.remove("a", null));
        t("replace(null, 1)", () -> m.replace(null, 1));
        t("replace(\"a\", null)", () -> m.replace("a", null));
        t("replace(null, 1, 2)", () -> m.replace(null, 1, 2));
        t("replace(\"a\", null, 2)", () -> m.replace("a", null, 2));
        t("replace(\"a\", 1, null)", () -> m.replace("a", 1, null));
        t("merge(null, 1, f)", () -> m.merge(null, 1, (x, y) -> x));
        t("merge(\"a\", null, f)", () -> m.merge("a", null, (x, y) -> x));
        t("merge(\"a\", 1, null)", () -> m.merge("a", 1, null));
        t("compute(null, f)", () -> m.compute(null, (k, v) -> 1));
        t("compute(\"a\", null)", () -> m.compute("a", null));
        t("computeIfAbsent(null, f)", () -> m.computeIfAbsent(null, k -> 1));
        t("computeIfAbsent(\"a\", null)", () -> m.computeIfAbsent("a", null));
        t("computeIfPresent(null, f)", () -> m.computeIfPresent(null, (k, v) -> 1));
        t("computeIfPresent(\"a\", null)", () -> m.computeIfPresent("a", null));
        t("putAll(null)", () -> m.putAll(null));
        t("forEach(null)", () -> m.forEach((BiConsumer<String, Integer>) null));
        t("replaceAll(null)", () -> m.replaceAll(null));
        p("map is unchanged by every refusal above", sorted(m.entrySet()));
        p("size unchanged", m.size());
    }

    // ---- put / get / remove / replace ---------------------------------------
    static void basics() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        p("put returns old (none)", m.put("a", 1));
        p("put returns old (present)", m.put("a", 2));
        p("get", m.get("a"));
        p("getOrDefault present", m.getOrDefault("a", 99));
        p("getOrDefault absent", m.getOrDefault("zz", 99));
        p("containsKey present", m.containsKey("a"));
        p("containsKey absent", m.containsKey("zz"));
        p("containsValue present", m.containsValue(2));
        p("containsValue absent", m.containsValue(999));
        p("contains (Hashtable alias) present", m.contains(2));
        p("putIfAbsent when present", m.putIfAbsent("a", 5));
        p("putIfAbsent when absent", m.putIfAbsent("b", 5));
        p("get after putIfAbsent", m.get("b"));
        p("remove present", m.remove("b"));
        p("remove absent", m.remove("b"));
        p("remove(k, v) wrong value", m.remove("a", 999));
        p("remove(k, v) right value", m.remove("a", 2));
        p("size after removes", m.size());
        m.put("a", 1);
        p("replace(k, v) present", m.replace("a", 7));
        p("replace(k, v) absent", m.replace("zz", 7));
        p("replace(k, old, new) wrong old", m.replace("a", 1, 8));
        p("replace(k, old, new) right old", m.replace("a", 7, 8));
        p("get after replace", m.get("a"));
        p("isEmpty", m.isEmpty());
        p("mappingCount", m.mappingCount());
        m.clear();
        p("isEmpty after clear", m.isEmpty());
        p("size after clear", m.size());
        p("mappingCount after clear", m.mappingCount());
        ConcurrentHashMap<String, Integer> src = abc();
        ConcurrentHashMap<String, Integer> dst = new ConcurrentHashMap<>();
        dst.putAll(src);
        p("putAll contents", sorted(dst.entrySet()));
        dst.putAll(new HashMap<String, Integer>());
        p("putAll empty leaves size", dst.size());
        p("toString of an empty map", new ConcurrentHashMap<>().toString());
        p("equals a HashMap with the same contents",
          abc().equals(new HashMap<>(Map.of("a", 1, "b", 2, "c", 3))));
        p("hashCode matches an equal HashMap",
          abc().hashCode() == new HashMap<>(Map.of("a", 1, "b", 2, "c", 3)).hashCode());
        p("equals(null)", abc().equals(null));
        p("equals itself", equalsSelf());
    }
    static boolean equalsSelf() { ConcurrentHashMap<String, Integer> m = abc(); return m.equals(m); }

    // ---- compute / merge: the callback boundary ------------------------------
    static void callbacks() {
        ConcurrentHashMap<String, Integer> m = abc();
        p("computeIfAbsent when present", m.computeIfAbsent("a", k -> 99));
        p("computeIfAbsent when absent", m.computeIfAbsent("d", k -> 4));
        p("get after computeIfAbsent", m.get("d"));
        // A mapper returning null stores NOTHING.
        p("computeIfAbsent mapper returns null", m.computeIfAbsent("e", k -> null));
        p("containsKey after a null-returning mapper", m.containsKey("e"));
        p("size after a null-returning mapper", m.size());
        // A throwing mapper propagates and leaves the map untouched.
        t("computeIfAbsent mapper throws",
          () -> m.computeIfAbsent("f", k -> { throw new IllegalStateException("l6"); }));
        p("containsKey after a throwing mapper", m.containsKey("f"));

        // computeIfPresent
        p("computeIfPresent when absent", m.computeIfPresent("zz", (k, v) -> 1));
        p("computeIfPresent when present", m.computeIfPresent("a", (k, v) -> v + 10));
        p("computeIfPresent returning null removes", m.computeIfPresent("a", (k, v) -> null));
        p("containsKey after computeIfPresent removed it", m.containsKey("a"));

        // compute
        p("compute on an absent key", m.compute("g", (k, v) -> v == null ? 1 : v + 1));
        p("compute on a present key", m.compute("g", (k, v) -> v == null ? 1 : v + 1));
        p("compute returning null removes", m.compute("g", (k, v) -> null));
        p("containsKey after compute removed it", m.containsKey("g"));
        p("compute returning null on an absent key", m.compute("zz", (k, v) -> null));

        // merge
        p("merge on an absent key ignores the function", m.merge("h", 5, (a, b) -> 999));
        p("merge on a present key applies it", m.merge("h", 5, (a, b) -> a + b));
        p("merge returning null removes", m.merge("h", 5, (a, b) -> null));
        p("containsKey after merge removed it", m.containsKey("h"));

        // The RECURSIVE UPDATE guard. Modifying the same map from inside a
        // computeIfAbsent mapper is an IllegalStateException in the JDK, not a
        // silently corrupt table: the native decides the key is absent BEFORE
        // calling the mapper and writes its result AFTER.
        ConcurrentHashMap<String, Integer> r = new ConcurrentHashMap<>();
        r.put("x", 1);
        tw("computeIfAbsent that writes the same map",
           () -> r.computeIfAbsent("y", k -> { r.put("z", 9); return 2; }));
        // The JDK spells this one out: "Recursive update" is an
        // IllegalStateException, because the bin is already locked by this very
        // thread. A shim holding a real lock across the mapper self-deadlocks
        // here instead, which is why the row carries a watchdog.
        tw("computeIfAbsent that recurses on the SAME key",
           () -> r.computeIfAbsent("q", k -> r.computeIfAbsent("q", k2 -> 1)));
        tw("computeIfAbsent that recurses on a DIFFERENT key",
           () -> r.computeIfAbsent("q2", k -> r.computeIfAbsent("q3", k2 -> 1)));
        tw("compute that recurses on the SAME key",
           () -> r.compute("c1", (k, v) -> r.compute("c1", (k2, v2) -> 1)));
        tw("merge that recurses on the SAME key",
           () -> r.merge("m1", 1, (a, b) -> r.merge("m1", 2, (x, y) -> 3)));
        tw("compute that writes the same map",
           () -> r.compute("w", (k, v) -> { r.put("v", 9); return 2; }));
        tw("computeIfPresent that writes the same map",
           () -> { r.put("p1", 1); r.computeIfPresent("p1", (k, v) -> { r.put("p2", 9); return 2; }); });
        p("map after the recursive attempts", sorted(r.keySet()));

        // replaceAll and forEach
        ConcurrentHashMap<String, Integer> f = abc();
        f.replaceAll((k, v) -> v * 10);
        p("after replaceAll", sorted(f.entrySet()));
        final AtomicInteger seen = new AtomicInteger();
        f.forEach((k, v) -> seen.incrementAndGet());
        p("forEach visited", seen.get());
        t("replaceAll returning null", () -> abc().replaceAll((k, v) -> null));
        final List<String> ks = new ArrayList<>();
        f.forEach(1, (k, v) -> { synchronized (ks) { ks.add(k); } });
        Collections.sort(ks);
        p("parallel forEach visited", ks);
    }

    // ---- the views ------------------------------------------------------------
    static void views() {
        ConcurrentHashMap<String, Integer> m = abc();
        p("keySet contents", sorted(m.keySet()));
        p("values contents", sorted(m.values()));
        p("entrySet contents", sorted(m.entrySet()));
        p("keySet class", m.keySet().getClass().getName());
        p("values class", m.values().getClass().getName());
        p("entrySet class", m.entrySet().getClass().getName());
        p("keySet size", m.keySet().size());
        p("keySet contains", m.keySet().contains("a"));
        t("keySet contains(null)", () -> m.keySet().contains(null));
        t("values contains(null)", () -> m.values().contains(null));
        t("entrySet contains(null)", () -> m.entrySet().contains(null));
        // A plain keySet() view is READ-ONLY for additions but supports remove.
        t("keySet add", () -> m.keySet().add("zz"));
        p("keySet remove", m.keySet().remove("c"));
        p("map after keySet remove", sorted(m.keySet()));
        p("values remove", m.values().remove(2));
        p("map after values remove", sorted(m.keySet()));

        // keySet(defaultValue) IS addable, and that is the difference.
        ConcurrentHashMap<String, Integer> k = abc();
        ConcurrentHashMap.KeySetView<String, Integer> kv = k.keySet(9);
        p("keySet(v) class", kv.getClass().getName());
        p("keySet(v) getMappedValue", kv.getMappedValue());
        p("keySet(v) add", kv.add("d"));
        p("value of the added key", k.get("d"));
        p("keySet(v) add again", kv.add("d"));
        p("keySet(v) getMap is the map", kv.getMap() == k);
        t("keySet(null) mapped value", () -> abc().keySet(null));
        ConcurrentHashMap.KeySetView<String, Boolean> ns = ConcurrentHashMap.newKeySet();
        p("newKeySet is empty", ns.isEmpty());
        p("newKeySet add", ns.add("q"));
        p("newKeySet add again", ns.add("q"));
        p("newKeySet contains", ns.contains("q"));
        p("newKeySet size", ns.size());
        t("newKeySet add(null)", () -> ns.add(null));
        p("newKeySet(16) is empty", ConcurrentHashMap.newKeySet(16).isEmpty());
        t("newKeySet(-1)", () -> ConcurrentHashMap.newKeySet(-1));

        // Entry.setValue writes THROUGH to the map, and refuses null.
        ConcurrentHashMap<String, Integer> e = abc();
        Map.Entry<String, Integer> first = null;
        for (Map.Entry<String, Integer> x : e.entrySet()) {
            if ("a".equals(x.getKey())) { first = x; break; }
        }
        p("found the entry", first != null);
        if (first != null) {
            final Map.Entry<String, Integer> ent = first;
            p("entry class", ent.getClass().getName());
            p("setValue returns old", ent.setValue(77));
            p("map saw setValue", e.get("a"));
            t("setValue(null)", () -> ent.setValue(null));
        }
        // A view is a VIEW: a later put is visible through it.
        ConcurrentHashMap<String, Integer> v = abc();
        Set<String> live = v.keySet();
        v.put("d", 4);
        p("keySet saw the later put", sorted(live));
        v.remove("a");
        p("keySet saw the later remove", sorted(live));
        p("view isEmpty tracks the map", clearedViewIsEmpty());
    }
    static boolean clearedViewIsEmpty() {
        ConcurrentHashMap<String, Integer> m = abc();
        Collection<Integer> vals = m.values();
        m.clear();
        return vals.isEmpty();
    }

    // ---- iterators: weakly consistent, never fail-fast -------------------------
    static void iterators() {
        // A CHM iterator is WEAKLY CONSISTENT: a concurrent structural change
        // must NOT produce ConcurrentModificationException. That is the
        // opposite of the HashMap family sharing the same registrar.
        ConcurrentHashMap<String, Integer> m = abc();
        t("write during keySet iteration", () -> {
            int guard = 0;
            for (Iterator<String> it = m.keySet().iterator(); it.hasNext(); ) {
                it.next();
                m.put("added" + guard, guard);
                if (++guard > 8) break;
            }
        });
        ConcurrentHashMap<String, Integer> m2 = abc();
        t("clear during values iteration", () -> {
            int guard = 0;
            for (Iterator<Integer> it = m2.values().iterator(); it.hasNext(); ) {
                it.next();
                m2.clear();
                if (++guard > 8) break;
            }
        });
        ConcurrentHashMap<String, Integer> m3 = abc();
        Iterator<String> it = m3.keySet().iterator();
        p("keySet iterator class", it.getClass().getName());
        p("values iterator class", abc().values().iterator().getClass().getName());
        p("entrySet iterator class", abc().entrySet().iterator().getClass().getName());
        t("iterator remove before next", () -> abc().keySet().iterator().remove());
        ConcurrentHashMap<String, Integer> m4 = abc();
        Iterator<String> r = m4.keySet().iterator();
        r.next(); r.remove();
        p("size after iterator remove", m4.size());
        t("iterator remove twice", () -> {
            Iterator<String> q = abc().keySet().iterator();
            q.next(); q.remove(); q.remove();
        });
        // next() past the end is NoSuchElementException.
        t("next past the end", () -> {
            Iterator<String> q = new ConcurrentHashMap<String, Integer>().keySet().iterator();
            q.next();
        });
        p("empty map iterator hasNext",
          new ConcurrentHashMap<String, Integer>().keySet().iterator().hasNext());
        p("spliterator characteristics are CONCURRENT",
          (abc().keySet().spliterator().characteristics() & Spliterator.CONCURRENT) != 0);
        p("stream count", abc().keySet().stream().count());
        p("values stream sum", abc().values().stream().mapToInt(Integer::intValue).sum());
    }

    // ---- keys() and elements(): the Enumeration pair -----------------------
    static void enumerations() {
        ConcurrentHashMap<String, Integer> m = abc();
        p("keys()", drain(m.keys()));
        p("elements()", drain(m.elements()));
        p("keys() on an empty map", drain(new ConcurrentHashMap<>().keys()));
        p("elements() on an empty map", drain(new ConcurrentHashMap<>().elements()));
        ConcurrentHashMap<String, Integer> one = new ConcurrentHashMap<>();
        one.put("solo", 1);
        p("keys() on a one-entry map", drain(one.keys()));
        p("elements() on a one-entry map", drain(one.elements()));
        p("keys() class", m.keys().getClass().getName());
        p("elements() class", m.elements().getClass().getName());
        // nextElement past the end is NoSuchElementException.
        t("keys() nextElement past the end", () -> {
            Enumeration<String> e = new ConcurrentHashMap<String, Integer>().keys();
            e.nextElement();
        });
        t("elements() nextElement past the end", () -> {
            Enumeration<Integer> e = new ConcurrentHashMap<String, Integer>().elements();
            e.nextElement();
        });
        // A second, independent enumeration over the same map.
        p("two independent keys() drains agree",
          drain(m.keys()).equals(drain(m.keys())));
        p("elements() then keys() on one map", drain(m.elements()) + "/" + drain(m.keys()));
    }

    // ---- search / reduce ------------------------------------------------------
    static void bulkOps() {
        ConcurrentHashMap<String, Integer> m = abc();
        p("reduceValues sum", m.reduceValues(1, Integer::sum));
        p("reduceKeys concat length",
          m.reduceKeys(1, (a, b) -> a + b) == null ? "null"
              : String.valueOf(m.reduceKeys(1, (a, b) -> a + b).length()));
        p("reduceValuesToInt", m.reduceValuesToInt(1, Integer::intValue, 0, Integer::sum));
        p("reduceValuesToLong", m.reduceValuesToLong(1, Integer::longValue, 0L, Long::sum));
        p("searchValues finds", m.searchValues(1, v -> v == 2 ? "found" : null));
        p("searchValues misses", m.searchValues(1, v -> v == 999 ? "found" : null));
        p("searchKeys finds", m.searchKeys(1, k -> "b".equals(k) ? "found" : null));
        p("search over both", m.search(1, (k, v) -> v == 3 ? "found" : null));
        p("reduce on an empty map",
          new ConcurrentHashMap<String, Integer>().reduceValues(1, Integer::sum));
        p("reduceEntriesToInt count",
          m.reduceEntriesToInt(1, e -> 1, 0, Integer::sum));
        t("reduceValues(null reducer)", () -> m.reduceValues(1, null));
        t("searchValues(null)", () -> m.searchValues(1, null));
        p("forEachValue collected", forEachValueSorted(m));
        p("forEachKey with a transformer",
          m.reduceKeys(1, k -> k.toUpperCase(), (a, b) -> a.compareTo(b) <= 0 ? a : b));
        p("mappingCount on a 3-entry map", m.mappingCount());
    }
    static String forEachValueSorted(ConcurrentHashMap<String, Integer> m) {
        List<String> out = Collections.synchronizedList(new ArrayList<>());
        m.forEachValue(1, v -> out.add(String.valueOf(v)));
        List<String> copy = new ArrayList<>(out);
        Collections.sort(copy);
        return copy.toString();
    }

    // ---- concurrent writers, joined before anything is read -------------------
    static void concurrentWriters() throws Exception {
        final ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        final int W = 4, N = 200;
        Thread[] ts = new Thread[W];
        for (int w = 0; w < W; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                for (int i = 0; i < N; i++) m.put(id + ":" + i, i);
            });
        }
        for (Thread x : ts) x.start();
        for (Thread x : ts) x.join();
        p("all writers' entries are present", m.size());
        p("mappingCount agrees with size", m.mappingCount() == (long) m.size());
        // Every key readable back — a table that is a mirror rather than the
        // authority loses writes here.
        boolean all = true;
        for (int w = 0; w < W && all; w++) {
            for (int i = 0; i < N; i++) {
                Integer v = m.get(w + ":" + i);
                if (v == null || v != i) { all = false; break; }
            }
        }
        p("every written key reads back", all);
        p("keySet size matches", m.keySet().size());
        p("values size matches", m.values().size());
        p("entrySet size matches", m.entrySet().size());
        p("drain of a large map terminates",
          !drain(m.elements(), W * N + 1).equals("NEVER TERMINATED"));
        p("drain of a large map yields every element",
          drain(m.elements(), W * N + 1).split(",").length);
        p("iteration of a large map terminates",
          !sorted(m.keySet()).equals("NEVER TERMINATED"));

        // A concurrent computeIfAbsent that is a pure function of its key: the
        // JDK guarantees the mapper runs at most once per key, so the counter
        // is a deterministic N.
        final ConcurrentHashMap<Integer, Integer> c = new ConcurrentHashMap<>();
        final AtomicInteger calls = new AtomicInteger();
        Thread[] cs = new Thread[W];
        for (int w = 0; w < W; w++) {
            cs[w] = new Thread(() -> {
                for (int i = 0; i < 100; i++) {
                    c.computeIfAbsent(i, k -> { calls.incrementAndGet(); return k * 2; });
                }
            });
        }
        for (Thread x : cs) x.start();
        for (Thread x : cs) x.join();
        p("computeIfAbsent produced every key", c.size());
        p("computeIfAbsent mapper ran at most once per key", calls.get() == 100);
        boolean values = true;
        for (int i = 0; i < 100; i++) if (c.get(i) == null || c.get(i) != i * 2) values = false;
        p("computeIfAbsent values are correct", values);
    }

    public static void main(String[] args) throws Exception {
        construction();
        nullAxis();
        basics();
        callbacks();
        views();
        iterators();
        enumerations();
        bulkOps();
        concurrentWriters();
        System.out.println("rows " + rows + " DONE ChmShadowSweep");
    }
}
