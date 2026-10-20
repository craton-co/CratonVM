import java.io.*;
import java.lang.reflect.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * nio21 wave 1 lane L4: conformance and bench rows for the two `--compatible` rows the lane
 * added, both default OFF:
 *
 *  * `jdk-concurrent-hash-map` -- `ConcurrentHashMap` (with its views) and `Properties`, the
 *    PHASE3 pair, plus `System.setProperty` / `clearProperty` / `getProperties`, run the JDK's
 *    bytecode as under `--jdk-only`; `native-collections` reads the JDK table where it used to
 *    read the segmented store (`CRATONVM_CHM_JDK_TABLE_READS`, default on, `=0` reverts).
 *  * `jdk-scalar-atomics` -- the retired rows of `AtomicInteger` / `AtomicLong` /
 *    `AtomicReference` / `AtomicBoolean`.
 *
 * Every conformance line is a fact (sizes, hits, String-keyed orders, exception types and
 * messages), so the output diffs against HotSpot 25.
 *
 * Run:
 *   javac -d out tools/probes/Nio21ChmPair.java
 *   java -cp out Nio21ChmPair > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21ChmPair > jo.txt
 *   cratonvm --compatible -cp out Nio21ChmPair > co.txt
 *   CRATONVM_COMPAT_ALIGN=+jdk-concurrent-hash-map cratonvm --compatible -cp out Nio21ChmPair > chm.txt
 *   CRATONVM_COMPAT_ALIGN=+jdk-scalar-atomics      cratonvm --compatible -cp out Nio21ChmPair > at.txt
 *   CRATONVM_CHM_JDK_TABLE_READS=0 cratonvm --jdk-only -cp out Nio21ChmPair > jo0.txt
 * hs, jo, co and at must be identical. chm must be identical too once the system-properties arms
 * take the row (the `sys*` rows are the ones that depend on them; see
 * docs/internal/fixed-bugs/r21-compatible-chm-row-needs-the-system-properties-arms-FIXED-20261008.md).
 * -Dnio21.rows=a,b selects rows.
 *
 * Bench (`<row> <N> ns/op`, printed as each row finishes): -Dnio21.bench=true, rows bench-chmstr
 * bench-chmlocale bench-chmput bench-chmiface bench-props bench-sysprop bench-atomic
 * (-Dnio21.iters=N default 200000, -Dnio21.warm=N default 3). Interleave `--jdk-only`,
 * `--compatible`, and `--compatible` with each row, 5 runs each.
 */
public class Nio21ChmPair {
    static String rows = System.getProperty("nio21.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    static class Throws { @Override public int hashCode() { throw new IllegalStateException("h"); } }

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        if (Boolean.getBoolean("nio21.bench")) { bench(); return; }
        String[] all = {"basic", "iface", "copy", "views", "bulk", "ser", "props", "sysprops",
                "sysreplace", "nulls", "atomics"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio21ChmPair.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    static ConcurrentHashMap<String, Integer> filled(int n) {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < n; i++) m.put("k" + i, i);
        return m;
    }

    static void basic() {
        ConcurrentHashMap<String, Integer> m = filled(20);
        System.out.println("basic size " + m.size() + " get " + m.get("k7") + " miss " + m.get("zz"));
        System.out.println("basic order " + m);
        System.out.println("basic pia " + m.putIfAbsent("k1", 99) + " " + m.putIfAbsent("new", 5));
        System.out.println("basic compute " + m.compute("k2", (k, v) -> v + 100) + " merge "
                + m.merge("k3", 10, Integer::sum) + " cia " + m.computeIfAbsent("abs", k -> 1)
                + " cip " + m.computeIfPresent("none", (k, v) -> 1));
        System.out.println("basic remove " + m.remove("k4") + " " + m.remove("k5", 0) + " " + m.remove("k5", 5)
                + " replace " + m.replace("k6", 60) + " " + m.replace("k7", 7, 70));
        System.out.println("basic after " + m.size() + " " + m.containsKey("k4") + " " + m.containsValue(70)
                + " " + m.getOrDefault("k4", -1) + " " + m.mappingCount());
        // A resize under load: 5 000 keys, then every one read back.
        ConcurrentHashMap<String, Integer> big = filled(5000);
        int hits = 0;
        for (int i = 0; i < 5000; i++) if (big.get("k" + i) == i) hits++;
        System.out.println("basic big " + big.size() + " hits " + hits + " hash " + big.hashCode());
    }

    static void iface() {
        Map<String, Integer> m = filled(12);
        ConcurrentMap<String, Integer> cm = filled(12);
        System.out.println("iface map " + m.size() + " " + m.get("k3") + " " + m.containsKey("k11")
                + " " + m.put("k3", 33) + " " + m.remove("k0") + " " + m.isEmpty());
        System.out.println("iface cmap " + cm.size() + " " + cm.get("k3") + " " + cm.containsKey("k11")
                + " " + cm.put("k3", 33) + " " + cm.remove("k0") + " " + cm.putIfAbsent("k1", 0));
        StringBuilder sb = new StringBuilder();
        m.forEach((k, v) -> sb.append(k).append('=').append(v).append(';'));
        System.out.println("iface forEach " + sb);
        System.out.println("iface views " + m.keySet() + " " + m.values() + " " + m.entrySet().size()
                + " " + m.keySet().getClass().getName() + " " + m.values().getClass().getName()
                + " " + m.entrySet().getClass().getName());
        m.clear();
        System.out.println("iface clear " + m.size() + " " + m);
    }

    static void copy() {
        ConcurrentHashMap<String, Integer> m = filled(15);
        System.out.println("copy tree " + new TreeMap<>(m));
        System.out.println("copy list " + new ArrayList<>(m.values()));
        System.out.println("copy hm " + new HashMap<>(m).equals(m) + " " + m.equals(new HashMap<>(m)));
        System.out.println("copy keys " + String.join(",", new TreeSet<>(m.keySet())));
        System.out.println("copy unmod " + Collections.unmodifiableMap(m).entrySet().size() + " "
                + Collections.unmodifiableMap(m).get("k9"));
        System.out.println("copy of " + List.copyOf(m.keySet()).size() + " " + Map.copyOf(m).size()
                + " " + Set.copyOf(m.values()).size());
        LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        lhm.putAll(m);
        System.out.println("copy lhm " + lhm);
        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.putAll(m);
        System.out.println("copy putAll " + tm.size() + " " + tm.firstKey() + " " + tm.lastKey());
    }

    static void views() {
        ConcurrentHashMap<String, Integer> m = filled(8);
        ConcurrentHashMap.KeySetView<String, Integer> ks = m.keySet(-1);
        System.out.println("views add " + ks.add("added") + " " + ks.add("added") + " " + m.get("added")
                + " mapped " + ks.getMappedValue());
        for (Map.Entry<String, Integer> e : m.entrySet()) if (e.getKey().equals("k2")) e.setValue(200);
        System.out.println("views setValue " + m.get("k2"));
        System.out.println("views values.remove " + m.values().remove(3) + " " + m.containsKey("k3"));
        Iterator<String> it = m.keySet().iterator();
        while (it.hasNext()) if (it.next().equals("k4")) it.remove();
        System.out.println("views itremove " + m.containsKey("k4") + " " + m.size());
        int n = 0;
        for (Enumeration<String> e = m.keys(); e.hasMoreElements(); e.nextElement()) n++;
        int v = 0;
        for (Enumeration<Integer> e = m.elements(); e.hasMoreElements(); e.nextElement()) v++;
        System.out.println("views enum " + n + " " + v);
        Set<String> nks = ConcurrentHashMap.newKeySet();
        nks.add("a"); nks.add("b"); nks.add("a");
        System.out.println("views newKeySet " + nks.size() + " " + nks.contains("b") + " " + nks.getClass().getName());
        Set<String> sfm = Collections.newSetFromMap(new ConcurrentHashMap<>());
        sfm.add("x"); sfm.add("y");
        System.out.println("views setFromMap " + sfm.size() + " " + new TreeSet<>(sfm));
    }

    static void bulk() {
        ConcurrentHashMap<String, Integer> m = filled(30);
        System.out.println("bulk reduce " + m.reduceValues(1, Integer::sum) + " " + m.reduceValuesToLong(1, Integer::longValue, 0L, Long::sum));
        System.out.println("bulk search " + m.search(1, (k, v) -> v == 17 ? k : null));
        long[] c = {0};
        m.forEach(1, (k, v) -> { synchronized (c) { c[0] += v; } });
        System.out.println("bulk forEach " + c[0]);
        System.out.println("bulk keys " + m.reduceKeys(1, (x, y) -> x.compareTo(y) < 0 ? x : y));
    }

    static void ser() throws Exception {
        ConcurrentHashMap<String, Integer> m = filled(10);
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (ObjectOutputStream oo = new ObjectOutputStream(bo)) { oo.writeObject(m); }
        @SuppressWarnings("unchecked")
        ConcurrentHashMap<String, Integer> back = (ConcurrentHashMap<String, Integer>)
                new ObjectInputStream(new ByteArrayInputStream(bo.toByteArray())).readObject();
        System.out.println("ser " + back.size() + " " + back.equals(m) + " " + back);
    }

    static void props() throws Exception {
        Properties d = new Properties();
        d.setProperty("def", "D");
        Properties p = new Properties(d);
        p.setProperty("b", "2");
        p.setProperty("a", "1");
        p.put("c", "3");
        System.out.println("props get " + p.getProperty("a") + " " + p.getProperty("def") + " "
                + p.getProperty("zz", "dflt") + " " + p.get("def") + " " + p.size());
        System.out.println("props names " + new TreeSet<>(p.stringPropertyNames()));
        int n = 0;
        for (Enumeration<?> e = p.propertyNames(); e.hasMoreElements(); e.nextElement()) n++;
        System.out.println("props propertyNames " + n);
        Properties q = new Properties();
        q.load(new StringReader("x=1\ny = two\\\n  lines\n# c\n!d\nz:3\n"));
        System.out.println("props load " + new TreeMap<>(q));
        StringWriter sw = new StringWriter();
        q.store(sw, null);
        String[] lines = sw.toString().split("\\R");
        List<String> body = new ArrayList<>();
        for (String l : lines) if (!l.startsWith("#")) body.add(l);
        Collections.sort(body);
        System.out.println("props store " + body);
        System.out.println("props copy " + new TreeMap<>(new HashMap<>(p)) + " clone " + ((Properties) p.clone()).size());
    }

    static void sysprops() {
        System.setProperty("nio21.x", "1");
        System.out.println("sysprops set " + System.getProperty("nio21.x") + " "
                + System.getProperties().getProperty("nio21.x"));
        System.getProperties().setProperty("nio21.y", "2");
        System.out.println("sysprops through " + System.getProperty("nio21.y") + " " + Integer.getInteger("nio21.y"));
        System.out.println("sysprops clear " + System.clearProperty("nio21.x") + " " + System.getProperty("nio21.x")
                + " " + System.getProperties().containsKey("nio21.x"));
        System.out.println("sysprops same " + (System.getProperties() == System.getProperties())
                + " javaver " + (System.getProperty("java.version") != null)
                + " default " + System.getProperty("nio21.none", "d"));
        System.setProperty("nio21.bool", "true");
        System.out.println("sysprops bool " + Boolean.getBoolean("nio21.bool") + " " + Long.getLong("nio21.y"));
    }

    static void sysreplace() {
        Properties saved = System.getProperties();
        Properties copy = new Properties();
        copy.putAll(saved);
        copy.setProperty("nio21.only", "here");
        System.setProperties(copy);
        System.out.println("sysreplace " + System.getProperty("nio21.only") + " " + (System.getProperties() == copy)
                + " " + (System.getProperty("java.version") != null));
        System.setProperties(saved);
        System.out.println("sysreplace back " + System.getProperty("nio21.only"));
    }

    static void nulls() {
        ConcurrentHashMap<Object, Object> m = new ConcurrentHashMap<>();
        try { m.get(null); } catch (Throwable t) { System.out.println("nulls get " + ex(t)); }
        try { m.put(null, 1); } catch (Throwable t) { System.out.println("nulls put " + ex(t)); }
        try { m.put(1, null); } catch (Throwable t) { System.out.println("nulls putv " + ex(t)); }
        try { m.put(new Throws(), 1); } catch (Throwable t) { System.out.println("nulls hash " + ex(t)); }
        Map<Object, Object> im = m;
        try { im.containsKey(null); } catch (Throwable t) { System.out.println("nulls iface " + ex(t)); }
        try { new Properties().setProperty(null, "v"); } catch (Throwable t) { System.out.println("nulls props " + ex(t)); }
        try { System.setProperty("", "v"); } catch (Throwable t) { System.out.println("nulls sys " + ex(t)); }
    }

    static void atomics() {
        AtomicInteger ai = new AtomicInteger(5);
        AtomicLong al = new AtomicLong();
        AtomicReference<String> ar = new AtomicReference<>("a");
        AtomicBoolean ab = new AtomicBoolean(true);
        System.out.println("atomics int " + ai.incrementAndGet() + " " + ai.getAndAdd(10) + " " + ai.compareAndSet(16, 1)
                + " " + ai.compareAndSet(0, 2) + " " + ai.getAndUpdate(x -> x * 3) + " " + ai + " " + ai.longValue());
        System.out.println("atomics long " + al.addAndGet(1L << 40) + " " + al.decrementAndGet() + " " + al.getAndSet(7)
                + " " + al.weakCompareAndSetPlain(7, 8) + " " + al.accumulateAndGet(2, Long::sum) + " " + al);
        System.out.println("atomics ref " + ar.compareAndSet("a", "b") + " " + ar.compareAndSet("a", "c") + " " + ar.get()
                + " " + ar.getAndSet("d") + " " + ar.updateAndGet(s -> s + s) + " " + ar);
        ab.lazySet(false);
        System.out.println("atomics bool " + ab.get() + " " + ab.compareAndSet(false, true) + " " + ab.getAndSet(false) + " " + ab);
    }

    // ---- bench ---------------------------------------------------------------------------

    static volatile Object sink;
    static volatile long lsink;

    interface Op { void run(int i); }

    static void time(String row, int iters, Op op) {
        int warm = Integer.getInteger("nio21.warm", 3);
        for (int w = 0; w < warm; w++) for (int i = 0; i < iters; i++) op.run(i);
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) op.run(i);
        long ns = System.nanoTime() - t0;
        System.out.println(row + " " + (ns / iters) + " ns/op");
    }

    static void bench() {
        int iters = Integer.getInteger("nio21.iters", 200000);
        String[] strs = new String[16];
        ConcurrentHashMap<String, Integer> chmStr = new ConcurrentHashMap<>();
        for (int i = 0; i < strs.length; i++) { strs[i] = "key-" + i; chmStr.put(strs[i], i); }
        Map<String, Integer> iface = chmStr;
        Locale[] ls = {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN, Locale.ROOT, Locale.CHINA,
                Locale.forLanguageTag("de-CH"), Locale.forLanguageTag("sr-Latn-RS")};
        ConcurrentHashMap<Locale, Integer> chmLoc = new ConcurrentHashMap<>();
        for (int i = 0; i < ls.length; i++) chmLoc.put(ls[i], i);
        ConcurrentHashMap<String, Integer> chmPut = new ConcurrentHashMap<>();
        Properties props = new Properties();
        for (int i = 0; i < 16; i++) props.setProperty(strs[i], "v" + i);
        System.setProperty("nio21.bench.key", "v");
        AtomicLong counter = new AtomicLong();
        AtomicReference<Object> ref = new AtomicReference<>();
        String[] all = {"bench-chmstr", "bench-chmlocale", "bench-chmput", "bench-chmiface", "bench-props",
                "bench-sysprop", "bench-atomic"};
        for (String r : all) {
            if (!(on(r) || on(r.substring(6)))) continue;
            switch (r) {
                case "bench-chmstr" -> time(r, iters, i -> sink = chmStr.get(strs[i & 15]));
                case "bench-chmlocale" -> time(r, iters, i -> sink = chmLoc.get(ls[i & 7]));
                case "bench-chmput" -> time(r, iters, i -> sink = chmPut.put(strs[i & 15], i));
                case "bench-chmiface" -> time(r, iters, i -> sink = iface.get(strs[i & 15]));
                case "bench-props" -> time(r, iters, i -> sink = props.getProperty(strs[i & 15]));
                case "bench-sysprop" -> time(r, iters, i -> sink = System.getProperty("nio21.bench.key"));
                case "bench-atomic" -> time(r, iters, i -> {
                    lsink = counter.incrementAndGet();
                    Object o = ref.get();
                    ref.compareAndSet(o, strs[i & 15]);
                });
                default -> { }
            }
        }
    }
}
