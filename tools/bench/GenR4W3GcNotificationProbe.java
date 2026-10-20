// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import javax.management.MBeanNotificationInfo;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationFilter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * Generational GC round 4, wave 3, lane obs2 (2026-09-23): GC notifications.
 *
 * <p>Registers a {@link NotificationListener} on every {@link GarbageCollectorMXBean}
 * (what Micrometer's {@code JvmGcMetrics} does for {@code jvm.gc.pause}), churns
 * short-lived garbage until the young collector has run a few times, calls
 * {@code System.gc()} three times, waits for the notifications and prints ONLY
 * fields that are deterministic across runs and across the two VMs.
 *
 * <p>Run:
 * <pre>
 *   java     -XX:+UseSerialGC      -Xmx256m -cp tools/bench GenR4W3GcNotificationProbe   # reference
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W3GcNotificationProbe
 * </pre>
 *
 * <p>Expected, identical on both (HotSpot 25 SerialGC, and CratonVM's generational
 * backend after round 4 wave 3):
 * <pre>
 *   bean name=Copy emitter=true com_sun_bean=true notif_types=com.sun.management.gc.notification
 *   bean name=MarkSweepCompact emitter=true com_sun_bean=true notif_types=com.sun.management.gc.notification
 *   same_beans_on_every_call=true
 *   young_collections_moved=true
 *   minor_notifications_seen=true
 *   minor gcName=Copy gcAction=end of minor GC gcCause=Allocation Failure
 *   major_system_gc_notifications=3
 *   major gcName=MarkSweepCompact gcAction=end of major GC gcCause=System.gc()
 *   type_is_gc_notification=true
 *   source_is_the_beans_object_name=true
 *   message_is_gc_name=true
 *   user_data_round_trips=true
 *   ids_strictly_increase_per_bean=true
 *   sequence_numbers_strictly_increase=true
 *   times_consistent=true
 *   gcinfo_pools_are_all_pools=true
 *   gcinfo_heap_pools=Eden Space,Survivor Space,Tenured Gen
 *   usages_sane=true
 *   gc_thread_count=1
 *   last_major_id_is_its_count=true
 *   last_gc_info id_is_count=true bean=Copy
 *   last_gc_info id_is_count=true bean=MarkSweepCompact
 *   DONE
 * </pre>
 *
 * <p>Before wave 3 CratonVM printed {@code emitter=true com_sun_bean=false
 * notif_types=} (plain {@code sun.management.GarbageCollectorImpl} beans, fresh on
 * every call so {@code same_beans_on_every_call=false}), and no notification ever
 * arrived: {@code minor_notifications_seen=false}, {@code major_system_gc_notifications=0}.
 * Under G1 and ZGC CratonVM still sends none (no per-bean model; see
 * {@code docs/internal/gc-common-round-20260923/gengc-r4w2-obs-g1-and-zgc-jmx-beans-are-still-the-legacy-pair-20260923-FIXED-20260923.md}).
 *
 * <p>On CratonVM, notifications are delivered by the thread that collected, after
 * the collection (HotSpot: its Notification Thread). An allocation-triggered
 * collection inside compiled code defers delivery to the next drain point; the
 * {@code System.gc()} calls are such points, which is why the minor notifications
 * are checked only after them. See
 * {@code docs/internal/gc/gengc-r4w3-obs2-gc-notifications-are-delivered-by-the-collecting-thread-RETIRED-20260924.md}.
 */
public final class GenR4W3GcNotificationProbe {
    static volatile Object sink;

    /** One received notification, reduced to what the checks need. */
    static final class Seen {
        String type;
        Object source;
        String message;
        long seq;
        String gcName;
        String gcAction;
        String gcCause;
        long id;
        long start;
        long end;
        long duration;
        Set<String> beforeKeys;
        Set<String> afterKeys;
        Set<String> heapKeys;
        boolean usagesSane;
        Object gcThreadCount;
        boolean roundTrips;
    }

    static final List<Seen> SEEN = Collections.synchronizedList(new ArrayList<Seen>());
    static final Set<String> HEAP_POOLS = new TreeSet<>();

    public static void main(String[] args) throws Exception {
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        Set<String> allPools = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            allPools.add(p.getName());
            if (p.getType() == MemoryType.HEAP) {
                HEAP_POOLS.add(p.getName());
            }
        }
        List<GarbageCollectorMXBean> again = ManagementFactory.getGarbageCollectorMXBeans();
        boolean same = gcs.size() == again.size();
        for (int i = 0; same && i < gcs.size(); i++) {
            same = gcs.get(i) == again.get(i);
        }

        NotificationListener listener = new NotificationListener() {
            @Override
            public void handleNotification(Notification n, Object handback) {
                SEEN.add(reduce(n));
            }
        };
        NotificationFilter filter = new NotificationFilter() {
            @Override
            public boolean isNotificationEnabled(Notification n) {
                return GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType());
            }
        };
        Map<String, GarbageCollectorMXBean> byName = new HashMap<>();
        String young = null;
        String old = null;
        for (GarbageCollectorMXBean gc : gcs) {
            byName.put(gc.getName(), gc);
            if (young == null) {
                young = gc.getName();
            } else if (old == null) {
                old = gc.getName();
            }
            StringBuilder types = new StringBuilder();
            boolean emitter = gc instanceof NotificationEmitter;
            if (emitter) {
                NotificationEmitter e = (NotificationEmitter) gc;
                for (MBeanNotificationInfo info : e.getNotificationInfo()) {
                    for (String t : info.getNotifTypes()) {
                        types.append(types.length() == 0 ? "" : ",").append(t);
                    }
                }
                try {
                    e.addNotificationListener(listener, filter, gc.getName());
                } catch (RuntimeException ex) {
                    // Never on HotSpot. Printed so a VM whose bean cannot take a
                    // listener says so instead of dying before the report.
                    System.out.println("listener_registration_failed bean=" + gc.getName()
                            + " exception=" + ex.getClass().getName());
                }
            }
            System.out.println("bean name=" + gc.getName() + " emitter=" + emitter
                    + " com_sun_bean=" + (gc instanceof com.sun.management.GarbageCollectorMXBean)
                    + " notif_types=" + types);
        }
        System.out.println("same_beans_on_every_call=" + same);
        if (young == null || old == null) {
            System.out.println("expected two collector beans, got " + gcs.size());
            return;
        }

        // Young collections: short-lived garbage only, so nothing is promoted
        // and the young collector is the one that runs.
        GarbageCollectorMXBean youngBean = byName.get(young);
        long youngBefore = youngBean.getCollectionCount();
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (youngBean.getCollectionCount() < youngBefore + 3 && System.nanoTime() < deadline) {
            for (int i = 0; i < 10_000; i++) {
                sink = new byte[512];
            }
        }
        System.out.println("young_collections_moved=" + (youngBean.getCollectionCount() >= youngBefore + 3));

        // Full collections.
        for (int i = 0; i < 3; i++) {
            System.gc();
        }

        // Notifications are asynchronous on HotSpot: wait for them.
        waitFor(young, old, 10_000);
        List<Seen> seen;
        synchronized (SEEN) {
            seen = new ArrayList<>(SEEN);
        }

        List<Seen> minors = new ArrayList<>();
        List<Seen> majorsSystemGc = new ArrayList<>();
        for (Seen s : seen) {
            if (young.equals(s.gcName)) {
                minors.add(s);
            } else if (old.equals(s.gcName) && "System.gc()".equals(s.gcCause)) {
                majorsSystemGc.add(s);
            }
        }
        System.out.println("minor_notifications_seen=" + !minors.isEmpty());
        if (!minors.isEmpty()) {
            Seen m = minors.get(0);
            System.out.println("minor gcName=" + m.gcName + " gcAction=" + m.gcAction + " gcCause=" + m.gcCause);
        }
        System.out.println("major_system_gc_notifications=" + majorsSystemGc.size());
        if (!majorsSystemGc.isEmpty()) {
            Seen m = majorsSystemGc.get(0);
            System.out.println("major gcName=" + m.gcName + " gcAction=" + m.gcAction + " gcCause=" + m.gcCause);
        }

        boolean typeOk = !seen.isEmpty();
        boolean sourceOk = !seen.isEmpty();
        boolean messageOk = !seen.isEmpty();
        boolean roundTrips = !seen.isEmpty();
        boolean idsIncrease = !seen.isEmpty();
        boolean seqIncrease = !seen.isEmpty();
        boolean timesOk = !seen.isEmpty();
        boolean poolsOk = !seen.isEmpty();
        boolean usagesOk = !seen.isEmpty();
        Set<String> heapKeys = new TreeSet<>();
        Object threadCount = null;
        Map<String, Long> lastId = new HashMap<>();
        long lastSeq = Long.MIN_VALUE;
        for (Seen s : seen) {
            typeOk &= GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(s.type);
            GarbageCollectorMXBean bean = byName.get(s.gcName);
            sourceOk &= bean != null && bean.getObjectName().equals(s.source);
            messageOk &= s.gcName != null && s.gcName.equals(s.message);
            roundTrips &= s.roundTrips;
            Long prev = lastId.get(s.gcName);
            idsIncrease &= prev == null || s.id > prev;
            lastId.put(s.gcName, s.id);
            seqIncrease &= s.seq > lastSeq;
            lastSeq = s.seq;
            timesOk &= s.start >= 0 && s.end >= s.start && s.duration == s.end - s.start;
            poolsOk &= s.beforeKeys.equals(allPools) && s.afterKeys.equals(allPools);
            usagesOk &= s.usagesSane;
            heapKeys.addAll(s.heapKeys);
            if (threadCount == null) {
                threadCount = s.gcThreadCount;
            }
        }
        System.out.println("type_is_gc_notification=" + typeOk);
        System.out.println("source_is_the_beans_object_name=" + sourceOk);
        System.out.println("message_is_gc_name=" + messageOk);
        System.out.println("user_data_round_trips=" + roundTrips);
        System.out.println("ids_strictly_increase_per_bean=" + idsIncrease);
        System.out.println("sequence_numbers_strictly_increase=" + seqIncrease);
        System.out.println("times_consistent=" + timesOk);
        System.out.println("gcinfo_pools_are_all_pools=" + poolsOk);
        System.out.println("gcinfo_heap_pools=" + String.join(",", heapKeys));
        System.out.println("usages_sane=" + usagesOk);
        System.out.println("gc_thread_count=" + threadCount);
        Long lastMajor = lastId.get(old);
        System.out.println("last_major_id_is_its_count="
                + (lastMajor != null && lastMajor == byName.get(old).getCollectionCount()));

        // `getLastGcInfo()`: the same data, pulled instead of pushed.
        for (GarbageCollectorMXBean gc : gcs) {
            if (gc instanceof com.sun.management.GarbageCollectorMXBean) {
                GcInfo last = ((com.sun.management.GarbageCollectorMXBean) gc).getLastGcInfo();
                boolean ok = last != null && last.getId() == gc.getCollectionCount();
                System.out.println("last_gc_info id_is_count=" + ok + " bean=" + gc.getName());
            }
        }
        System.out.println("DONE");
    }

    /** Wait until the three System.gc() notifications (and one minor) have arrived. */
    static void waitFor(String young, String old, long millis) throws InterruptedException {
        long deadline = System.currentTimeMillis() + millis;
        while (System.currentTimeMillis() < deadline) {
            int minors = 0;
            int majors = 0;
            synchronized (SEEN) {
                for (Seen s : SEEN) {
                    if (young.equals(s.gcName)) {
                        minors++;
                    } else if (old.equals(s.gcName) && "System.gc()".equals(s.gcCause)) {
                        majors++;
                    }
                }
            }
            if (minors >= 1 && majors >= 3) {
                return;
            }
            Thread.sleep(10);
        }
    }

    static Seen reduce(Notification n) {
        Seen s = new Seen();
        s.type = n.getType();
        s.source = n.getSource();
        s.message = n.getMessage();
        s.seq = n.getSequenceNumber();
        CompositeData cd = (CompositeData) n.getUserData();
        GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo.from(cd);
        s.gcName = info.getGcName();
        s.gcAction = info.getGcAction();
        s.gcCause = info.getGcCause();
        GcInfo gi = info.getGcInfo();
        s.id = gi.getId();
        s.start = gi.getStartTime();
        s.end = gi.getEndTime();
        s.duration = gi.getDuration();
        s.beforeKeys = new TreeSet<>(gi.getMemoryUsageBeforeGc().keySet());
        s.afterKeys = new TreeSet<>(gi.getMemoryUsageAfterGc().keySet());
        s.heapKeys = new TreeSet<>(s.beforeKeys);
        s.heapKeys.retainAll(HEAP_POOLS);
        boolean sane = true;
        for (Map<String, MemoryUsage> m : List.of(gi.getMemoryUsageBeforeGc(), gi.getMemoryUsageAfterGc())) {
            for (MemoryUsage u : m.values()) {
                sane &= u != null && u.getUsed() >= 0 && u.getCommitted() >= u.getUsed()
                        && (u.getMax() < 0 || u.getMax() >= u.getCommitted());
            }
        }
        s.usagesSane = sane;
        s.gcThreadCount = gi.containsKey("GcThreadCount") ? gi.get("GcThreadCount") : "absent";
        // The CompositeData view must say what the typed view says.
        GarbageCollectionNotificationInfo again = GarbageCollectionNotificationInfo.from(info.toCompositeData(null));
        s.roundTrips = again.getGcName().equals(s.gcName)
                && again.getGcAction().equals(s.gcAction)
                && again.getGcCause().equals(s.gcCause)
                && again.getGcInfo().getId() == s.id;
        return s;
    }
}
