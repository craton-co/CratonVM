// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedObject;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i43/tj ({@code gci-i42mx-jfr-gc-configuration-events-are-not-hotspots-20261016}
 * items 5-8): the flag-valued periodic GC configuration events of one
 * recording against HotSpot 25.
 *
 * <p>One recording (period {@code everyChunk}) of
 * {@code jdk.YoungGenerationConfiguration}, {@code jdk.GCSurvivorConfiguration},
 * {@code jdk.GCTLABConfiguration}, {@code jdk.GCConfiguration} (its
 * {@code gcTimeRatio} only) and the flag events (the {@code origin} of a fixed
 * list of flags) around one {@code System.gc()}. Every distinct line printed
 * once, sorted.
 *
 * <p>stdout (deterministic):
 * <pre>
 * tj43 young min=N max=N ratio=N
 * tj43 survivor max=N initial=N
 * tj43 tlab uses=B min=N refill=N
 * tj43 gcTimeRatio=N
 * tj43 origin &lt;Flag&gt;=&lt;origin&gt;[,..]     (one per flag of ORIGINS, "none" if not written)
 * tj43 events young=B survivor=B tlab=B config=B
 * </pre>
 */
public class GciTj43JfrFlagConfigProbe {
    static final String[] ORIGINS = {
        "CICompilerCount", "InitialHeapSize", "MaxHeapSize", "MaxJavaStackTraceDepth", "ParallelGCThreads",
        "UseCompressedOops", "UseG1GC", "UseSerialGC", "UseTLAB",
    };

    static String str(RecordedEvent e, String field) {
        // Through Object: `String.valueOf` of a generic `getValue` would bind
        // the `char[]` overload.
        Object raw = e.getValue(field);
        return String.valueOf(raw);
    }

    /** The flag row's origin as text: the struct's `description`, or the value itself. */
    static String origin(RecordedEvent e) {
        Object raw = e.getValue("origin");
        if (raw instanceof RecordedObject o && o.hasField("description")) {
            return o.getString("description");
        }
        return String.valueOf(raw);
    }

    public static void main(String[] args) throws Exception {
        Path file = Files.createTempFile("tj43-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.YoungGenerationConfiguration").with("period", "everyChunk");
            r.enable("jdk.GCSurvivorConfiguration").with("period", "everyChunk");
            r.enable("jdk.GCTLABConfiguration").with("period", "everyChunk");
            r.enable("jdk.GCConfiguration").with("period", "everyChunk");
            r.enable("jdk.BooleanFlag").with("period", "everyChunk");
            r.enable("jdk.UnsignedLongFlag").with("period", "everyChunk");
            r.enable("jdk.UnsignedIntFlag").with("period", "everyChunk");
            r.enable("jdk.LongFlag").with("period", "everyChunk");
            r.enable("jdk.IntFlag").with("period", "everyChunk");
            r.start();
            System.gc();
            r.stop();
            r.dump(file);
        }
        TreeSet<String> young = new TreeSet<>();
        TreeSet<String> survivor = new TreeSet<>();
        TreeSet<String> tlab = new TreeSet<>();
        TreeSet<String> ratio = new TreeSet<>();
        Map<String, TreeSet<String>> origins = new TreeMap<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            String name = e.getEventType().getName();
            switch (name) {
                case "jdk.YoungGenerationConfiguration" -> young.add("tj43 young min=" + str(e, "minSize")
                        + " max=" + str(e, "maxSize") + " ratio=" + str(e, "newRatio"));
                case "jdk.GCSurvivorConfiguration" -> survivor.add("tj43 survivor max="
                        + str(e, "maxTenuringThreshold") + " initial=" + str(e, "initialTenuringThreshold"));
                case "jdk.GCTLABConfiguration" -> tlab.add("tj43 tlab uses=" + str(e, "usesTLABs")
                        + " min=" + str(e, "minTLABSize") + " refill=" + str(e, "tlabRefillWasteLimit"));
                case "jdk.GCConfiguration" -> ratio.add("tj43 gcTimeRatio=" + str(e, "gcTimeRatio"));
                case "jdk.BooleanFlag", "jdk.UnsignedLongFlag", "jdk.UnsignedIntFlag", "jdk.LongFlag",
                        "jdk.IntFlag" -> {
                    String flag = str(e, "name");
                    for (String wanted : ORIGINS) {
                        if (wanted.equals(flag)) {
                            origins.computeIfAbsent(flag, k -> new TreeSet<>()).add(origin(e));
                        }
                    }
                }
                default -> {
                }
            }
        }
        Files.deleteIfExists(file);
        young.forEach(System.out::println);
        survivor.forEach(System.out::println);
        tlab.forEach(System.out::println);
        ratio.forEach(System.out::println);
        for (String wanted : ORIGINS) {
            TreeSet<String> values = origins.get(wanted);
            System.out.println("tj43 origin " + wanted + "=" + (values == null ? "none" : String.join(",", values)));
        }
        System.out.println("tj43 events young=" + !young.isEmpty() + " survivor=" + !survivor.isEmpty()
                + " tlab=" + !tlab.isEmpty() + " config=" + !ratio.isEmpty());
    }
}
