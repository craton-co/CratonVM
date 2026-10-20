// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i41/rs ({@code gci-i39rv-waves-i35-i38-review-residuals-20261016} item 1):
 * the G1 {@code System.gc()} door's Survivor re-stamp against another thread's
 * young pause. Before i41 the door re-stamped {@code G1 Survivor Space}'s
 * collection usage to 0 AFTER its full-compaction pause released the world, so
 * a young pause another thread ran in between had its Survivor figure
 * overwritten by the door's late zero. HotSpot's answer is the young pause's.
 *
 * <pre>
 *   java     -XX:+UseG1GC -Xmx256m -cp &lt;classes&gt; GciI41RsG1SurvivorStampRaceProbe [iterations]
 *   cratonvm -XX:+UseG1GC -Xmx256m -cp &lt;classes&gt; GciI41RsG1SurvivorStampRaceProbe [iterations]
 * </pre>
 *
 * Thread {@code alloc} allocates without pause, keeping a ring of recent arrays
 * alive so every young pause has survivors. {@code main} calls
 * {@code System.gc()} {@code iterations} times (default 200) and, right after
 * each returns, reads the young collector's last {@code GcInfo}, the old
 * collector's, and the Survivor pool's collection usage, then the two
 * {@code GcInfo} ids again. An iteration is JUDGED when the ids did not move
 * across the read, the last young collection's cause (from its notification) is
 * not {@code System.gc()} (an allocation pause of {@code alloc}, not a pause of
 * the door), it started after the old collector's last collection ended, and
 * it left the Survivor space non-empty. A judged iteration whose Survivor
 * collection usage reads 0 is a VIOLATION: the last collection to affect the
 * pool was that young pause, and its figure is not 0.
 *
 * stdout (deterministic): {@code g1 survivor stamp race: violations 0} when
 * there was none ({@code violations >0} otherwise), then {@code done}; off G1
 * {@code not g1} and {@code done}. stderr: {@code [i41rs] iterations=.. judged=..
 * violations=.. unstable=.. door-young=.. no-cause=..} and one
 * {@code [i41rs] violation ..} line per violation (the first 16).
 */
public class GciI41RsG1SurvivorStampRaceProbe {
    static volatile boolean stop;
    static volatile Object sink;
    static final Map<Long, String> causeById = new ConcurrentHashMap<>();

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        com.sun.management.GarbageCollectorMXBean young = null;
        com.sun.management.GarbageCollectorMXBean old = null;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc.getName().equals("G1 Young Generation")) {
                young = (com.sun.management.GarbageCollectorMXBean) gc;
            } else if (gc.getName().equals("G1 Old Generation")) {
                old = (com.sun.management.GarbageCollectorMXBean) gc;
            }
        }
        MemoryPoolMXBean survivor = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals("G1 Survivor Space")) {
                survivor = p;
            }
        }
        if (young == null || old == null || survivor == null) {
            System.out.println("not g1");
            System.out.println("done");
            return;
        }
        NotificationListener listener = new NotificationListener() {
            @Override
            public void handleNotification(Notification n, Object handback) {
                if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    return;
                }
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                if (info.getGcName().equals("G1 Young Generation")) {
                    causeById.put(info.getGcInfo().getId(), info.getGcCause());
                }
            }
        };
        ((NotificationEmitter) young).addNotificationListener(listener, null, null);

        Thread alloc = new Thread(GciI41RsG1SurvivorStampRaceProbe::allocate, "alloc");
        alloc.setDaemon(true);
        alloc.start();
        Thread.sleep(200);

        int judged = 0;
        int violations = 0;
        int unstable = 0;
        int doorYoung = 0;
        int noCause = 0;
        int printed = 0;
        for (int it = 0; it < iterations; it++) {
            System.gc();
            GcInfo y = young.getLastGcInfo();
            GcInfo o = old.getLastGcInfo();
            MemoryUsage cu = survivor.getCollectionUsage();
            GcInfo y2 = young.getLastGcInfo();
            GcInfo o2 = old.getLastGcInfo();
            if (y == null || y2 == null || y.getId() != y2.getId() || id(o) != id(o2)) {
                unstable++;
                continue;
            }
            if (o != null && y.getStartTime() <= o.getEndTime()) {
                continue;
            }
            MemoryUsage after = y.getMemoryUsageAfterGc().get("G1 Survivor Space");
            if (after == null || after.getUsed() == 0) {
                continue;
            }
            String cause = awaitCause(y.getId());
            if (cause == null) {
                noCause++;
                continue;
            }
            if (cause.equals("System.gc()")) {
                doorYoung++;
                continue;
            }
            judged++;
            long used = cu == null ? -1 : cu.getUsed();
            if (used == 0) {
                violations++;
                if (printed++ < 16) {
                    System.err.println("[i41rs] violation it=" + it + " young_id=" + y.getId()
                            + " cause=" + cause + " young_start=" + y.getStartTime()
                            + " old_end=" + (o == null ? -1 : o.getEndTime())
                            + " young_survivor_after=" + after.getUsed()
                            + " collection_usage_used=" + used);
                }
            }
        }
        stop = true;
        alloc.join(10_000);
        System.err.println("[i41rs] iterations=" + iterations + " judged=" + judged
                + " violations=" + violations + " unstable=" + unstable
                + " door-young=" + doorYoung + " no-cause=" + noCause);
        System.out.println("g1 survivor stamp race: violations " + (violations == 0 ? "0" : ">0"));
        System.out.println("done");
    }

    static long id(GcInfo g) {
        return g == null ? -1 : g.getId();
    }

    /** The cause of young collection {@code id}, waiting up to 2 s for its notification. */
    static String awaitCause(long id) throws InterruptedException {
        for (int i = 0; i < 200; i++) {
            String c = causeById.get(id);
            if (c != null) {
                return c;
            }
            Thread.sleep(10);
        }
        return null;
    }

    static void allocate() {
        Object[] ring = new Object[4096];
        long i = 0;
        while (!stop) {
            ring[(int) (i & 4095)] = new byte[256 + (int) (i & 255)];
            sink = new byte[1024];
            i++;
        }
        sink = ring;
    }
}
