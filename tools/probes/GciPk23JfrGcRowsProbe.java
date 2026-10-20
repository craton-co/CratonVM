// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.List;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedFrame;
import jdk.jfr.consumer.RecordedStackTrace;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i23/pk: three JFR GC-row residuals of the {@code jdk.jfr} backend
 * against HotSpot 25.
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code sysgc} (gci-i19ob-jfr-gc-row-field-residuals items 2-3): one
 *       {@code System.gc()} from {@code requestGc()} inside a recording of
 *       {@code jdk.SystemGC} (with stack traces) and
 *       {@code jdk.MetaspaceSummary}. HotSpot's {@code JVM_GC} records the
 *       stack from the {@code Runtime.gc()} native frame down, and
 *       {@code gcThreshold} is {@code MetaspaceGC::capacity_until_GC()}
 *       (this VM wrote no stack and a 0 threshold).</li>
 *   <li>{@code g1cycle} (gci-i20ob-g1-remark-and-cleanup-write-no-vm-operation-rows):
 *       an old generation that keeps turning over (concurrent cycles at
 *       -Xmx96m) inside a recording of {@code jdk.ExecuteVMOperation} and
 *       {@code jdk.GCPhasePause} at threshold 0. HotSpot runs every G1 Remark
 *       and Cleanup pause as a VM operation ({@code G1PauseRemark},
 *       {@code G1PauseCleanup}), one per {@code Pause Remark} /
 *       {@code Pause Cleanup} phase row (this VM wrote none).</li>
 * </ul>
 *
 * <p>stdout (deterministic, the same on HotSpot):
 * <pre>
 * pk23 sysgc events=1 stack=true
 * pk23 sysgc frame0=java.lang.Runtime.gc
 * pk23 sysgc frame1=java.lang.System.gc
 * pk23 sysgc frame2=GciPk23JfrGcRowsProbe.requestGc
 * pk23 metaspace rows=true gcThreshold=&lt;HotSpot's capacity_until_GC&gt;
 * </pre>
 * <pre>
 * pk23 g1cycle remark=yes cleanup=yes
 * pk23 g1cycle remark-ops-match-rows=yes cleanup-ops-match-rows=yes
 * </pre>
 * stderr: {@code [pk23] ...} counts and every distinct operation name.
 */
public class GciPk23JfrGcRowsProbe {
    static volatile Object sink;

    static void requestGc() {
        System.gc();
    }

    static String frameName(RecordedFrame f) {
        return f.getMethod().getType().getName() + "." + f.getMethod().getName();
    }

    static void sysgc() throws Exception {
        Path file = Files.createTempFile("pk23s-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.SystemGC").withStackTrace();
            r.enable("jdk.MetaspaceSummary");
            r.start();
            requestGc();
            r.stop();
            r.dump(file);
        }
        int events = 0;
        List<RecordedFrame> frames = null;
        long threshold = -1;
        int metaRows = 0;
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            String name = e.getEventType().getName();
            if (name.equals("jdk.SystemGC")) {
                events++;
                RecordedStackTrace st = e.getStackTrace();
                if (frames == null && st != null) {
                    frames = st.getFrames();
                }
            } else if (name.equals("jdk.MetaspaceSummary")) {
                metaRows++;
                if (threshold < 0) {
                    threshold = e.getLong("gcThreshold");
                }
            }
        }
        Files.deleteIfExists(file);
        System.out.println("pk23 sysgc events=" + events + " stack=" + (frames != null && !frames.isEmpty()));
        for (int i = 0; i < 3; i++) {
            String f = frames != null && i < frames.size() ? frameName(frames.get(i)) : "none";
            System.out.println("pk23 sysgc frame" + i + "=" + f);
        }
        if (frames != null) {
            for (RecordedFrame f : frames) {
                System.err.println("[pk23] frame " + frameName(f) + " line=" + f.getLineNumber()
                        + " type=" + f.getType());
            }
        }
        System.err.println("[pk23] metaspace rows=" + metaRows + " gcThreshold=" + threshold);
        System.out.println("pk23 metaspace rows=" + (metaRows > 0) + " gcThreshold=" + threshold);
    }

    static void mixedChurn() {
        int slots = 12 * 1024;
        Object[] table = new Object[slots];
        for (int i = 0; i < slots; i++) {
            table[i] = new byte[4096];
        }
        long seed = 12345;
        for (int round = 0; round < 40; round++) {
            for (int k = 0; k < slots / 4; k++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int at = (int) ((seed >>> 33) % slots);
                table[at] = new byte[4096];
            }
            Object[] window = new Object[256];
            for (int i = 0; i < 16 * 1024; i++) {
                window[i % window.length] = new byte[512];
            }
            sink = window;
        }
        sink = table;
    }

    static void g1cycle() throws Exception {
        Path file = Files.createTempFile("pk23g-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ExecuteVMOperation").withThreshold(Duration.ZERO);
            r.enable("jdk.GCPhasePause").withThreshold(Duration.ZERO);
            r.start();
            mixedChurn();
            r.stop();
            r.dump(file);
        }
        TreeMap<String, Integer> ops = new TreeMap<>();
        TreeMap<String, Integer> pauses = new TreeMap<>();
        TreeSet<String> opNames = new TreeSet<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            String name = e.getEventType().getName();
            if (name.equals("jdk.ExecuteVMOperation")) {
                // Through Object: `String.valueOf` of a generic `getValue`
                // would bind the `char[]` overload.
                Object raw = e.getValue("operation");
                String op = String.valueOf(raw);
                opNames.add(op);
                ops.merge(op, 1, Integer::sum);
            } else if (name.equals("jdk.GCPhasePause")) {
                pauses.merge(e.getString("name"), 1, Integer::sum);
            }
        }
        Files.deleteIfExists(file);
        int remarkOps = ops.getOrDefault("G1PauseRemark", 0);
        int cleanupOps = ops.getOrDefault("G1PauseCleanup", 0);
        int remarkRows = pauses.getOrDefault("Pause Remark", 0);
        int cleanupRows = pauses.getOrDefault("Pause Cleanup", 0);
        System.err.println("[pk23] ops=" + ops + " pauses=" + pauses + " names=" + opNames);
        System.out.println("pk23 g1cycle remark=" + (remarkOps > 0 ? "yes" : "no")
                + " cleanup=" + (cleanupOps > 0 ? "yes" : "no"));
        // Judged only where the operations exist: a Generational cycle writes
        // `Pause Remark` phase rows of its own with no HotSpot operation (the
        // first line already says remark=no there).
        boolean remarkMatch = remarkOps == 0 || remarkOps == remarkRows;
        boolean cleanupMatch = cleanupOps == 0 || cleanupOps == cleanupRows;
        System.out.println("pk23 g1cycle remark-ops-match-rows=" + (remarkMatch ? "yes" : "no")
                + " cleanup-ops-match-rows=" + (cleanupMatch ? "yes" : "no"));
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgc";
        if (mode.equals("g1cycle")) {
            g1cycle();
        } else {
            sysgc();
        }
    }
}
