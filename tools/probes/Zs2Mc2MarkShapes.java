// zs s2/mc2: the stop-the-world mark's per-object costs on the shapes the direct scan
// (CRATONVM_ZGC_MARK_DIRECT_SCAN) and the run-only prefetch (CRATONVM_ZGC_MARK_PREFETCH_RUNS)
// change, plus the referent hole on the direct path.
//
//   map    a HashMap<Long, String> (Node -> Long, Node -> String -> byte[]): four classes
//          interleaved on every DFS path -- the one-entry layout memo's WORST case (refills)
//   chain  a long singly linked list of one compact class: every object a memo hit, one
//          child per object (no prefetch run ever opens)
//   hubs   Object[64] hubs each holding 64 small objects: a run of 64 per hub scan (the
//          run-only prefetch's target) and memo hits inside it
//   refs   WeakReferences: half the referents also held strongly, half only weakly. After
//          System.gc() exactly the weakly-held half is cleared (the referent hole must hold
//          on every mark path)
//
// STDOUT is deterministic (identical on every JVM):
//   zs2mc2 shape=<s> live_mb=<n> rounds=<r> objects=<k> check=<c>
//   zs2mc2 shape=refs weak=<w> cleared=<c> strong_kept=<true|false>
// STDERR carries timing, never compared:
//   zs2mc2-cpu shape=<s> cpu_ms=<t> cpu_ms_per_round=<x> java_cpu_ms=<j> gcs=<g>
//
// Usage: Zs2Mc2MarkShapes <map|chain|hubs|refs|all> [liveMb=64] [rounds=20]
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;

public class Zs2Mc2MarkShapes {
    static final class Link {
        Link next;
        long v;
        Link(long v, Link next) { this.v = v; this.next = next; }
    }

    static final class Small {
        long v;
        Object pad;
        Small(long v) { this.v = v; }
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) n += c;
        }
        return n;
    }

    static long cpuNanos() {
        java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        if (os instanceof com.sun.management.OperatingSystemMXBean) {
            return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
        }
        return -1;
    }

    static long javaCpuNanos() {
        ThreadMXBean t = ManagementFactory.getThreadMXBean();
        long sum = 0;
        for (long id : t.getAllThreadIds()) {
            long c = t.getThreadCpuTime(id);
            if (c > 0) sum += c;
        }
        return sum;
    }

    static String val(long i) {
        return "v" + (i * 7919 % 100003);
    }

    static void run(String shape, int liveMb, int rounds) {
        long objects;
        HashMap<Long, String> map = null;
        Link[] heads = null;
        List<Object[]> hubs = null;
        if (shape.equals("map")) {
            // ~120 B per entry (Node 32 + Long 16 + String 24 + byte[] ~24, plus the table).
            int n = liveMb * 8000;
            map = new HashMap<>(n * 2);
            for (long i = 0; i < n; i++) map.put(i, val(i));
            objects = n;
        } else if (shape.equals("chain")) {
            // 32 chains of ~32 B links.
            int n = liveMb * 32768;
            heads = new Link[32];
            for (int c = 0; c < 32; c++) {
                Link h = null;
                for (int i = 0; i < n / 32; i++) h = new Link((long) c * n + i, h);
                heads[c] = h;
            }
            objects = (long) (n / 32) * 32;
        } else {
            // hubs: Object[64] of Small (~24 B + 272 B per hub).
            int n = liveMb * 640;
            hubs = new ArrayList<>(n);
            long k = 0;
            for (int h = 0; h < n; h++) {
                Object[] hub = new Object[64];
                for (int j = 0; j < 64; j++) hub[j] = new Small(k++);
                hubs.add(hub);
            }
            objects = k;
        }
        System.gc();
        long g0 = gcCount();
        long c0 = cpuNanos();
        long j0 = javaCpuNanos();
        for (int r = 0; r < rounds; r++) {
            if (map != null) {
                long n = map.size();
                for (long i = r & 3; i < n; i += 4) map.put(i, val(i));
            } else if (heads != null) {
                // Rebuild one chain in four.
                for (int c = r & 3; c < 32; c += 4) {
                    Link old = heads[c];
                    Link fresh = null;
                    Link rev = null;
                    for (Link x = old; x != null; x = x.next) rev = new Link(x.v, rev);
                    for (Link x = rev; x != null; x = x.next) fresh = new Link(x.v, fresh);
                    heads[c] = fresh;
                }
            } else {
                for (int h = r & 3; h < hubs.size(); h += 4) {
                    Object[] hub = hubs.get(h);
                    for (int j = 0; j < 64; j++) hub[j] = new Small(((Small) hub[j]).v);
                }
            }
            System.gc();
        }
        long c1 = cpuNanos();
        long j1 = javaCpuNanos();
        long g1 = gcCount();

        long check = 0;
        if (map != null) {
            for (long i = 0; i < map.size(); i++) check += i + 3L * map.get(i).length();
        } else if (heads != null) {
            for (Link h : heads) {
                for (Link x = h; x != null; x = x.next) check += x.v;
            }
        } else {
            for (Object[] hub : hubs) {
                for (Object o : hub) check += ((Small) o).v;
            }
        }
        System.out.printf(java.util.Locale.ROOT,
                "zs2mc2 shape=%s live_mb=%d rounds=%d objects=%d check=%d%n",
                shape, liveMb, rounds, objects, check);
        double cpuMs = (c1 - c0) / 1e6;
        System.err.printf(java.util.Locale.ROOT,
                "zs2mc2-cpu shape=%s cpu_ms=%.0f cpu_ms_per_round=%.1f java_cpu_ms=%.0f gcs=%d%n",
                shape, cpuMs, cpuMs / rounds, (j1 - j0) / 1e6, g1 - g0);
    }

    static void refs() {
        int n = 20000;
        List<WeakReference<Small>> weak = new ArrayList<>(n);
        List<Small> strong = new ArrayList<>(n / 2);
        for (int i = 0; i < n; i++) {
            Small s = new Small(i);
            weak.add(new WeakReference<>(s));
            if ((i & 1) == 0) strong.add(s);
        }
        System.gc();
        int cleared = 0;
        boolean strongKept = true;
        for (int i = 0; i < n; i++) {
            Small s = weak.get(i).get();
            if (s == null) {
                cleared++;
                if ((i & 1) == 0) strongKept = false;
            } else if (s.v != i) {
                strongKept = false;
            }
        }
        System.out.printf(java.util.Locale.ROOT,
                "zs2mc2 shape=refs weak=%d cleared=%d strong_kept=%b%n", n, cleared, strongKept);
        // Keep `strong` reachable to here.
        if (strong.size() != n / 2) throw new AssertionError();
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        int liveMb = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        for (String s : new String[] {"map", "chain", "hubs"}) {
            if (which.equals("all") || which.equals(s)) {
                run(s, liveMb, rounds);
            }
        }
        if (which.equals("all") || which.equals("refs")) {
            refs();
        }
    }
}
