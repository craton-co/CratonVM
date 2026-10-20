// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

import jdk.jfr.Configuration;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

/**
 * zt t1/jf: the stop-the-world VM operations of ZGC's explicit collections,
 * as JFR records them ({@code jdk.ExecuteVMOperation},
 * {@code jdk.SafepointBegin} / {@code End}, enabled with a 0 ms threshold),
 * and the young event of a major ({@code jdk.ZYoungGarbageCollection}).
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx256m Zt1JfVmOperations
 *   cratonvm         -Xmx256m Zt1JfVmOperations
 * </pre>
 *
 * <p>zt t3/jf, rewritten against local HotSpot 25.0.3's own answer (its
 * stdout refuted the t1 guesses). A HotSpot {@code System.gc()} is a
 * promote-all young collection ({@code ZMarkStartYoung}, {@code ZMarkEndYoung},
 * {@code ZRelocateStartYoung}) then the major ({@code ZMarkStartYoungAndOld},
 * {@code ZMarkEndYoung}, {@code ZRelocateStartYoung}, {@code ZMarkEndOld},
 * {@code ZRendezvousGCThreads}, {@code ZRelocateStartOld}), with
 * {@code ZMarkFlushOperation}s requested by the {@code ZWorker*} threads during
 * the marks: 8 pause operations for 8 {@code jdk.GCPhasePause} rows. This
 * VM's whole-heap collection runs 3. stdout (default mode) states what holds
 * on both: every {@code Z...} operation strictly inside an explicit
 * collection is one of HotSpot's eight ZGC operation names; the major's old
 * side ({@code ZMarkStartYoungAndOld} .. {@code ZMarkEndOld} ..
 * {@code ZRelocateStartOld}) runs in that order; the pause operations are as
 * many as the pause rows; each pause operation is a safepoint with its
 * begin / end rows; the pause operations' callers; the major's young event.
 * {@code seq} mode prints each collection's pause-operation sequence on
 * stdout (the structural row: HotSpot 8, this VM 3).
 * stderr carries every operation and the other callers ({@code info}).
 */
public class Zt1JfVmOperations {
    static final Set<String> HOTSPOT_ZGC_OPS = Set.of(
            "ZMarkStartYoung", "ZMarkStartYoungAndOld", "ZMarkEndYoung", "ZMarkEndOld",
            "ZRelocateStartYoung", "ZRelocateStartOld");
    /**
     * zt t3/jf: HotSpot 25's other ZGC operations inside a collection, with no
     * pause row of their own (local 25.0.3: `ZMarkFlushOperation` several times
     * per mark, `ZRendezvousGCThreads` before the old relocate start; their
     * callers are the `ZWorker*` threads).
     */
    static final Set<String> OTHER_ZGC_OPS = Set.of("ZMarkFlushOperation", "ZRendezvousGCThreads");
    /** The major's old side, in HotSpot's order. */
    static final List<String> MAJOR_ORDER = List.of("ZMarkStartYoungAndOld", "ZMarkEndOld", "ZRelocateStartOld");

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        Recording r = new Recording(Configuration.getConfiguration("default"));
        for (String t : new String[] {"jdk.ExecuteVMOperation", "jdk.SafepointBegin", "jdk.SafepointEnd",
                "jdk.GCPhasePause", "jdk.GarbageCollection", "jdk.ZYoungGarbageCollection"}) {
            r.enable(t).withThreshold(Duration.ZERO);
        }
        r.start();
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 16 * 16; i++) {
            keep.add(new byte[64 * 1024]);
        }
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 200_000; i++) {
                sink = new byte[128];
            }
            System.gc();
        }
        r.stop();
        Path p = Files.createTempFile("zt1jf", ".jfr");
        r.dump(p);
        r.close();
        List<RecordedEvent> events = RecordingFile.readAllEvents(p);
        Files.deleteIfExists(p);
        boolean seq = args.length > 0 && args[0].equals("seq");
        System.out.println("workload live=" + keep.size());
        report(events, seq);
    }

    static String type(RecordedEvent e) {
        return e.getEventType().getName();
    }

    static long start(RecordedEvent e) {
        Instant s = e.getStartTime();
        return s.getEpochSecond() * 1_000_000_000L + s.getNano();
    }

    static long end(RecordedEvent e) {
        Instant s = e.getEndTime();
        return s.getEpochSecond() * 1_000_000_000L + s.getNano();
    }

    static boolean inside(RecordedEvent e, RecordedEvent g) {
        return start(e) >= start(g) - 1_000_000 && end(e) <= end(g) + 1_000_000;
    }

    static String threadName(RecordedThread t) {
        if (t == null) {
            return "<none>";
        }
        return t.getJavaName() != null ? t.getJavaName() : String.valueOf(t.getOSName());
    }

    static void report(List<RecordedEvent> events, boolean seq) {
        Map<Integer, RecordedEvent> gcs = new TreeMap<>();
        for (RecordedEvent e : events) {
            if (type(e).equals("jdk.GarbageCollection") && "System.gc()".equals(String.valueOf(e.getString("cause")))) {
                gcs.put(e.getInt("gcId"), e);
            }
        }
        Set<Long> begins = new TreeSet<>();
        Set<Long> ends = new TreeSet<>();
        List<RecordedEvent> vmops = new ArrayList<>();
        for (RecordedEvent e : events) {
            switch (type(e)) {
                case "jdk.SafepointBegin" -> begins.add(e.getLong("safepointId"));
                case "jdk.SafepointEnd" -> ends.add(e.getLong("safepointId"));
                case "jdk.ExecuteVMOperation" -> vmops.add(e);
                default -> { }
            }
        }
        // zt t3/jf: in time order (the file's order is per thread buffer).
        vmops.sort((a, b) -> Long.compare(start(a), start(b)));
        boolean any = !gcs.isEmpty();
        boolean names = any, oldSide = any, perPause = any, atSafepoint = any, young = any, youngInside = any;
        TreeSet<String> allNames = new TreeSet<>();
        TreeSet<String> pauseCallers = new TreeSet<>();
        TreeSet<String> otherCallers = new TreeSet<>();
        for (Map.Entry<Integer, RecordedEvent> g : gcs.entrySet()) {
            List<String> ops = new ArrayList<>();
            List<String> pauseOps = new ArrayList<>();
            int pauses = 0;
            boolean hasYoung = false;
            for (RecordedEvent e : vmops) {
                String op = String.valueOf(e.getString("operation"));
                if (!op.startsWith("Z") || !strictlyInside(e, g.getValue())) {
                    continue;
                }
                ops.add(op);
                allNames.add(op);
                names &= HOTSPOT_ZGC_OPS.contains(op) || OTHER_ZGC_OPS.contains(op);
                if (HOTSPOT_ZGC_OPS.contains(op)) {
                    pauseOps.add(op);
                    pauseCallers.add(threadName(e.getThread("caller")));
                    long sp = e.getLong("safepointId");
                    atSafepoint &= e.getBoolean("safepoint") && begins.contains(sp) && ends.contains(sp);
                } else {
                    otherCallers.add(threadName(e.getThread("caller")));
                }
            }
            for (RecordedEvent e : events) {
                switch (type(e)) {
                    case "jdk.GCPhasePause" -> pauses += e.getInt("gcId") == g.getKey() ? 1 : 0;
                    case "jdk.ZYoungGarbageCollection" -> {
                        if (e.getInt("gcId") == g.getKey()) {
                            hasYoung = true;
                            // zt t3/jf (zt2x1 item 4): HotSpot's young
                            // collection ends before its major does.
                            youngInside &= end(e) <= end(g.getValue())
                                    && e.getDuration().compareTo(g.getValue().getDuration()) < 0;
                        }
                    }
                    default -> { }
                }
            }
            // The major's old side, in this order (other operations between).
            int at = 0;
            for (String op : pauseOps) {
                if (at < MAJOR_ORDER.size() && op.equals(MAJOR_ORDER.get(at))) {
                    at++;
                }
            }
            oldSide &= at == MAJOR_ORDER.size();
            perPause &= pauseOps.size() == pauses;
            young &= hasYoung;
            if (seq) {
                System.out.println("seq pause_ops=" + pauseOps);
            }
            System.err.println("info gc " + g.getKey() + " ops=" + ops + " pauses=" + pauses);
        }
        if (seq) {
            return;
        }
        System.out.println("vmops sysgc_collections=" + gcs.size());
        System.out.println("vmops names_in_hotspot_set=" + names + " major_old_side_in_order=" + oldSide
                + " pause_ops_match_pause_rows=" + perPause + " pause_ops_at_safepoint=" + atSafepoint);
        System.out.println("vmops pause_op_callers=" + pauseCallers);
        System.out.println("z_young per_major=" + young + " shorter_than_major=" + youngInside);
        System.err.println("info names=" + allNames + " other_callers=" + otherCallers);
    }

    /**
     * Inside the collection's own span, no slack: a neighbouring cycle's last
     * operation is not this one's (HotSpot's first explicit collection showed
     * a `ZRelocateStartOld` of the cycle before it under the 1 ms slack).
     */
    static boolean strictlyInside(RecordedEvent e, RecordedEvent g) {
        return start(e) >= start(g) && end(e) <= end(g);
    }
}
