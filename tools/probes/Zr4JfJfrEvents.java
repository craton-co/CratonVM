// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;

import jdk.jfr.ValueDescriptor;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * zr z4/jf: ZGC's JFR surface, recorded with {@code -XX:StartFlightRecording}
 * and read back with {@code jdk.jfr.consumer.RecordingFile}.
 *
 * <p>Two steps (a recording is only complete once the recording VM exits):
 *
 * <pre>
 *   java|cratonvm -XX:+UseZGC -Xmx256m -XX:StartFlightRecording=filename=z.jfr Zr4JfJfrEvents sysgc
 *   java Zr4JfJfrEvents read z.jfr
 * </pre>
 *
 * <p>Workloads: {@code sysgc} (three {@code System.gc()} over a 16 MiB live
 * set: collections, phase rows, relocation sets), {@code churn} (allocation
 * driven cycles), {@code stall} (at {@code -Xmx64m}: a 40 MiB live set and
 * churn, so allocations stall).
 *
 * <p>{@code read} prints, per event type of interest: {@code type <name>
 * present=<bool>} and {@code fields <name> <payload field names>} (the lines
 * to diff against HotSpot), then {@code count ...} and {@code names ...}
 * (informational: counts differ by design), and a {@code check ...} line per
 * consistency rule.
 */
public class Zr4JfJfrEvents {
    static final String[] TYPES = {
        "jdk.GarbageCollection",
        "jdk.GCPhasePause",
        "jdk.GCPhasePauseLevel1",
        "jdk.GCPhaseConcurrent",
        "jdk.GCHeapSummary",
        "jdk.ZAllocationStall",
        "jdk.ZPageAllocation",
        "jdk.ZRelocationSet",
        "jdk.ZRelocationSetGroup",
        "jdk.ZUncommit",
        "jdk.ZUnmap",
        "jdk.ZYoungGarbageCollection",
        "jdk.ZOldGarbageCollection",
        "jdk.YoungGarbageCollection",
        "jdk.OldGarbageCollection",
    };

    /** The key payload fields a reader relies on, per type. */
    static final Map<String, String[]> KEY = new HashMap<>();
    static {
        KEY.put("jdk.GarbageCollection", new String[] {"gcId", "name", "cause", "sumOfPauses", "longestPause"});
        KEY.put("jdk.GCPhasePause", new String[] {"gcId", "name"});
        KEY.put("jdk.GCPhasePauseLevel1", new String[] {"gcId", "name"});
        KEY.put("jdk.GCPhaseConcurrent", new String[] {"gcId", "name"});
        KEY.put("jdk.GCHeapSummary", new String[] {"gcId", "when", "heapUsed"});
        KEY.put("jdk.ZAllocationStall", new String[] {"type", "size"});
        KEY.put("jdk.ZRelocationSet", new String[] {"total", "empty", "relocate"});
        KEY.put("jdk.ZRelocationSetGroup", new String[] {"type", "candidatePages", "total", "empty", "selectedPages", "relocate"});
        KEY.put("jdk.ZUncommit", new String[] {"uncommitted"});
        KEY.put("jdk.ZOldGarbageCollection", new String[] {"gcId"});
        KEY.put("jdk.ZYoungGarbageCollection", new String[] {"gcId", "tenuringThreshold"});
    }

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgc";
        switch (mode) {
            case "read" -> read(Paths.get(args[1]));
            case "sysgc" -> sysgc();
            case "churn" -> churn(args.length > 1 ? Integer.parseInt(args[1]) : 2_000_000);
            case "stall" -> stall(args.length > 1 ? Integer.parseInt(args[1]) : 3_000_000);
            default -> throw new IllegalArgumentException(mode);
        }
    }

    static List<byte[]> live(int mib) {
        List<byte[]> l = new ArrayList<>();
        for (int i = 0; i < mib * 16; i++) {
            l.add(new byte[64 * 1024]);
        }
        return l;
    }

    static void sysgc() {
        List<byte[]> keep = live(16);
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 200_000; i++) {
                sink = new byte[128];
            }
            System.gc();
        }
        System.out.println("workload sysgc done live=" + keep.size());
    }

    static void churn(int n) {
        List<byte[]> keep = live(16);
        byte[][] window = new byte[4096][];
        for (int i = 0; i < n; i++) {
            window[i & 4095] = new byte[256 + (i & 1023)];
        }
        sink = window;
        System.out.println("workload churn done live=" + keep.size());
    }

    static void stall(int n) {
        List<byte[]> keep = live(40);
        byte[][] window = new byte[2048][];
        for (int i = 0; i < n; i++) {
            window[i & 2047] = new byte[1024 + (i & 4095)];
        }
        sink = window;
        System.out.println("workload stall done live=" + keep.size());
    }

    static void read(Path file) throws Exception {
        Map<String, Integer> counts = new TreeMap<>();
        Map<String, List<String>> fields = new HashMap<>();
        Map<String, Boolean> keyOk = new HashMap<>();
        Map<String, TreeSet<String>> names = new TreeMap<>();
        Map<Integer, Long> gcDur = new HashMap<>();
        Map<Integer, Long> level1Sum = new HashMap<>();
        TreeSet<Integer> gcIds = new TreeSet<>();
        TreeSet<Integer> phaseIds = new TreeSet<>();
        try (RecordingFile rf = new RecordingFile(file)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String t = e.getEventType().getName();
                counts.merge(t, 1, Integer::sum);
                if (!fields.containsKey(t)) {
                    List<String> f = new ArrayList<>();
                    for (ValueDescriptor d : e.getFields()) {
                        String n = d.getName();
                        if (!n.equals("startTime") && !n.equals("duration") && !n.equals("eventThread")
                                && !n.equals("stackTrace")) {
                            f.add(n);
                        }
                    }
                    fields.put(t, f);
                }
                String[] key = KEY.get(t);
                if (key != null) {
                    boolean ok = true;
                    for (String k : key) {
                        ok &= e.hasField(k);
                    }
                    keyOk.merge(t, ok, Boolean::logicalAnd);
                }
                if (t.startsWith("jdk.GCPhase") || t.equals("jdk.GarbageCollection")) {
                    Object n = e.getValue("name");
                    names.computeIfAbsent(t, k -> new TreeSet<>()).add(String.valueOf(n));
                }
                if (t.equals("jdk.ZRelocationSetGroup") || t.equals("jdk.ZAllocationStall")) {
                    Object ty = e.getValue("type");
                    names.computeIfAbsent(t, k -> new TreeSet<>()).add(String.valueOf(ty));
                }
                if (t.equals("jdk.GarbageCollection")) {
                    int id = e.getInt("gcId");
                    gcIds.add(id);
                    gcDur.put(id, e.getDuration().toNanos());
                }
                if (t.startsWith("jdk.GCPhase")) {
                    phaseIds.add(e.getInt("gcId"));
                }
                if (t.equals("jdk.GCPhasePauseLevel1")) {
                    level1Sum.merge(e.getInt("gcId"), e.getDuration().toNanos(), Long::sum);
                }
            }
        }
        for (String t : TYPES) {
            boolean present = counts.containsKey(t);
            System.out.println("type " + t + " present=" + present
                    + (present && KEY.containsKey(t) ? " key_fields=" + keyOk.getOrDefault(t, false) : ""));
            if (present) {
                System.out.println("fields " + t + " " + String.join(",", fields.get(t)));
            }
        }
        for (String t : TYPES) {
            if (counts.containsKey(t)) {
                System.out.println("count " + t + " " + counts.get(t));
            }
        }
        for (Map.Entry<String, TreeSet<String>> n : names.entrySet()) {
            System.out.println("names " + n.getKey() + " " + n.getValue());
        }
        // Every phase row files under a gcId a collection carries.
        TreeSet<Integer> orphan = new TreeSet<>(phaseIds);
        orphan.removeAll(gcIds);
        System.out.println("check phase_ids_have_a_collection=" + orphan.isEmpty()
                + (orphan.isEmpty() ? "" : " orphans=" + orphan));
        // Informational (CratonVM's level-1 rows partition its one pause;
        // HotSpot's are sub-phases of several pauses): level-1 sum <= collection.
        int over = 0;
        for (Map.Entry<Integer, Long> s : level1Sum.entrySet()) {
            Long d = gcDur.get(s.getKey());
            if (d != null && s.getValue() > d + 1_000_000L) {
                over++;
            }
        }
        System.out.println("info level1_rows_over_collection=" + over + " of " + level1Sum.size());
    }
}
