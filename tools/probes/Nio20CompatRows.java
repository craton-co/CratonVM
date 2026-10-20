import java.lang.reflect.*;
import java.util.*;
import java.util.concurrent.*;

/**
 * nio20 wave 6 lane L5: conformance and bench rows for the `--compatible` hashed-key path the
 * lane changed in `native-collections` (`CRATONVM_COMPAT_IDENTITY_KEY_HASH`, default on):
 * an IDENTITY key (a class whose chain declares neither `hashCode()` nor `equals(Object)`:
 * `Class`, a bare `Object`, a plain user class, a lambda) is hashed and compared without the
 * re-entrant Java call, and a `Locale` key reads its cached `hashCodeValue` once set. The answers
 * must not move: every conformance line is a fact (sizes, hits, booleans, counts, exception
 * types), never an identity hash or an iteration order, so the output diffs against HotSpot 25.
 *
 * Run:
 *   javac -d out tools/probes/Nio20CompatRows.java
 *   java -cp out Nio20CompatRows > hs.txt
 *   cratonvm --jdk-only   -cp out Nio20CompatRows > jo.txt
 *   cratonvm --compatible -cp out Nio20CompatRows > co.txt
 *   CRATONVM_COMPAT_IDENTITY_KEY_HASH=0 cratonvm --compatible -cp out Nio20CompatRows > co0.txt
 * All four must be identical. -Dnio20cr.rows=a,b selects rows.
 *
 * Bench (`<row> <N> ns/op`, printed as each row finishes): -Dnio20cr.bench=true
 * (rows bench-chmclass bench-chmobj bench-chmlocale bench-chmuser bench-chmstr bench-hmclass,
 * -Dnio20cr.iters=N default 200000, -Dnio20cr.warm=N default 3). Interleave `--compatible`
 * with and without CRATONVM_COMPAT_IDENTITY_KEY_HASH=0, and `--jdk-only`, 5 runs each.
 * Expected: bench-chmclass / bench-chmobj / bench-chmlocale fall with the switch on;
 * bench-chmuser (a key with its own hashCode/equals) and bench-chmstr do not move.
 *
 * `jdk-print-writer` (default ON): the `pw*` conformance rows must match HotSpot in all arms,
 * and also with CRATONVM_COMPAT_ALIGN=-jdk-print-writer (the old natives; any line that differs
 * there and not with the row on is a defect the row fixed). Bench rows bench-pwsw / bench-pwbaos
 * (a PrintWriter over a StringWriter / ByteArrayOutputStream) against bench-sw (no PrintWriter,
 * the control): interleave `--compatible` with and without -jdk-print-writer and `--jdk-only`.
 */
public class Nio20CompatRows {
    static String rows = System.getProperty("nio20cr.rows", "all");
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static String ex(Throwable t) { return t.getClass().getName(); }

    /** No hashCode / equals anywhere below Object: an identity key. */
    static class Plain { final int id; Plain(int id) { this.id = id; } }
    static class PlainSub extends Plain { PlainSub(int id) { super(id); } }
    /** Content key. */
    static final class Point {
        final int x, y;
        Point(int x, int y) { this.x = x; this.y = y; }
        @Override public boolean equals(Object o) { return o instanceof Point p && p.x == x && p.y == y; }
        @Override public int hashCode() { return 31 * x + y; }
    }
    /** Inherits a content hashCode from its parent only. */
    static class HashOnly { @Override public int hashCode() { return 7; } }
    static class HashOnlySub extends HashOnly { }
    /** equals overridden, hashCode not: HotSpot uses the identity hash, then equals. */
    static class EqOnly {
        final int v; EqOnly(int v) { this.v = v; }
        @Override public boolean equals(Object o) { return o instanceof EqOnly e && e.v == v; }
    }
    /** A hashCode that throws: the exception must reach the caller in both modes. */
    static class Throws { @Override public int hashCode() { throw new IllegalStateException("h"); } }

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        if (Boolean.getBoolean("nio20cr.bench")) { bench(); return; }
        String[] all = {"chmid", "chmlocale", "chmcontent", "hmid", "htid", "chmviews", "chmthrow",
                "pwstring", "pwstream", "pwsub", "pwclosed"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio20CompatRows.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                System.out.println(r + " THREW " + ex(e.getCause()));
            }
        }
        System.out.println("done");
    }

    static void chmid() {
        ConcurrentHashMap<Object, String> m = new ConcurrentHashMap<>();
        Object o1 = new Object(), o2 = new Object();
        Plain p1 = new Plain(1), p2 = new Plain(1);
        PlainSub s1 = new PlainSub(1);
        Runnable lam = () -> { };
        Class<?>[] classes = {String.class, Integer.class, Plain.class, int.class, int[].class, Map.class};
        m.put(o1, "o1"); m.put(o2, "o2"); m.put(p1, "p1"); m.put(p2, "p2"); m.put(s1, "s1"); m.put(lam, "lam");
        for (Class<?> c : classes) m.put(c, c.getName());
        System.out.println("chmid size=" + m.size());
        System.out.println("chmid get o1=" + m.get(o1) + " o2=" + m.get(o2) + " p1=" + m.get(p1) + " p2=" + m.get(p2)
                + " s1=" + m.get(s1) + " lam=" + m.get(lam));
        System.out.println("chmid miss newObject=" + m.get(new Object()) + " newPlain=" + m.get(new Plain(1)));
        StringBuilder sb = new StringBuilder();
        for (Class<?> c : classes) sb.append(m.get(c)).append(';');
        System.out.println("chmid classes " + sb);
        System.out.println("chmid containsKey p1=" + m.containsKey(p1) + " fresh=" + m.containsKey(new Plain(2))
                + " Class.forName=" + m.containsKey(forName("java.lang.String")));
        System.out.println("chmid remove p2=" + m.remove(p2) + " again=" + m.remove(p2) + " size=" + m.size());
        System.out.println("chmid putIfAbsent p1=" + m.putIfAbsent(p1, "x") + " new=" + m.putIfAbsent(new Plain(3), "y")
                + " size=" + m.size());
        System.out.println("chmid computeIfAbsent class=" + m.computeIfAbsent(Long.class, k -> "L")
                + " again=" + m.computeIfAbsent(Long.class, k -> "L2") + " size=" + m.size());
        // Many identity keys: every one found, none spuriously equal to another.
        ConcurrentHashMap<Plain, Integer> big = new ConcurrentHashMap<>();
        Plain[] keys = new Plain[5000];
        for (int i = 0; i < keys.length; i++) { keys[i] = new Plain(i % 7); big.put(keys[i], i); }
        int hits = 0;
        for (int i = 0; i < keys.length; i++) if (big.get(keys[i]) == i) hits++;
        int strangers = 0;
        for (int i = 0; i < 1000; i++) if (big.get(new Plain(i % 7)) != null) strangers++;
        System.out.println("chmid big size=" + big.size() + " hits=" + hits + " strangers=" + strangers);
    }

    static Class<?> forName(String n) { try { return Class.forName(n); } catch (ClassNotFoundException e) { return null; } }

    static void chmlocale() {
        ConcurrentHashMap<Locale, String> m = new ConcurrentHashMap<>();
        Locale[] ls = {Locale.US, Locale.ROOT, Locale.GERMANY, Locale.forLanguageTag("sr-Latn-RS"),
                Locale.forLanguageTag("th-TH-u-nu-thai"), Locale.of("fr", "CA")};
        for (Locale l : ls) m.put(l, l.toLanguageTag());
        // Before any hashCode() call on these two: built fresh, equal to US / fr-CA.
        Locale us2 = new Locale.Builder().setLanguage("en").setRegion("US").build();
        Locale frca2 = Locale.forLanguageTag("fr-CA");
        Locale usClone = (Locale) Locale.US.clone();   // copies the cached hashCodeValue
        System.out.println("chmlocale size=" + m.size());
        System.out.println("chmlocale get us2=" + m.get(us2) + " frca2=" + m.get(frca2) + " clone=" + m.get(usClone)
                + " ja=" + m.get(Locale.JAPAN));
        System.out.println("chmlocale hash agree=" + (us2.hashCode() == Locale.US.hashCode())
                + " clone=" + (usClone.hashCode() == Locale.US.hashCode())
                + " equals=" + usClone.equals(Locale.US) + " same=" + (usClone == Locale.US));
        System.out.println("chmlocale containsKey ROOT=" + m.containsKey(Locale.ROOT) + " und="
                + m.containsKey(Locale.forLanguageTag("und")) + " remove clone=" + m.remove(usClone)
                + " size=" + m.size() + " gone=" + m.get(Locale.US));
        HashMap<Locale, Integer> hm = new HashMap<>();
        for (Locale l : Locale.getAvailableLocales()) hm.merge(l, 1, Integer::sum);
        int dup = 0;
        for (Integer v : hm.values()) if (v > 1) dup++;
        System.out.println("chmlocale available distinct=" + (hm.size() == Locale.getAvailableLocales().length)
                + " dup=" + dup);
    }

    static void chmcontent() {
        ConcurrentHashMap<Object, String> m = new ConcurrentHashMap<>();
        m.put(new Point(1, 2), "p12");
        m.put(new HashOnlySub(), "hos");
        EqOnly e1 = new EqOnly(5);
        m.put(e1, "e5");
        System.out.println("chmcontent point=" + m.get(new Point(1, 2)) + " hashOnly=" + m.get(new HashOnlySub())
                + " eqSame=" + m.get(e1) + " size=" + m.size());
        // EqOnly: equal but (almost surely) a different identity hash -> HotSpot misses unless the
        // identity hashes collide in the table; print only the same-object hit.
        System.out.println("chmcontent enum=" + new ConcurrentHashMap<>(Map.of(Thread.State.NEW, "n")).get(Thread.State.NEW));
    }

    static void hmid() {
        HashMap<Object, Integer> m = new HashMap<>();
        Object[] ks = new Object[300];
        for (int i = 0; i < ks.length; i++) { ks[i] = (i % 3 == 0) ? new Object() : (i % 3 == 1) ? new Plain(i) : new PlainSub(i); m.put(ks[i], i); }
        int hits = 0;
        for (int i = 0; i < ks.length; i++) if (m.get(ks[i]) == i) hits++;
        System.out.println("hmid size=" + m.size() + " hits=" + hits + " stranger=" + m.get(new Object())
                + " removed=" + m.remove(ks[7]) + " size=" + m.size());
        LinkedHashMap<Class<?>, String> lhm = new LinkedHashMap<>();
        lhm.put(String.class, "s"); lhm.put(Object.class, "o"); lhm.put(String.class, "s2");
        System.out.println("hmid lhm " + lhm.values());
        HashSet<Object> hs = new HashSet<>(Arrays.asList(ks));
        System.out.println("hmid set size=" + hs.size() + " contains=" + hs.contains(ks[10]) + " stranger="
                + hs.contains(new Plain(10)));
    }

    static void htid() {
        Hashtable<Object, Integer> t = new Hashtable<>();
        Plain a1 = new Plain(1), a2 = new Plain(1);
        t.put(a1, 1); t.put(a2, 2); t.put(Plain.class, 3);
        System.out.println("htid size=" + t.size() + " a1=" + t.get(a1) + " a2=" + t.get(a2) + " cls=" + t.get(Plain.class)
                + " stranger=" + t.get(new Plain(1)) + " contains=" + t.containsKey(a2));
        IdentityHashMap<Object, Integer> ih = new IdentityHashMap<>();
        Point q1 = new Point(0, 0), q2 = new Point(0, 0);
        ih.put(q1, 1); ih.put(q2, 2);
        System.out.println("htid identityHashMap size=" + ih.size() + " q1=" + ih.get(q1));
    }

    static void chmviews() {
        ConcurrentHashMap<Object, Integer> m = new ConcurrentHashMap<>();
        Plain[] ks = new Plain[64];
        for (int i = 0; i < ks.length; i++) { ks[i] = new Plain(i); m.put(ks[i], i); }
        int sum = 0;
        for (Integer v : m.values()) sum += v;
        int keyHits = 0;
        for (Object k : m.keySet()) if (m.get(k) != null) keyHits++;
        Set<Object> ks2 = ConcurrentHashMap.newKeySet();
        for (Plain p : ks) ks2.add(p);
        System.out.println("chmviews sum=" + sum + " keyHits=" + keyHits + " keySet=" + ks2.size()
                + " contains=" + ks2.contains(ks[3]) + " stranger=" + ks2.contains(new Plain(3)));
        m.replaceAll((k, v) -> v * 2);
        System.out.println("chmviews replaced=" + m.get(ks[10]) + " merge=" + m.merge(ks[10], 1, Integer::sum)
                + " compute=" + m.compute(new Plain(99), (k, v) -> v == null ? -1 : v));
    }

    static void chmthrow() {
        ConcurrentHashMap<Object, Integer> m = new ConcurrentHashMap<>();
        try { m.put(new Throws(), 1); System.out.println("chmthrow put no-throw"); }
        catch (Throwable t) { System.out.println("chmthrow put " + ex(t)); }
        try { m.get(new Throws()); System.out.println("chmthrow get no-throw"); }
        catch (Throwable t) { System.out.println("chmthrow get " + ex(t)); }
        try { m.get(null); System.out.println("chmthrow null no-throw"); }
        catch (Throwable t) { System.out.println("chmthrow null " + ex(t)); }
        System.out.println("chmthrow size=" + m.size());
    }

    // ---- PrintWriter (`jdk-print-writer`) ------------------------------------------------

    /** UTF-16 code units, escaped, so a separator or a lone surrogate is visible. */
    static String units(CharSequence s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f) b.append(c); else b.append(String.format("\\u%04x", (int) c));
        }
        return b.toString();
    }

    static String hex(byte[] bs) {
        StringBuilder b = new StringBuilder();
        for (byte x : bs) b.append(String.format("%02x", x & 0xff));
        return b.toString();
    }

    static void pwstring() {
        java.io.StringWriter sw = new java.io.StringWriter();
        java.io.PrintWriter pw = new java.io.PrintWriter(sw);
        pw.println("plain");
        pw.println();
        pw.println(42);
        pw.println((Object) null);
        pw.println("lone\uD800surrogate\uDC00x");
        pw.write("w(String)");
        pw.write("0123456789", 2, 3);
        pw.print('c');
        pw.printf("%d|%s", 7, "f");
        System.out.println("pwstring " + units(sw.getBuffer()) + " err=" + pw.checkError());
    }

    static void pwstream() {
        java.io.ByteArrayOutputStream baos = new java.io.ByteArrayOutputStream();
        java.io.PrintWriter pw = new java.io.PrintWriter(baos);
        pw.println("abcé\uD800z");
        System.out.println("pwstream beforeFlush=" + baos.size());
        pw.flush();
        System.out.println("pwstream " + hex(baos.toByteArray()));
        java.io.ByteArrayOutputStream auto = new java.io.ByteArrayOutputStream();
        java.io.PrintWriter apw = new java.io.PrintWriter(auto, true);
        apw.print("no-newline");
        int afterPrint = auto.size();
        apw.println("line");
        System.out.println("pwstream autoflush afterPrint=" + afterPrint + " afterPrintln=" + (auto.size() > 0));
        java.io.PrintWriter cs = new java.io.PrintWriter(new java.io.OutputStreamWriter(baos,
                java.nio.charset.StandardCharsets.UTF_16BE));
        baos.reset();
        cs.println("é");
        cs.flush();
        System.out.println("pwstream utf16be " + hex(baos.toByteArray()));
    }

    static void pwsub() {
        final int[] calls = new int[3];
        java.io.StringWriter sw = new java.io.StringWriter();
        java.io.PrintWriter pw = new java.io.PrintWriter(sw) {
            @Override public void write(String s, int off, int len) { calls[0]++; super.write(s, off, len); }
            @Override public void write(int c) { calls[1]++; super.write(c); }
            @Override public void flush() { calls[2]++; super.flush(); }
        };
        pw.println("one");
        pw.println(2);
        pw.println();
        pw.write("three");
        System.out.println("pwsub writeStr=" + calls[0] + " writeInt=" + calls[1] + " flush=" + calls[2]
                + " text=" + units(sw.getBuffer()));
    }

    static void pwclosed() {
        java.io.StringWriter sw = new java.io.StringWriter();
        java.io.PrintWriter pw = new java.io.PrintWriter(sw);
        pw.close();
        pw.println("after");
        pw.write("after2");
        System.out.println("pwclosed err=" + pw.checkError() + " text=" + units(sw.getBuffer()));
        java.io.PrintWriter bad = new java.io.PrintWriter(new java.io.Writer() {
            @Override public void write(char[] b, int o, int l) throws java.io.IOException { throw new java.io.IOException("x"); }
            @Override public void flush() { }
            @Override public void close() { }
        });
        bad.println("y");
        System.out.println("pwclosed ioerr=" + bad.checkError());
        try {
            new java.io.PrintWriter((java.io.OutputStream) null);
            System.out.println("pwclosed nullOut no-throw");
        } catch (Throwable t) {
            System.out.println("pwclosed nullOut " + ex(t));
        }
    }

    // ---- bench ---------------------------------------------------------------------------

    static volatile Object sink;

    interface Op { void run(int i); }

    static void time(String row, int iters, Op op) {
        int warm = Integer.getInteger("nio20cr.warm", 3);
        for (int w = 0; w < warm; w++) for (int i = 0; i < iters; i++) op.run(i);
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) op.run(i);
        long ns = System.nanoTime() - t0;
        System.out.println(row + " " + (ns / iters) + " ns/op");
    }

    static void bench() {
        int iters = Integer.getInteger("nio20cr.iters", 200000);
        Class<?>[] cls = {String.class, Integer.class, Long.class, Object.class, Map.class, List.class, Thread.class,
                Plain.class};
        ConcurrentHashMap<Class<?>, Integer> chmClass = new ConcurrentHashMap<>();
        for (int i = 0; i < cls.length; i++) chmClass.put(cls[i], i);
        HashMap<Class<?>, Integer> hmClass = new HashMap<>(chmClass);
        Object[] objs = new Object[16];
        ConcurrentHashMap<Object, Integer> chmObj = new ConcurrentHashMap<>();
        for (int i = 0; i < objs.length; i++) { objs[i] = new Plain(i); chmObj.put(objs[i], i); }
        Locale[] ls = {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN, Locale.ROOT, Locale.CHINA,
                Locale.forLanguageTag("de-CH"), Locale.forLanguageTag("sr-Latn-RS")};
        ConcurrentHashMap<Locale, Integer> chmLoc = new ConcurrentHashMap<>();
        for (int i = 0; i < ls.length; i++) chmLoc.put(ls[i], i);
        Point[] pts = new Point[16];
        ConcurrentHashMap<Point, Integer> chmUser = new ConcurrentHashMap<>();
        for (int i = 0; i < pts.length; i++) { pts[i] = new Point(i, i); chmUser.put(pts[i], i); }
        String[] strs = new String[16];
        ConcurrentHashMap<String, Integer> chmStr = new ConcurrentHashMap<>();
        for (int i = 0; i < strs.length; i++) { strs[i] = "key-" + i; chmStr.put(strs[i], i); }
        java.io.StringWriter sw = new java.io.StringWriter();
        java.io.PrintWriter pwSw = new java.io.PrintWriter(sw);
        java.io.ByteArrayOutputStream baos = new java.io.ByteArrayOutputStream();
        java.io.PrintWriter pwBaos = new java.io.PrintWriter(baos);
        String[] all = {"bench-chmclass", "bench-chmobj", "bench-chmlocale", "bench-chmuser", "bench-chmstr",
                "bench-hmclass", "bench-pwsw", "bench-pwbaos", "bench-sw"};
        for (String r : all) {
            if (!(on(r) || on(r.substring(6)))) continue;
            switch (r) {
                case "bench-chmclass" -> time(r, iters, i -> sink = chmClass.get(cls[i & 7]));
                case "bench-chmobj" -> time(r, iters, i -> sink = chmObj.get(objs[i & 15]));
                case "bench-chmlocale" -> time(r, iters, i -> sink = chmLoc.get(ls[i & 7]));
                case "bench-chmuser" -> time(r, iters, i -> sink = chmUser.get(pts[i & 15]));
                case "bench-chmstr" -> time(r, iters, i -> sink = chmStr.get(strs[i & 15]));
                case "bench-hmclass" -> time(r, iters, i -> sink = hmClass.get(cls[i & 7]));
                case "bench-pwsw" -> time(r, iters, i -> {
                    pwSw.println("row ");
                    pwSw.println(i);
                    if ((i & 63) == 0) sw.getBuffer().setLength(0);
                });
                case "bench-pwbaos" -> time(r, iters, i -> {
                    pwBaos.println("row ");
                    if ((i & 15) == 0) { pwBaos.flush(); baos.reset(); }
                });
                case "bench-sw" -> time(r, iters, i -> {
                    sw.write("row ");
                    if ((i & 63) == 0) sw.getBuffer().setLength(0);
                });
                default -> { }
            }
        }
    }
}
