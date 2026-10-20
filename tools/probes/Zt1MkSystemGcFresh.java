// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t1/mk (CRATONVM_ZGC_FULL_REQUEST_FRESH_MARK): System.gc() while a ZGC
// concurrent mark cycle is open. HotSpot ZGC answers System.gc() with a cycle
// that STARTS after the call, so an object that is only weakly reachable when
// System.gc() is called has its WeakReference cleared by the time it returns.
// CratonVM closed the open cycle with its snapshot-at-the-beginning mark set
// instead: an object allocated during the phase is allocated black, so it and
// its referent survived System.gc() (WeakReference.get() != null afterwards).
//
// Each round allocates `per` referents that are only weakly reachable (made in
// a separate method, so no local slot holds one), churns `churn` bytes of
// garbage so a concurrent cycle is open (CRATONVM_ZGC_CONC_START), calls
// System.gc() and counts the WeakReferences that still answer.
//
// Every line on stdout is deterministic:
//   Zt1MkSystemGcFresh rounds=<r> per=<p> alive_after_gc=0
// Args: [rounds] [per] [churn-bytes] (default 40, 2000, 6 MiB).
// Engagement (stderr, --verbose:gc): `[GC] zgc-conc-abandon:` lines; zero of
// them means no cycle was open at a System.gc() and the row proves nothing.
//
// zt t3/mk (CRATONVM_ZGC_FULL_REQUEST_CLOSES_OPEN_CYCLE, x1 finding X1-9): on
// STDERR (timing-dependent, so not compared) the probe also prints how far
// the "Cycles" collectors' collection counts moved per System.gc():
//   Zt1MkSystemGcFresh deltas 1=<n> 2=<n> other=<n>
// HotSpot moves them by 2 when a cycle was running at the call (it completes
// that cycle, then runs one for the request) and by 1 otherwise; CratonVM
// with the switch moves them the same way, under `=0` only by 1.
//
// zt t6/rv3 (`zgc-zt4x3-*` item 6): also on STDERR, the cause of the "Cycles"
// collection that came right before each `System.gc()` one, from
// GarbageCollectionNotificationInfo.getGcCause():
//   Zt1MkSystemGcFresh closing-causes <cause>=<n> ...
// HotSpot completes a running cycle under the cause that STARTED it (Warmup,
// Allocation Rate, Proactive, ...); compare this line with the HotSpot twin
// under CRATONVM_ZGC_CONC_START=20.
import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

public class Zt1MkSystemGcFresh {
    static volatile Object sink;

    @SuppressWarnings("unchecked")
    static WeakReference<int[]>[] make(int per) {
        WeakReference<int[]>[] refs = new WeakReference[per];
        for (int i = 0; i < per; i++) {
            refs[i] = new WeakReference<>(new int[16]);
        }
        return refs;
    }

    static void churn(int bytes) {
        for (int done = 0; done < bytes; done += 512) {
            sink = new byte[512];
        }
        sink = null;
    }

    /** Causes of the "Cycles" collections, in notification order. */
    static final List<String> CAUSES = new ArrayList<>();

    static void listen() {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (!(b instanceof NotificationEmitter) || !b.getName().contains("Cycles")) {
                continue;
            }
            ((NotificationEmitter) b).addNotificationListener((n, h) -> {
                if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION
                        .equals(n.getType())) {
                    return;
                }
                GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo
                        .from((CompositeData) n.getUserData());
                synchronized (CAUSES) {
                    CAUSES.add(info.getGcCause());
                }
            }, null, null);
        }
    }

    static String closingCauses() {
        Map<String, Integer> seen = new TreeMap<>();
        synchronized (CAUSES) {
            for (int i = 1; i < CAUSES.size(); i++) {
                if (CAUSES.get(i).equals("System.gc()")
                        && !CAUSES.get(i - 1).equals("System.gc()")) {
                    seen.merge(CAUSES.get(i - 1), 1, Integer::sum);
                }
            }
        }
        StringBuilder sb = new StringBuilder("Zt1MkSystemGcFresh closing-causes");
        for (Map.Entry<String, Integer> e : seen.entrySet()) {
            sb.append(' ').append(e.getKey().replace(' ', '_')).append('=').append(e.getValue());
        }
        return sb.toString();
    }

    static long cycles() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().contains("Cycles") && b.getCollectionCount() > 0) {
                n += b.getCollectionCount();
            }
        }
        return n;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        int churnBytes = args.length > 2 ? Integer.parseInt(args[2]) : 6 << 20;
        long alive = 0;
        int one = 0;
        int two = 0;
        int other = 0;
        listen();
        for (int r = 0; r < rounds; r++) {
            churn(churnBytes);
            WeakReference<int[]>[] refs = make(per);
            churn(churnBytes / 8);
            long before = cycles();
            System.gc();
            long delta = cycles() - before;
            if (delta == 1) {
                one++;
            } else if (delta == 2) {
                two++;
            } else {
                other++;
            }
            for (WeakReference<int[]> w : refs) {
                if (w.get() != null) {
                    alive++;
                }
            }
        }
        System.out.println("Zt1MkSystemGcFresh rounds=" + rounds + " per=" + per
                + " alive_after_gc=" + alive);
        System.err.println("Zt1MkSystemGcFresh deltas 1=" + one + " 2=" + two
                + " other=" + other);
        try {
            Thread.sleep(200); // notifications are delivered asynchronously
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        System.err.println(closingCauses());
    }
}
