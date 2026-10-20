// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Path;
import java.time.Instant;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i5g/g1o (2026-10-12), rewritten by gci i8/jf (2026-10-13): the G1 half of
 * {@code gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted}, as a
 * program (the G1 twin of {@code GcfF3cJfrPhaseOrder}). Run it with the HOST
 * JDK's {@code java} on a recording, once on CratonVM's and once on HotSpot
 * 25's ({@code -XX:+UseG1GC -XX:+ExplicitGCInvokesConcurrent}):
 * <pre>
 *   java -cp &lt;classes&gt; GciI5gG1oJfrCycleCheck /path/to/recording.jfr
 * </pre>
 * The i5g version required a {@code jdk.GCPhaseConcurrent} row named
 * {@code Concurrent Mark}, which HotSpot G1 never records (its marking phase
 * is {@code Concurrent Mark From Roots}), so it failed HotSpot's own recording
 * (vi5g-g1o J0, marks=0). This version checks HotSpot G1's shape:
 * <ul>
 * <li>a concurrent cycle is a {@code gcId} with a {@code jdk.GCPhasePause}
 *     row named {@code Pause Remark};</li>
 * <li>{@code mark-before-remark}: the cycle's id has a
 *     {@code Concurrent Mark From Roots} row ({@code jdk.GCPhaseConcurrent} or
 *     {@code jdk.GCPhaseConcurrentLevel1}) starting before its first remark;</li>
 * <li>{@code start-before-mark}: some {@code GC Pause} row (the concurrent
 *     start's young pause) has a SMALLER id and starts no later than that
 *     marking;</li>
 * <li>{@code cleanup-after-remark}: the cycle's id has a {@code Pause Cleanup}
 *     row starting no earlier than its first remark; only the highest-id cycle
 *     may lack one (the recording may end between the two pauses), and at
 *     least one cycle must have one;</li>
 * <li>{@code hotspot-names}: no {@code jdk.GCPhasePause} named
 *     {@code Pause Init Mark} and no {@code jdk.GCPhaseConcurrent} named
 *     {@code Concurrent Mark} (names HotSpot G1 never records).</li>
 * </ul>
 * Stdout (deterministic, one line):
 * <pre>
 *   jfr-g1-cycle cycles>=1 start-before-mark=ok mark-before-remark=ok cleanup-after-remark=ok hotspot-names=ok PROBE-OK
 * </pre>
 * {@code cycles=0} (no remark row at all) is a failure of the recording row,
 * not a pass. Counts, names and the first offending ids go to stderr.
 */
public final class GciI5gG1oJfrCycleCheck {
    private static void earliest(Map<Integer, Instant> m, int id, Instant t) {
        m.merge(id, t, (a, b) -> a.isBefore(b) ? a : b);
    }

    public static void main(String[] args) throws Exception {
        List<RecordedEvent> events = RecordingFile.readAllEvents(Path.of(args[0]));
        Map<Integer, Instant> remarkStart = new HashMap<>(); // gcId -> earliest Pause Remark start
        Map<Integer, Instant> cleanupStart = new HashMap<>(); // gcId -> earliest Pause Cleanup start
        Map<Integer, Instant> markStart = new HashMap<>(); // gcId -> earliest Concurrent Mark From Roots
        TreeMap<Integer, Instant> gcPauseStart = new TreeMap<>(); // gcId -> earliest GC Pause start
        Map<String, Integer> pauseNames = new TreeMap<>();
        Map<String, Integer> concNames = new TreeMap<>();
        int pauses = 0;
        int concurrent = 0;
        int foreignNames = 0;
        for (RecordedEvent e : events) {
            String type = e.getEventType().getName();
            if (type.equals("jdk.GCPhasePause")) {
                pauses++;
                String name = String.valueOf(e.getString("name"));
                int id = e.getInt("gcId");
                pauseNames.merge(name, 1, Integer::sum);
                if (name.equals("Pause Remark")) {
                    earliest(remarkStart, id, e.getStartTime());
                } else if (name.equals("Pause Cleanup")) {
                    earliest(cleanupStart, id, e.getStartTime());
                } else if (name.equals("GC Pause")) {
                    earliest(gcPauseStart, id, e.getStartTime());
                } else if (name.equals("Pause Init Mark")) {
                    foreignNames++;
                }
            } else if (type.equals("jdk.GCPhaseConcurrent") || type.equals("jdk.GCPhaseConcurrentLevel1")) {
                concurrent++;
                String name = String.valueOf(e.getString("name"));
                boolean level0 = type.equals("jdk.GCPhaseConcurrent");
                concNames.merge((level0 ? "" : "L1:") + name, 1, Integer::sum);
                if (name.equals("Concurrent Mark From Roots")) {
                    earliest(markStart, e.getInt("gcId"), e.getStartTime());
                } else if (level0 && name.equals("Concurrent Mark")) {
                    foreignNames++;
                }
            }
        }
        TreeSet<Integer> badMark = new TreeSet<>();
        TreeSet<Integer> badStart = new TreeSet<>();
        TreeSet<Integer> badCleanup = new TreeSet<>();
        TreeSet<Integer> noCleanup = new TreeSet<>();
        int withCleanup = 0;
        TreeSet<Integer> cycleIds = new TreeSet<>(remarkStart.keySet());
        for (int id : cycleIds) {
            Instant remark = remarkStart.get(id);
            Instant mark = markStart.get(id);
            if (mark == null || !mark.isBefore(remark)) {
                badMark.add(id);
            }
            boolean started = false;
            if (mark != null) {
                for (Instant s : gcPauseStart.headMap(id, false).values()) {
                    if (!s.isAfter(mark)) {
                        started = true;
                        break;
                    }
                }
            }
            if (!started) {
                badStart.add(id);
            }
            Instant cleanup = cleanupStart.get(id);
            if (cleanup == null) {
                noCleanup.add(id);
            } else if (cleanup.isBefore(remark)) {
                badCleanup.add(id);
            } else {
                withCleanup++;
            }
        }
        // Only the last cycle may end with the recording before its cleanup.
        for (int id : noCleanup) {
            if (id != cycleIds.last()) {
                badCleanup.add(id);
            }
        }
        boolean cleanupOk = badCleanup.isEmpty() && withCleanup >= 1;
        int cycles = cycleIds.size();
        boolean ok = cycles >= 1
                && badMark.isEmpty()
                && badStart.isEmpty()
                && cleanupOk
                && foreignNames == 0;
        System.out.println("jfr-g1-cycle " + (cycles >= 1 ? "cycles>=1" : "cycles=0")
                + " start-before-mark=" + (badStart.isEmpty() ? "ok" : "BAD")
                + " mark-before-remark=" + (badMark.isEmpty() ? "ok" : "BAD")
                + " cleanup-after-remark=" + (cleanupOk ? "ok" : "BAD")
                + " hotspot-names=" + (foreignNames == 0 ? "ok" : "BAD")
                + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        System.err.println("[jfr-g1-cycle] pauses=" + pauses + " concurrent=" + concurrent
                + " remarks=" + remarkStart.size() + " cleanups=" + cleanupStart.size()
                + " marks=" + markStart.size() + " gc_pauses=" + gcPauseStart.size()
                + " cycles=" + cycles + " with_cleanup=" + withCleanup
                + " bad_start=" + badStart + " bad_mark=" + badMark
                + " bad_cleanup=" + badCleanup + " no_cleanup=" + noCleanup
                + " foreign_names=" + foreignNames);
        System.err.println("[jfr-g1-cycle] pause_names=" + pauseNames);
        System.err.println("[jfr-g1-cycle] concurrent_names=" + concNames);
    }
}
