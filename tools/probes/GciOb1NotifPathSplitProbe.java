// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.lang.reflect.Type;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i1/ob (2026-10-11): where the first GC notification's cold cost goes
 * ({@code docs/known-issues/gc/gcd-d10o-first-gc-notification-arrives-a-second-late-20261005.md}).
 *
 * <p>Runs the JDK half of a delivery step by step on the main thread, BEFORE
 * any listener exists, each step cold once and then warm, and only then
 * measures a real first delivery (which should now be warm):
 * <ol>
 * <li>{@code last}: {@code getLastGcInfo()} on every GC bean after one
 *     {@code System.gc()} (the bean's {@code GcInfoBuilder} and the
 *     {@code GcInfo} the VM builds; the VM's delivery takes the same two);</li>
 * <li>{@code gcinfo_cd}: the {@code GcInfo}'s composite data, read
 *     ({@code GcInfoCompositeData.getCompositeData}: the {@code MappedMXBeanType}
 *     of {@code Map<String,MemoryUsage>} via {@code getGenericReturnType}, the
 *     tabular usage maps, {@code GcInfoBuilder.getGcInfoCompositeType});</li>
 * <li>{@code notif_cd}: {@code new GarbageCollectionNotificationInfo(..)}'s
 *     composite data, read ({@code GarbageCollectionNotifInfoCompositeData}:
 *     {@code Class.forName}, three {@code Field.get} of {@code GcInfo}'s
 *     private fields -- method-handle field accessors under the JDK's own
 *     reflection -- and the notification's {@code CompositeType});</li>
 * <li>{@code field}: {@code Field.get} of this class's own private field
 *     (a fresh member: one more field accessor), and {@code generic}:
 *     {@code getGenericReturnType} of a fresh generic method;</li>
 * <li>{@code deliver}: a listener on every bean, {@code System.gc()}, the
 *     wait for the first notification (as {@code GchOb1NotifLatencyProbe}).</li>
 * </ol>
 * Arg 0 {@code micro-first}: run step 4 before steps 1-3, so it measures the
 * process's first field accessor and generic parse instead of the per-member
 * cost.
 *
 * <p>stdout is deterministic (what each step read, never a time). stderr:
 * {@code [probe] step=<name> begin} before each step (the marker a
 * {@code -Xlog:class+load} census counts between) and one
 * {@code [probe] split ...} line of micro-seconds at the end. No lambda and no
 * string concatenation in this class: either would spin {@code LambdaForm}
 * classes of its own inside a step.
 */
public final class GciOb1NotifPathSplitProbe {
    private Object fieldA = "a";
    private Object fieldB = "b";

    static Map<String, Integer> genericA() {
        return null;
    }

    static Map<String, Long> genericB() {
        return null;
    }

    private static void mark(String step) {
        StringBuilder sb = new StringBuilder();
        sb.append("[probe] step=").append(step).append(" begin");
        System.err.println(sb.toString());
    }

    private static long usSince(long t0) {
        return (System.nanoTime() - t0) / 1_000L;
    }

    private static final StringBuilder SPLIT = new StringBuilder("[probe] split");

    private static void note(String key, long us) {
        SPLIT.append(' ').append(key).append('=').append(us);
    }

    private static void out(String a, Object b) {
        StringBuilder sb = new StringBuilder();
        sb.append(a).append(b);
        System.out.println(sb.toString());
    }

    private static void micro() throws Exception {
        GciOb1NotifPathSplitProbe self = new GciOb1NotifPathSplitProbe();
        mark("field");
        long t = System.nanoTime();
        Field fa = GciOb1NotifPathSplitProbe.class.getDeclaredField("fieldA");
        Object va = fa.get(self);
        note("field_cold_us", usSince(t));
        t = System.nanoTime();
        Object va2 = GciOb1NotifPathSplitProbe.class.getDeclaredField("fieldA").get(self);
        note("field_again_us", usSince(t));
        t = System.nanoTime();
        Object vb = GciOb1NotifPathSplitProbe.class.getDeclaredField("fieldB").get(self);
        note("field_fresh_us", usSince(t));
        out("field=", va);
        out("field_again=", va2);
        out("field_fresh=", vb);

        mark("generic");
        t = System.nanoTime();
        Method ga = GciOb1NotifPathSplitProbe.class.getDeclaredMethod("genericA");
        Type ta = ga.getGenericReturnType();
        note("generic_cold_us", usSince(t));
        t = System.nanoTime();
        Type tb = GciOb1NotifPathSplitProbe.class.getDeclaredMethod("genericB").getGenericReturnType();
        note("generic_fresh_us", usSince(t));
        out("generic=", ta.getTypeName());
        out("generic_fresh=", tb.getTypeName());
    }

    public static void main(String[] args) throws Exception {
        boolean microFirst = args.length > 0 && "micro-first".equals(args[0]);
        if (microFirst) {
            micro();
        }
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();

        mark("gc0");
        System.gc();

        mark("last");
        long t = System.nanoTime();
        GcInfo info = null;
        String infoBean = null;
        int ext = 0;
        for (GarbageCollectorMXBean b : beans) {
            if (b instanceof com.sun.management.GarbageCollectorMXBean e) {
                ext++;
                GcInfo g = e.getLastGcInfo();
                if (g != null && info == null) {
                    info = g;
                    infoBean = b.getName();
                }
            }
        }
        note("last_us", usSince(t));
        out("ext_beans_ok=", Boolean.valueOf(ext == beans.size()));
        out("last=", info == null ? "absent" : "present");

        if (info != null) {
            mark("gcinfo_cd");
            t = System.nanoTime();
            CompositeData gcd = info.toCompositeData(null);
            Object id = gcd.get("id");
            note("gcinfo_cd_cold_us", usSince(t));
            out("gcinfo_id_matches=", Boolean.valueOf(id instanceof Long && ((Long) id).longValue() == info.getId()));
            out("gcinfo_has_usage=", Boolean.valueOf(gcd.containsKey("memoryUsageAfterGc")));

            mark("notif_cd");
            t = System.nanoTime();
            GarbageCollectionNotificationInfo n1 =
                    new GarbageCollectionNotificationInfo(infoBean, "end of major GC", "System.gc()", info);
            CompositeData ncd = n1.toCompositeData(null);
            Object cause = ncd.get("gcCause");
            note("notif_cd_cold_us", usSince(t));
            t = System.nanoTime();
            GarbageCollectionNotificationInfo n2 =
                    new GarbageCollectionNotificationInfo(infoBean, "end of major GC", "System.gc()", info);
            Object cause2 = n2.toCompositeData(null).get("gcCause");
            note("notif_cd_warm_us", usSince(t));
            out("notif_cause=", cause);
            out("notif_cause_warm=", cause2);
            out("notif_keys=", new TreeSet<String>(ncd.getCompositeType().keySet()));
        }

        if (!microFirst) {
            micro();
        }

        mark("listen");
        final AtomicLong arrived = new AtomicLong();
        final CountDownLatch[] latch = {new CountDownLatch(1)};
        NotificationListener listener = new NotificationListener() {
            @Override
            public void handleNotification(Notification n, Object hb) {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    arrived.compareAndSet(0, System.nanoTime());
                    synchronized (latch) {
                        latch[0].countDown();
                    }
                }
            }
        };
        for (GarbageCollectorMXBean b : beans) {
            ((NotificationEmitter) b).addNotificationListener(listener, null, null);
        }
        for (int round = 1; round <= 2; round++) {
            CountDownLatch l = new CountDownLatch(1);
            synchronized (latch) {
                latch[0] = l;
            }
            arrived.set(0);
            mark(round == 1 ? "deliver1" : "deliver2");
            long tCall = System.nanoTime();
            System.gc();
            long t0 = System.nanoTime();
            boolean ok = l.await(10, TimeUnit.SECONDS);
            long at = arrived.get();
            note(round == 1 ? "deliver1_call_us" : "deliver2_call_us", ok && at != 0 ? (at - tCall) / 1_000L : -1);
            note(round == 1 ? "deliver1_after_return_us" : "deliver2_after_return_us",
                    ok && at != 0 ? Math.max(0, (at - t0) / 1_000L) : -1);
            out(round == 1 ? "deliver1 notified=" : "deliver2 notified=", Boolean.valueOf(ok));
        }
        mark("end");
        System.err.println(SPLIT.toString());
        System.out.println("done");
    }
}
