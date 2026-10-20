// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Path;
import java.time.Instant;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeSet;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gcf f3/c (2026-10-01): the ordering check of
 * {@code gengc-r4-plumbing-jfr-phase-and-concurrent-events-not-emitted} (vf1
 * row c-10), as a program instead of a by-hand {@code jfr print}. Run it with
 * the HOST JDK's {@code java} on the recording a CratonVM run wrote:
 * <pre>
 *   java -cp &lt;classes&gt; GcfF3cJfrPhaseOrder /path/to/c3_jfr_gen.jfr
 * </pre>
 * For every {@code gcId} that has a {@code jdk.GCPhaseConcurrent} row named
 * {@code Concurrent Sweep}, the sweep must start at or after the END of that
 * id's {@code jdk.GCPhasePause} row named {@code Pause Remark}; every
 * {@code Concurrent Mark} row of an id must start before that id's
 * {@code Pause Remark} starts. Stdout (deterministic):
 * <pre>
 *   jfr-phase-order cycles>=1 sweep-after-remark=ok mark-before-remark=ok PROBE-OK
 * </pre>
 * Diagnostics (counts, the first offending ids) go to stderr. A recording
 * with no cycle prints {@code cycles=0 ... PROBE-FAIL}: the run produced
 * nothing to check, which is a failure of the row, not a pass.
 */
public final class GcfF3cJfrPhaseOrder {
    public static void main(String[] args) throws Exception {
        List<RecordedEvent> events = RecordingFile.readAllEvents(Path.of(args[0]));
        Map<Integer, Instant[]> remark = new HashMap<>(); // gcId -> {start, end}
        Map<Integer, Instant> sweepStart = new HashMap<>();
        Map<Integer, Instant> markStart = new HashMap<>();
        int pauses = 0;
        int concurrent = 0;
        for (RecordedEvent e : events) {
            String type = e.getEventType().getName();
            if (type.equals("jdk.GCPhasePause")) {
                pauses++;
                if ("Pause Remark".equals(e.getString("name"))) {
                    remark.put(e.getInt("gcId"), new Instant[] {e.getStartTime(), e.getEndTime()});
                }
            } else if (type.equals("jdk.GCPhaseConcurrent")) {
                concurrent++;
                String name = e.getString("name");
                int id = e.getInt("gcId");
                if ("Concurrent Sweep".equals(name)) {
                    sweepStart.merge(id, e.getStartTime(), (a, b) -> a.isBefore(b) ? a : b);
                } else if ("Concurrent Mark".equals(name)) {
                    markStart.merge(id, e.getStartTime(), (a, b) -> a.isAfter(b) ? a : b);
                }
            }
        }
        TreeSet<Integer> badSweep = new TreeSet<>();
        TreeSet<Integer> badMark = new TreeSet<>();
        int cycles = 0;
        for (Map.Entry<Integer, Instant> s : sweepStart.entrySet()) {
            Instant[] r = remark.get(s.getKey());
            if (r == null || s.getValue().isBefore(r[1])) {
                badSweep.add(s.getKey());
            } else {
                cycles++;
            }
        }
        for (Map.Entry<Integer, Instant> m : markStart.entrySet()) {
            Instant[] r = remark.get(m.getKey());
            if (r != null && !m.getValue().isBefore(r[0])) {
                badMark.add(m.getKey());
            }
        }
        boolean ok = cycles >= 1 && badSweep.isEmpty() && badMark.isEmpty();
        System.out.println("jfr-phase-order " + (cycles >= 1 ? "cycles>=1" : "cycles=0")
                + " sweep-after-remark=" + (badSweep.isEmpty() ? "ok" : "BAD")
                + " mark-before-remark=" + (badMark.isEmpty() ? "ok" : "BAD")
                + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        System.err.println("[jfr-order] pauses=" + pauses + " concurrent=" + concurrent
                + " remarks=" + remark.size() + " sweeps=" + sweepStart.size()
                + " marks=" + markStart.size() + " ordered_cycles=" + cycles
                + " bad_sweep=" + badSweep + " bad_mark=" + badMark);
    }
}
