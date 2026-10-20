// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i42/mx ({@code gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-20261016}):
 * the periodic GC configuration events of one recording, field by field,
 * against HotSpot 25.
 *
 * <p>One recording of {@code jdk.GCConfiguration}, {@code jdk.GCHeapConfiguration},
 * {@code jdk.GCHeapMemoryPoolUsage} and the four flag events (period
 * {@code everyChunk}) around one {@code System.gc()}. Every distinct line is
 * printed once, sorted, so a configuration written at both chunk edges prints
 * once.
 *
 * <p>stdout (deterministic; run both sides at {@code -Xms == -Xmx} and HotSpot
 * with {@code -XX:-UseCompressedOops}, this VM's narrow-oop state):
 * <pre>
 * mx42 config young=.. old=.. parallel=N concurrent=N dynamic=.. explicitConcurrent=.. explicitDisabled=.. pauseTarget=..
 * mx42 heapconfig min=.. initial=.. max=.. compressed=.. mode=.. align=.. bits=..
 * mx42 pool name=&lt;heap pool&gt; usedLeCommitted=true      (one per heap pool, sorted)
 * mx42 flag InitialHeapSize=.. / MaxHeapSize=.. / ParallelGCThreads=.. / UseCompressedOops=.. / UseG1GC=.. / UseSerialGC=..
 * mx42 events config=true heapconfig=true pools=true flags=true
 * </pre>
 * stderr: {@code [mx42] gcTimeRatio=N} (the census of the page's residual, not
 * judged by stdout) and the raw pool rows.
 */
public class GciMx42JfrGcConfigProbe {
    static final String[] FLAGS = {
        "InitialHeapSize", "MaxHeapSize", "ParallelGCThreads", "UseCompressedOops", "UseG1GC", "UseSerialGC",
    };

    static String str(RecordedEvent e, String field) {
        // Through Object: `String.valueOf` of a generic `getValue` would bind
        // the `char[]` overload.
        Object raw = e.getValue(field);
        return String.valueOf(raw);
    }

    public static void main(String[] args) throws Exception {
        Path file = Files.createTempFile("mx42-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.GCConfiguration").with("period", "everyChunk");
            r.enable("jdk.GCHeapConfiguration").with("period", "everyChunk");
            r.enable("jdk.GCHeapMemoryPoolUsage").with("period", "everyChunk");
            r.enable("jdk.BooleanFlag").with("period", "everyChunk");
            r.enable("jdk.UnsignedLongFlag").with("period", "everyChunk");
            r.enable("jdk.UnsignedIntFlag").with("period", "everyChunk");
            r.start();
            System.gc();
            r.stop();
            r.dump(file);
        }
        TreeSet<String> config = new TreeSet<>();
        TreeSet<String> heap = new TreeSet<>();
        TreeSet<String> pools = new TreeSet<>();
        TreeSet<String> ratios = new TreeSet<>();
        Map<String, TreeSet<String>> flags = new TreeMap<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            String name = e.getEventType().getName();
            switch (name) {
                case "jdk.GCConfiguration" -> {
                    config.add("mx42 config young=" + str(e, "youngCollector")
                            + " old=" + str(e, "oldCollector")
                            + " parallel=" + str(e, "parallelGCThreads")
                            + " concurrent=" + str(e, "concurrentGCThreads")
                            + " dynamic=" + str(e, "usesDynamicGCThreads")
                            + " explicitConcurrent=" + str(e, "isExplicitGCConcurrent")
                            + " explicitDisabled=" + str(e, "isExplicitGCDisabled")
                            + " pauseTarget=" + str(e, "pauseTarget"));
                    ratios.add(str(e, "gcTimeRatio"));
                }
                case "jdk.GCHeapConfiguration" -> heap.add("mx42 heapconfig min=" + str(e, "minSize")
                        + " initial=" + str(e, "initialSize")
                        + " max=" + str(e, "maxSize")
                        + " compressed=" + str(e, "usesCompressedOops")
                        + " mode=" + str(e, "compressedOopsMode")
                        + " align=" + str(e, "objectAlignment")
                        + " bits=" + str(e, "heapAddressBits"));
                case "jdk.GCHeapMemoryPoolUsage" -> {
                    long used = e.getLong("used");
                    long committed = e.getLong("committed");
                    pools.add("mx42 pool name=" + str(e, "name") + " usedLeCommitted=" + (used <= committed));
                    System.err.println("[mx42] pool " + str(e, "name") + " used=" + used
                            + " committed=" + committed + " max=" + e.getLong("max"));
                }
                case "jdk.BooleanFlag", "jdk.UnsignedLongFlag", "jdk.UnsignedIntFlag" -> {
                    String flag = str(e, "name");
                    for (String wanted : FLAGS) {
                        if (wanted.equals(flag)) {
                            flags.computeIfAbsent(flag, k -> new TreeSet<>()).add(str(e, "value"));
                        }
                    }
                }
                default -> {
                }
            }
        }
        Files.deleteIfExists(file);
        config.forEach(System.out::println);
        heap.forEach(System.out::println);
        pools.forEach(System.out::println);
        for (String wanted : FLAGS) {
            TreeSet<String> values = flags.get(wanted);
            System.out.println("mx42 flag " + wanted + "=" + (values == null ? "none" : String.join(",", values)));
        }
        System.out.println("mx42 events config=" + !config.isEmpty() + " heapconfig=" + !heap.isEmpty()
                + " pools=" + !pools.isEmpty() + " flags=" + !flags.isEmpty());
        System.err.println("[mx42] gcTimeRatio=" + String.join(",", ratios));
    }
}
