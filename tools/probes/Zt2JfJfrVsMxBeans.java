// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

import jdk.jfr.Configuration;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedObject;
import jdk.jfr.consumer.RecordingFile;

/**
 * zt t2/jf: do the JFR GC and code-cache rows describe what the MXBeans
 * describe? (wave t1/ob listed HotSpot's five non-heap pools on ZGC,
 * {@code CRATONVM_ZGC_JMX_HOTSPOT_NON_HEAP_POOLS}.)
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx256m Zt2JfJfrVsMxBeans
 *   cratonvm         -Xmx256m Zt2JfJfrVsMxBeans
 * </pre>
 *
 * <p>stdout (shape lines both VMs must agree on):
 * <ul>
 * <li>{@code code_heaps}: the {@code jdk.CodeCacheStatistics} rows' heap
 * names are exactly the {@code CodeHeap} memory pools' names, none null, and
 * each row's {@code unallocatedCapacity} is at most the pool's {@code max}
 * (or 0 where the pool has none).</li>
 * <li>{@code metaspace}: {@code jdk.MetaspaceSummary} is ordered as the
 * pools are ({@code used <= committed <= reserved}, a class space with
 * {@code used <= committed}), and the {@code Compressed Class Space} pool
 * exists exactly when the summary's class space is non-empty.</li>
 * <li>{@code heap}: {@code jdk.GCHeapSummary.heapSpace.reservedSize} is
 * {@code Runtime.maxMemory()} and {@code committedSize >= heapUsed}.</li>
 * </ul>
 * stderr: the figures.
 */
public class Zt2JfJfrVsMxBeans {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        Recording r = new Recording(Configuration.getConfiguration("default"));
        r.enable("jdk.CodeCacheStatistics").withPeriod(Duration.ofMillis(100));
        r.enable("jdk.MetaspaceSummary");
        r.enable("jdk.GCHeapSummary");
        r.start();
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            keep.add(new byte[64 * 1024]);
        }
        long acc = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 200_000; i++) {
                sink = new byte[128];
                acc += i % 7;
            }
            System.gc();
            Thread.sleep(150);
        }
        r.stop();
        Path p = Files.createTempFile("zt2jf", ".jfr");
        r.dump(p);
        r.close();
        List<RecordedEvent> events = RecordingFile.readAllEvents(p);
        Files.deleteIfExists(p);
        System.out.println("workload live=" + keep.size() + " acc_pos=" + (acc > 0));

        TreeSet<String> codePools = new TreeSet<>();
        boolean classPool = false;
        long codeMaxNonProfiled = -1;
        for (MemoryPoolMXBean b : ManagementFactory.getMemoryPoolMXBeans()) {
            if (b.getType() != MemoryType.NON_HEAP) {
                continue;
            }
            if (b.getName().startsWith("CodeHeap")) {
                codePools.add(b.getName());
                if (b.getName().equals("CodeHeap 'non-profiled nmethods'")) {
                    codeMaxNonProfiled = b.getUsage().getMax();
                }
            }
            classPool |= b.getName().equals("Compressed Class Space");
        }

        TreeSet<String> rowNames = new TreeSet<>();
        boolean nonNull = true, unallocatedOk = true, codeRows = false;
        boolean msOrdered = true, msClass = false, msRows = false;
        boolean reservedIsMax = true, committedGeUsed = true, heapRows = false;
        long maxMemory = Runtime.getRuntime().maxMemory();
        for (RecordedEvent e : events) {
            switch (e.getEventType().getName()) {
                case "jdk.CodeCacheStatistics" -> {
                    codeRows = true;
                    Object t = e.getValue("codeBlobType");
                    nonNull &= t != null;
                    String name = String.valueOf(t);
                    rowNames.add(name);
                    long un = e.getLong("unallocatedCapacity");
                    if (name.equals("CodeHeap 'non-profiled nmethods'")) {
                        unallocatedOk &= codeMaxNonProfiled < 0 ? un == 0 : un <= codeMaxNonProfiled;
                        System.err.println("info code non-profiled unallocated=" + un + " pool_max="
                                + codeMaxNonProfiled);
                    }
                }
                case "jdk.MetaspaceSummary" -> {
                    msRows = true;
                    RecordedObject m = e.getValue("metaspace");
                    RecordedObject c = e.getValue("classSpace");
                    msOrdered &= m.getLong("used") <= m.getLong("committed")
                            && m.getLong("committed") <= m.getLong("reserved")
                            && c.getLong("used") <= c.getLong("committed");
                    msClass |= c.getLong("reserved") > 0;
                }
                case "jdk.GCHeapSummary" -> {
                    heapRows = true;
                    RecordedObject vs = e.getValue("heapSpace");
                    reservedIsMax &= vs.getLong("reservedSize") == maxMemory;
                    committedGeUsed &= vs.getLong("committedSize") >= e.getLong("heapUsed");
                    if (!(vs.getLong("reservedSize") == maxMemory)) {
                        System.err.println("info heap reserved=" + vs.getLong("reservedSize") + " max=" + maxMemory);
                    }
                }
                default -> { }
            }
        }
        System.out.println("code_heaps rows=" + codeRows + " names_are_pools=" + rowNames.equals(codePools)
                + " non_null=" + nonNull + " unallocated_within_max=" + unallocatedOk);
        System.out.println("metaspace rows=" + msRows + " ordered=" + msOrdered
                + " class_space_matches_pool=" + (msClass == classPool));
        System.out.println("heap rows=" + heapRows + " reserved_is_max_memory=" + reservedIsMax
                + " committed_ge_used=" + committedGeUsed);
        System.err.println("info code pools=" + codePools + " rows=" + rowNames);
    }
}
