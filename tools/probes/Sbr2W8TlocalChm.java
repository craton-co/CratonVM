import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * SBR round 2, wave 8, lane TLOCAL: the reference-array Unsafe reads
 * (ConcurrentHashMap.tabAt, reached through Unsafe.getReferenceAcquire /
 * getReferenceVolatile) must answer exactly what the registered native answers:
 * hot get/put/remove/resize loops, concurrent writers with readers, null and
 * absent keys, treeified bins, AtomicReferenceArray / ConcurrentLinkedQueue /
 * CopyOnWriteArrayList / ForkJoin-style array reads. Every line is
 * deterministic; diff the output against HotSpot in --jdk-only and --compatible.
 *
 *   java Sbr2W8TlocalChm
 */
public class Sbr2W8TlocalChm {
    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    static final class BadHash {
        final int v;
        BadHash(int v) { this.v = v; }
        @Override public int hashCode() { return v % 4; }
        @Override public boolean equals(Object o) { return o instanceof BadHash b && b.v == v; }
    }

    static final class Cmp implements Comparable<Cmp> {
        final int v;
        Cmp(int v) { this.v = v; }
        @Override public int hashCode() { return v % 3; }
        @Override public boolean equals(Object o) { return o instanceof Cmp b && b.v == v; }
        @Override public int compareTo(Cmp o) { return Integer.compare(v, o.v); }
    }

    public static void main(String[] args) throws Exception {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 5000; i++) {
            m.put("k" + i, i);
        }
        long s = 0;
        for (int r = 0; r < 40; r++) {
            for (int i = 0; i < 5000; i++) {
                Integer v = m.get("k" + i);
                s += v == null ? -1 : v;
            }
        }
        p("chm.getsum", s);
        p("chm.absent", m.get("nope"));
        p("chm.size", m.size());
        for (int i = 0; i < 5000; i += 3) {
            m.remove("k" + i);
        }
        s = 0;
        for (int i = 0; i < 5000; i++) {
            Integer v = m.get("k" + i);
            s += v == null ? -1 : v;
        }
        p("chm.afterRemove", s);
        try {
            m.get(null);
        } catch (NullPointerException e) {
            p("chm.getNull", "NPE");
        }
        p("chm.containsKey", m.containsKey("k1") + "/" + m.containsKey("k0"));
        p("chm.getOrDefault", m.getOrDefault("k0", -7));
        p("chm.computeIfAbsent", m.computeIfAbsent("k0", k -> 99));
        p("chm.merge", m.merge("k1", 1000, Integer::sum));
        p("chm.putIfAbsent", m.putIfAbsent("k1", 5));
        // treeified bins: colliding hashes
        ConcurrentHashMap<BadHash, Integer> bad = new ConcurrentHashMap<>();
        for (int i = 0; i < 200; i++) {
            bad.put(new BadHash(i), i);
        }
        long bs = 0;
        for (int r = 0; r < 20; r++) {
            for (int i = 0; i < 200; i++) {
                bs += bad.get(new BadHash(i));
            }
        }
        p("chm.collide", bs);
        ConcurrentHashMap<Cmp, Integer> cmp = new ConcurrentHashMap<>();
        for (int i = 0; i < 300; i++) {
            cmp.put(new Cmp(i), i);
        }
        long cs = 0;
        for (int r = 0; r < 20; r++) {
            for (int i = 0; i < 300; i++) {
                cs += cmp.get(new Cmp(i));
            }
        }
        p("chm.treeified", cs);
        // concurrent writers and readers
        ConcurrentHashMap<Integer, Integer> c = new ConcurrentHashMap<>();
        int T = 4;
        Thread[] ts = new Thread[T];
        AtomicLong readsSeen = new AtomicLong();
        for (int t = 0; t < T; t++) {
            final int base = t * 10000;
            ts[t] = new Thread(() -> {
                for (int i = 0; i < 10000; i++) {
                    c.put(base + i, i);
                    Integer v = c.get(base + i);
                    if (v != null && v == i) {
                        readsSeen.incrementAndGet();
                    }
                }
            });
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        p("chm.concurrent.size", c.size());
        p("chm.concurrent.reads", readsSeen.get());
        long cc = 0;
        for (int i = 0; i < T * 10000; i++) {
            cc += c.get(i);
        }
        p("chm.concurrent.sum", cc);
        // other Unsafe reference-array readers
        AtomicReferenceArray<String> ara = new AtomicReferenceArray<>(64);
        for (int i = 0; i < 64; i++) {
            ara.set(i, "s" + i);
        }
        long as = 0;
        for (int r = 0; r < 2000; r++) {
            for (int i = 0; i < 64; i++) {
                as += ara.get(i).length();
            }
        }
        p("ara.sum", as);
        p("ara.cas", ara.compareAndSet(3, "s3", "t3") + ara.get(3));
        ConcurrentLinkedQueue<Integer> q = new ConcurrentLinkedQueue<>();
        for (int i = 0; i < 1000; i++) {
            q.add(i);
        }
        long qs = 0;
        Integer x;
        while ((x = q.poll()) != null) {
            qs += x;
        }
        p("clq.sum", qs);
        CopyOnWriteArrayList<Integer> cow = new CopyOnWriteArrayList<>();
        for (int i = 0; i < 500; i++) {
            cow.add(i);
        }
        long ws = 0;
        for (int r = 0; r < 50; r++) {
            for (int i = 0; i < 500; i++) {
                ws += cow.get(i);
            }
        }
        p("cow.sum", ws);
        ConcurrentSkipListMap<Integer, Integer> sl = new ConcurrentSkipListMap<>();
        for (int i = 0; i < 2000; i++) {
            sl.put(i * 7 % 2003, i);
        }
        long ss = 0;
        for (int i = 0; i < 2003; i++) {
            Integer v = sl.get(i);
            ss += v == null ? -1 : v;
        }
        p("skiplist.sum", ss);
        ExecutorService ex = Executors.newFixedThreadPool(4);
        List<Future<Long>> fs = new ArrayList<>();
        for (int t = 0; t < 8; t++) {
            fs.add(ex.submit(() -> {
                long z = 0;
                for (int i = 0; i < 5000; i++) {
                    z += m.getOrDefault("k" + (i % 5000), 0);
                }
                return z;
            }));
        }
        long total = 0;
        for (Future<Long> f : fs) {
            total += f.get();
        }
        ex.shutdown();
        p("pool.sum", total);
        p("done", true);
    }
}
