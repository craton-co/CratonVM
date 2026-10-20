// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import com.sun.management.GcInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * zs s4/ob3 (2026-10-15): the relations between a ZGC cycle's pause records
 * and its cycle record that hold on HotSpot 25 AND on CratonVM once a
 * collection is booked as its phase pauses ({@code CRATONVM_ZGC_PAUSE_PHASES},
 * default ON). HotSpot books 8 pauses per {@code System.gc()} (local 25.0.3,
 * {@code -Xlog:gc,gc+phases}); CratonVM 3 (Pause Mark Start / Mark End /
 * Relocate Start of its one stop-the-world collection), so the exact count
 * is {@code Zr1TrPauseCountProbe}'s row, not this one.
 *
 * <p>stdout (deterministic on both VMs): every pause run before a cycle
 * notification is at least 3 long, the pause ids are consecutive, every
 * pause record lies inside its cycle record, the pause durations sum to no
 * more than the cycle's, pause GcInfo usage is all zero, the pause bean's
 * collection time is no more than the cycle bean's, and the causes. With
 * {@code CRATONVM_ZGC_PAUSE_PHASES=0} the first line reads {@code false}
 * (the kill-switch control: one pause per cycle).
 */
public final class Zs4Ob3PauseShape {
    static final String CYCLES = "ZGC Major Cycles";
    static final String PAUSES = "ZGC Major Pauses";

    record Note(String bean, String cause, long id, long start, long end, long duration, boolean zeroUsage) {}

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        List<Note> notes = new ArrayList<>();
        Map<String, GarbageCollectorMXBean> beans = new HashMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            beans.put(gc.getName(), gc);
            ((NotificationEmitter) gc).addNotificationListener((note, hb) -> {
                if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(note.getType())) {
                    return;
                }
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) note.getUserData());
                GcInfo gi = info.getGcInfo();
                boolean zero = true;
                for (Map<String, MemoryUsage> m : List.of(gi.getMemoryUsageBeforeGc(), gi.getMemoryUsageAfterGc())) {
                    for (MemoryUsage u : m.values()) {
                        zero &= u.getUsed() == 0 && u.getCommitted() == 0 && u.getInit() == 0 && u.getMax() == 0;
                    }
                }
                synchronized (notes) {
                    notes.add(new Note(info.getGcName(), info.getGcCause(), gi.getId(), gi.getStartTime(),
                            gi.getEndTime(), gi.getDuration(), zero));
                }
            }, null, null);
        }
        GarbageCollectorMXBean cycles = beans.get(CYCLES);
        GarbageCollectorMXBean pauses = beans.get(PAUSES);
        if (cycles == null || pauses == null) {
            System.out.println("no ZGC beans: " + new TreeSet<>(beans.keySet()));
            return;
        }
        long c0 = cycles.getCollectionCount();
        long p0 = pauses.getCollectionCount();
        long ct0 = cycles.getCollectionTime();
        long pt0 = pauses.getCollectionTime();
        for (int i = 0; i < n; i++) {
            System.gc();
        }
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (System.nanoTime() < deadline) {
            long booked = cycles.getCollectionCount() - c0 + pauses.getCollectionCount() - p0;
            synchronized (notes) {
                if (booked > 0 && notes.size() >= booked) {
                    break;
                }
            }
            Thread.sleep(20);
        }
        Thread.sleep(200);
        long dc = cycles.getCollectionCount() - c0;
        long dp = pauses.getCollectionCount() - p0;
        long dct = cycles.getCollectionTime() - ct0;
        long dpt = pauses.getCollectionTime() - pt0;
        List<Note> got;
        synchronized (notes) {
            got = new ArrayList<>(notes);
        }
        System.err.println("[probe] cycles=" + dc + " pauses=" + dp + " cycle_ms=" + dct + " pause_ms=" + dpt);

        boolean runsGe3 = dc > 0;
        boolean inside = true;
        boolean sumLe = true;
        boolean zero = true;
        boolean idsConsecutive = true;
        long lastPauseId = p0;
        int pauseNotes = 0;
        int cycleNotes = 0;
        TreeSet<String> causes = new TreeSet<>();
        List<Note> run = new ArrayList<>();
        StringBuilder runs = new StringBuilder();
        for (Note x : got) {
            causes.add(x.bean() + "|" + x.cause());
            if (x.bean().equals(PAUSES)) {
                pauseNotes++;
                idsConsecutive &= x.id() == lastPauseId + 1;
                lastPauseId = x.id();
                zero &= x.zeroUsage();
                run.add(x);
            } else if (x.bean().equals(CYCLES)) {
                cycleNotes++;
                runs.append(run.size()).append(' ');
                runsGe3 &= run.size() >= 3;
                long sum = 0;
                for (Note p : run) {
                    inside &= p.start() >= x.start() && p.end() <= x.end() && p.start() <= p.end();
                    sum += p.duration();
                }
                sumLe &= sum <= x.duration();
                run.clear();
            }
        }
        System.err.println("[probe] pause runs before each cycle: " + runs.toString().trim());
        System.out.println("pause_runs_before_each_cycle_ge_3=" + runsGe3);
        System.out.println("pauses_per_cycle_ge_3=" + (dc > 0 && dp >= 3 * dc));
        System.out.println("pause_notifications_match_count=" + (pauseNotes == dp));
        System.out.println("cycle_notifications_match_count=" + (cycleNotes == dc));
        System.out.println("pause_ids_consecutive=" + idsConsecutive);
        System.out.println("pauses_inside_their_cycle=" + inside);
        System.out.println("pause_durations_le_cycle=" + sumLe);
        System.out.println("pause_gcinfo_usage_zero=" + zero);
        System.out.println("pause_time_le_cycle_time=" + (dpt <= dct));
        System.out.println("causes " + causes);
        System.out.println("done");
    }
}
