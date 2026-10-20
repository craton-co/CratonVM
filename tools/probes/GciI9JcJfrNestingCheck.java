// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i9/jc (2026-10-13): does a recording nest a collection's phases as
 * HotSpot Serial does? HotSpot writes, per stop-the-world collection, one
 * top-level {@code jdk.GCPhasePause} named {@code GC Pause} under the
 * collection's {@code gcId}, and the collection's phases as
 * {@code jdk.GCPhasePauseLevel1} rows inside it. CratonVM's legacy recorder
 * wrote the Generational phases as level-0 {@code jdk.GCPhasePause} rows with
 * no parent (lane i8/jf's finding). Run with the HOST JDK's {@code java}:
 * <pre>
 *   java -cp &lt;classes&gt; GciI9JcJfrNestingCheck /path/to/recording.jfr
 * </pre>
 * Checks:
 * <ul>
 *   <li>{@code pause-per-gc}: every {@code gcId} with a
 *       {@code jdk.GarbageCollection} has exactly one {@code GC Pause} row,
 *       except a concurrent cycle's id (one with a {@code Pause Init Mark},
 *       {@code Pause Remark} or {@code Pause Cleanup} row: HotSpot G1 reports
 *       its cycle as a {@code G1Old} collection whose pauses are those);</li>
 *   <li>{@code level1-inside}: every {@code jdk.GCPhasePauseLevel1} row lies
 *       inside a level-0 {@code jdk.GCPhasePause} row of its own {@code gcId}
 *       (1 us slack): its {@code GC Pause}, or, for a concurrent cycle, the
 *       {@code Pause Remark} / {@code Pause Cleanup} that owns it (HotSpot G1
 *       nests Remark's and Cleanup's phases under those rows; vi9-jc N0,
 *       2026-10-13: the first version demanded a {@code GC Pause} and failed
 *       on HotSpot's own recording);</li>
 *   <li>{@code no-flat-phases}: every level-0 {@code jdk.GCPhasePause} is
 *       {@code GC Pause} or a concurrent cycle's pause ({@code Pause Init
 *       Mark}, {@code Pause Remark}, {@code Pause Cleanup}).</li>
 * </ul>
 * Stdout (deterministic):
 * <pre>
 *   jfr-nesting gcs>=1 pause-per-gc=ok level1-inside=ok no-flat-phases=ok level1=present PROBE-OK
 * </pre>
 * {@code level1=present|none} is informational (a recording may have no
 * phase rows); it is not part of {@code PROBE-OK}. Diagnostics (counts, the
 * first offending ids and names) go to stderr.
 */
public final class GciI9JcJfrNestingCheck {
    private static final Duration SLACK = Duration.ofNanos(1_000);
    private static final Set<String> LEVEL0_NAMES =
            Set.of("GC Pause", "Pause Init Mark", "Pause Remark", "Pause Cleanup");

    public static void main(String[] args) throws Exception {
        List<RecordedEvent> events = RecordingFile.readAllEvents(Path.of(args[0]));
        Set<Integer> gcIds = new HashSet<>();
        Map<Integer, List<Instant[]>> gcPause = new HashMap<>(); // gcId -> GC Pause spans
        Map<Integer, List<Instant[]>> level0 = new HashMap<>(); // gcId -> every level-0 pause span
        List<RecordedEvent> level1 = new ArrayList<>();
        Map<String, Integer> flatNames = new TreeMap<>();
        Set<Integer> cycleIds = new HashSet<>(); // ids with a concurrent cycle's pause
        for (RecordedEvent e : events) {
            String type = e.getEventType().getName();
            if (type.equals("jdk.GarbageCollection")) {
                gcIds.add(e.getInt("gcId"));
            } else if (type.equals("jdk.GCPhasePause")) {
                String name = String.valueOf(e.getString("name"));
                level0.computeIfAbsent(e.getInt("gcId"), k -> new ArrayList<>())
                        .add(new Instant[] {e.getStartTime(), e.getEndTime()});
                if (name.equals("GC Pause")) {
                    gcPause.computeIfAbsent(e.getInt("gcId"), k -> new ArrayList<>())
                            .add(new Instant[] {e.getStartTime(), e.getEndTime()});
                } else if (!LEVEL0_NAMES.contains(name)) {
                    flatNames.merge(name, 1, Integer::sum);
                } else {
                    cycleIds.add(e.getInt("gcId"));
                }
            } else if (type.equals("jdk.GCPhasePauseLevel1")) {
                level1.add(e);
            }
        }
        TreeSet<Integer> badPause = new TreeSet<>();
        for (int id : gcIds) {
            if (cycleIds.contains(id)) {
                continue; // a concurrent cycle (HotSpot G1's G1Old) has no `GC Pause`
            }
            List<Instant[]> spans = gcPause.get(id);
            if (spans == null || spans.size() != 1) {
                badPause.add(id);
            }
        }
        TreeSet<Integer> badInside = new TreeSet<>();
        for (RecordedEvent e : level1) {
            int id = e.getInt("gcId");
            List<Instant[]> spans = level0.get(id);
            boolean inside = false;
            if (spans != null) {
                for (Instant[] span : spans) {
                    if (!e.getStartTime().isBefore(span[0].minus(SLACK))
                            && !e.getEndTime().isAfter(span[1].plus(SLACK))) {
                        inside = true;
                        break;
                    }
                }
            }
            if (!inside) {
                badInside.add(id);
            }
        }
        int gcs = gcIds.size();
        boolean ok = gcs >= 1 && badPause.isEmpty() && badInside.isEmpty() && flatNames.isEmpty();
        System.out.println("jfr-nesting " + (gcs >= 1 ? "gcs>=1" : "gcs=0")
                + " pause-per-gc=" + (badPause.isEmpty() ? "ok" : "BAD")
                + " level1-inside=" + (badInside.isEmpty() ? "ok" : "BAD")
                + " no-flat-phases=" + (flatNames.isEmpty() ? "ok" : "BAD")
                + " level1=" + (level1.isEmpty() ? "none" : "present")
                + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        System.err.println("[nesting] gcs=" + gcs + " gc_pause_ids=" + gcPause.size()
                + " level1_rows=" + level1.size()
                + " bad_pause=" + first(badPause) + " bad_inside=" + first(badInside)
                + " flat_names=" + flatNames);
    }

    private static String first(TreeSet<Integer> ids) {
        StringBuilder sb = new StringBuilder();
        sb.append(ids.size()).append('[');
        int n = 0;
        for (int id : ids) {
            if (n++ == 8) {
                sb.append(",...");
                break;
            }
            if (n > 1) {
                sb.append(',');
            }
            sb.append(id);
        }
        return sb.append(']').toString();
    }
}
