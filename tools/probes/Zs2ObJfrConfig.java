// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.TreeMap;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordingFile;

/**
 * zs s2/ob (2026-10-14): ZGC's {@code jdk.GCConfiguration} and
 * {@code jdk.GCHeapConfiguration} as a recording started from Java (the real
 * JFR backend) writes them. stdout prints only fields that are deterministic
 * for the command line on HotSpot 25.0.3 (local): the collector names, the
 * explicit-GC and dynamic-thread flags, {@code pauseTarget}, {@code gcTimeRatio},
 * the heap sizes when {@code -Xms} pins them, the oop mode; and the thread
 * counts only when the command line gives them (argument {@code threads}).
 *
 * <p>Rows: {@code -XX:+UseZGC -Xmx256m -Xms64m Zs2ObJfrConfig} and
 * {@code -XX:+UseZGC -Xmx256m -Xms64m -XX:ParallelGCThreads=3 -XX:ConcGCThreads=1
 * -XX:+DisableExplicitGC Zs2ObJfrConfig threads}.
 */
public final class Zs2ObJfrConfig {
    public static void main(String[] args) throws Exception {
        boolean threads = args.length > 0 && args[0].equals("threads");
        Path out = Files.createTempFile("zs2ob", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.GCConfiguration");
            r.enable("jdk.GCHeapConfiguration");
            r.start();
            System.gc();
            r.stop();
            r.dump(out);
        }
        List<RecordedEvent> events = RecordingFile.readAllEvents(out);
        TreeMap<String, String> seen = new TreeMap<>();
        for (RecordedEvent e : events) {
            String type = e.getEventType().getName();
            if (seen.containsKey(type)) {
                continue;
            }
            StringBuilder sb = new StringBuilder();
            if (type.equals("jdk.GCConfiguration")) {
                sb.append("young=").append(e.getString("youngCollector"))
                        .append(" old=").append(e.getString("oldCollector"))
                        .append(" dynamic=").append(e.getBoolean("usesDynamicGCThreads"))
                        .append(" explicit_concurrent=").append(e.getBoolean("isExplicitGCConcurrent"))
                        .append(" explicit_disabled=").append(e.getBoolean("isExplicitGCDisabled"))
                        .append(" gc_time_ratio=").append(e.getInt("gcTimeRatio"));
                if (threads) {
                    sb.append(" parallel=").append(e.getInt("parallelGCThreads"))
                            .append(" concurrent=").append(e.getInt("concurrentGCThreads"));
                }
                System.err.println("[probe] " + e);
            } else if (type.equals("jdk.GCHeapConfiguration")) {
                sb.append("min=").append(e.getLong("minSize"))
                        .append(" initial=").append(e.getLong("initialSize"))
                        .append(" max=").append(e.getLong("maxSize"))
                        .append(" compressed_oops=").append(e.getBoolean("usesCompressedOops"))
                        .append(" oops_mode=").append(e.getString("compressedOopsMode"))
                        .append(" alignment=").append(e.getLong("objectAlignment"))
                        .append(" address_bits=").append(e.getInt("heapAddressBits"));
                System.err.println("[probe] " + e);
            } else {
                continue;
            }
            seen.put(type, sb.toString());
        }
        for (var en : seen.entrySet()) {
            System.out.println(en.getKey() + " " + en.getValue());
        }
        System.out.println("types=" + seen.size());
        Files.deleteIfExists(out);
        System.out.println("done");
    }
}
