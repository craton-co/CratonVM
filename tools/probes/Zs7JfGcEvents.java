// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.time.Instant;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

import jdk.jfr.Configuration;
import jdk.jfr.Recording;
import jdk.jfr.ValueDescriptor;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedFrame;
import jdk.jfr.consumer.RecordedObject;
import jdk.jfr.consumer.RecordedStackTrace;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

/**
 * zs s7/jf: ZGC's JFR GC events, recorded programmatically
 * ({@code jdk.jfr.Recording} with the JDK's {@code default} settings, the
 * set {@code -XX:StartFlightRecording} uses) and read back with
 * {@code RecordingFile} in the same process.
 *
 * <pre>
 *   java -XX:+UseZGC -Xmx256m Zs7JfGcEvents sysgc
 *   cratonvm         -Xmx256m Zs7JfGcEvents sysgc
 *   java -XX:+UseZGC -Xmx64m  Zs7JfGcEvents stall
 * </pre>
 *
 * <p>stdout carries only the lines this VM claims parity on (deterministic on
 * HotSpot 25: collections are filtered to the ones the workload caused, so
 * HotSpot's timing-dependent {@code Warmup} / {@code Allocation Rate}
 * collections do not enter them). stderr carries {@code info} lines: exact
 * counts, phase names, and the known structural differences.
 *
 * <p>Modes:
 * <ul>
 * <li>{@code sysgc}: three {@code System.gc()} over a 16 MiB live set with a
 * few weak references. Per explicit collection: the collection, its heap
 * summaries, its pauses, its whole-heap event, its relocation set,
 * {@code jdk.SystemGC} (thread and stack), the reference statistics.</li>
 * <li>{@code stall}: at {@code -Xmx64m}, a 40 MiB live set and churn; prints
 * whether {@code jdk.ZAllocationStall} fired and its {@code type} / {@code size}
 * shape (the event's threshold is lowered to 0 for this mode).</li>
 * <li>{@code fields}: the field shape (name, type, unit annotation) of every
 * GC event type the {@code sysgc} workload writes, after running it.</li>
 * <li>{@code minor}: allocation churn; the young ({@code ZGC Minor})
 * collections' events (this VM: generational mode only).</li>
 * <li>{@code cycles} (zs s10/jf3): the {@code minor} workload; every
 * collection's pause rows lie inside its {@code jdk.GarbageCollection} and
 * sum to its {@code sumOfPauses} (this VM's concurrent cycles: under
 * {@code CRATONVM_ZGC_CONC_START}).</li>
 * </ul>
 */
public class Zs7JfGcEvents {
    static final String[] GC_TYPES = {
        "jdk.GarbageCollection",
        "jdk.GCHeapSummary",
        "jdk.GCPhasePause",
        "jdk.GCPhasePauseLevel1",
        "jdk.GCPhaseConcurrent",
        "jdk.GCReferenceStatistics",
        "jdk.MetaspaceSummary",
        "jdk.MetaspaceChunkFreeListSummary",
        "jdk.SystemGC",
        "jdk.ZOldGarbageCollection",
        "jdk.ZYoungGarbageCollection",
        "jdk.ZRelocationSet",
        "jdk.ZRelocationSetGroup",
        "jdk.ZAllocationStall",
        "jdk.OldGarbageCollection",
        "jdk.YoungGarbageCollection",
        "jdk.GCCPUTime",
    };

    /** HotSpot 25 ZGC's pause names (`ZStatPhasePause`). */
    static final Set<String> HOTSPOT_PAUSES = Set.of(
            "Pause Mark Start", "Pause Mark Start (Major)", "Pause Mark End", "Pause Relocate Start");

    static volatile Object sink;
    static final List<WeakReference<Object>> WEAK = new ArrayList<>();

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgc";
        Recording r = new Recording(Configuration.getConfiguration("default"));
        if (mode.equals("stall")) {
            r.enable("jdk.ZAllocationStall").withThreshold(Duration.ZERO);
        }
        r.start();
        Instant t0 = Instant.now();
        switch (mode) {
            case "sysgc", "fields" -> sysgc();
            case "stall" -> stall();
            case "minor", "cycles" -> minor();
            default -> throw new IllegalArgumentException(mode);
        }
        r.stop();
        Path p = Files.createTempFile("zs7jf", ".jfr");
        r.dump(p);
        r.close();
        List<RecordedEvent> events = RecordingFile.readAllEvents(p);
        Files.deleteIfExists(p);
        switch (mode) {
            case "sysgc" -> reportSysgc(events);
            case "fields" -> reportFields(events);
            case "stall" -> reportStall(events);
            case "minor" -> reportMinor(events);
            case "cycles" -> reportCycles(events);
            default -> { }
        }
        System.err.println("info elapsed_ms=" + Duration.between(t0, Instant.now()).toMillis());
    }

    // ------------------------------------------------------------------ workloads

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
            // Fresh weak references to dead objects just before each explicit
            // collection, so each one finds at least 64 (an allocation-driven
            // collection in between cannot take them first).
            for (int i = 0; i < 64; i++) {
                WEAK.add(new WeakReference<>(new Object()));
            }
            System.gc();
        }
        System.out.println("workload sysgc live=" + keep.size());
    }

    static void stall() {
        List<byte[]> keep = live(40);
        byte[][] window = new byte[2048][];
        for (int i = 0; i < 3_000_000; i++) {
            window[i & 2047] = new byte[1024 + (i & 4095)];
        }
        sink = window;
        System.out.println("workload stall live=" + keep.size());
    }

    static void minor() {
        List<byte[]> keep = live(16);
        byte[][] window = new byte[4096][];
        for (int i = 0; i < 4_000_000; i++) {
            window[i & 4095] = new byte[256 + (i & 1023)];
        }
        sink = window;
        System.out.println("workload minor live=" + keep.size());
    }

    /**
     * zs s8/jf2: per collection-event type, the set of thread names its rows
     * for `ids` (or, for the gcId-less relocation set, inside one of `gcs`)
     * are committed on: Java name if any, else OS name.
     */
    static String collectorThreads(List<RecordedEvent> events, Set<Integer> ids, List<RecordedEvent> gcs,
            String... types) {
        Map<String, TreeSet<String>> out = new TreeMap<>();
        for (RecordedEvent e : events) {
            String t = type(e);
            boolean wanted = false;
            for (String w : types) {
                wanted |= w.equals(t);
            }
            if (!wanted) {
                continue;
            }
            boolean mine;
            if (e.hasField("gcId")) {
                mine = ids.contains(e.getInt("gcId"));
            } else {
                mine = false;
                for (RecordedEvent g : gcs) {
                    mine |= start(e) >= start(g) - 1_000_000 && start(e) <= end(g) + 1_000_000;
                }
            }
            if (!mine) {
                continue;
            }
            RecordedThread th = e.getThread();
            String n = th == null ? "<none>"
                    : th.getJavaName() != null ? th.getJavaName() : String.valueOf(th.getOSName());
            out.computeIfAbsent(t.substring(4), k -> new TreeSet<>()).add(n);
        }
        return out.toString();
    }

    // ------------------------------------------------------------------ helpers

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

    static String str(RecordedObject e, String f) {
        return e.hasField(f) ? String.valueOf((Object) e.getValue(f)) : "<none>";
    }

    // ------------------------------------------------------------------ sysgc

    static void reportSysgc(List<RecordedEvent> events) {
        // The explicit collections: `GarbageCollection` with cause System.gc().
        Map<Integer, RecordedEvent> gcs = new TreeMap<>();
        for (RecordedEvent e : events) {
            if (type(e).equals("jdk.GarbageCollection") && "System.gc()".equals(str(e, "cause"))) {
                gcs.put(e.getInt("gcId"), e);
            }
        }
        System.out.println("sysgc_collections=" + gcs.size());
        Map<String, Map<Integer, List<RecordedEvent>>> byType = new HashMap<>();
        for (RecordedEvent e : events) {
            if (e.hasField("gcId")) {
                byType.computeIfAbsent(type(e), k -> new TreeMap<>())
                        .computeIfAbsent(e.getInt("gcId"), k -> new ArrayList<>()).add(e);
            }
        }
        // Per type: how many rows each explicit collection has (min..max).
        for (String t : GC_TYPES) {
            Map<Integer, List<RecordedEvent>> m = byType.getOrDefault(t, Map.of());
            int min = Integer.MAX_VALUE;
            int max = 0;
            for (int id : gcs.keySet()) {
                int n = m.getOrDefault(id, List.of()).size();
                min = Math.min(min, n);
                max = Math.max(max, n);
            }
            if (gcs.isEmpty()) {
                min = 0;
            }
            System.err.println("info per_sysgc " + t + " " + min + ".." + max);
        }

        // jdk.GarbageCollection.
        boolean names = true, sumLe = true, longestLe = true, longestPos = true;
        for (RecordedEvent g : gcs.values()) {
            names &= "ZGC Major".equals(str(g, "name"));
            long d = g.getDuration().toNanos();
            long sum = g.getDuration("sumOfPauses").toNanos();
            long longest = g.getDuration("longestPause").toNanos();
            sumLe &= sum <= d;
            longestLe &= longest <= sum;
            longestPos &= longest > 0;
        }
        System.out.println("gc name_zgc_major=" + names + " sum_of_pauses_le_duration=" + sumLe
                + " longest_le_sum=" + longestLe + " longest_gt_0=" + longestPos);

        // jdk.GCHeapSummary: Before GC and After GC per collection, inside it.
        boolean two = true, inside = true, used = true, space = true;
        TreeSet<String> whens = new TreeSet<>();
        for (Map.Entry<Integer, RecordedEvent> g : gcs.entrySet()) {
            List<RecordedEvent> l = byType.getOrDefault("jdk.GCHeapSummary", Map.of())
                    .getOrDefault(g.getKey(), List.of());
            two &= l.size() == 2;
            for (RecordedEvent h : l) {
                whens.add(str(h, "when"));
                inside &= start(h) >= start(g.getValue()) - 1_000_000 && start(h) <= end(g.getValue()) + 1_000_000;
                used &= h.getLong("heapUsed") > 0;
                RecordedObject vs = h.getValue("heapSpace");
                space &= vs != null && vs.hasField("committedSize")
                        && vs.getLong("committedSize") > 0
                        && vs.getLong("committedSize") <= vs.getLong("reservedSize");
            }
        }
        System.out.println("heap_summary two_per_gc=" + two + " when=" + whens + " inside_gc=" + inside
                + " used_gt_0=" + used + " committed_le_reserved=" + space);

        // jdk.GCPhasePause: HotSpot's pause names; mark end and relocate start
        // in every collection, each pause inside its collection, pauses sum to
        // `sumOfPauses` and their longest is `longestPause`.
        boolean hsNames = true, markEnd = true, relocStart = true, pauseInside = true, sumMatch = true,
                longestMatch = true;
        TreeSet<String> pauseNames = new TreeSet<>();
        for (Map.Entry<Integer, RecordedEvent> g : gcs.entrySet()) {
            List<RecordedEvent> l = new ArrayList<>(byType.getOrDefault("jdk.GCPhasePause", Map.of())
                    .getOrDefault(g.getKey(), List.of()));
            l.sort((a, b) -> Long.compare(start(a), start(b)));
            Set<String> here = new TreeSet<>();
            long sum = 0, longest = 0;
            for (RecordedEvent pz : l) {
                String n = str(pz, "name");
                here.add(n);
                pauseNames.add(n);
                hsNames &= HOTSPOT_PAUSES.contains(n);
                long d = pz.getDuration().toNanos();
                sum += d;
                longest = Math.max(longest, d);
                pauseInside &= start(pz) >= start(g.getValue()) - 1_000_000
                        && end(pz) <= end(g.getValue()) + 1_000_000;
            }
            markEnd &= here.contains("Pause Mark End");
            relocStart &= here.contains("Pause Relocate Start");
            long gsum = g.getValue().getDuration("sumOfPauses").toNanos();
            long glong = g.getValue().getDuration("longestPause").toNanos();
            // Tick rounding: allow 1 us per row.
            sumMatch &= Math.abs(gsum - sum) <= 1_000L * (l.size() + 1);
            longestMatch &= Math.abs(glong - longest) <= 1_000L;
        }
        System.out.println("pause hotspot_names=" + hsNames + " mark_end=" + markEnd + " relocate_start="
                + relocStart + " inside_gc=" + pauseInside);
        System.out.println("pause sum_is_sum_of_pauses=" + sumMatch + " longest_is_longest_pause=" + longestMatch);
        System.err.println("info pause_names " + pauseNames);

        // Whole-heap event: one jdk.ZOldGarbageCollection per explicit
        // collection, inside it; no Serial/G1 kind events.
        boolean oneOld = true, oldInside = true;
        for (Map.Entry<Integer, RecordedEvent> g : gcs.entrySet()) {
            List<RecordedEvent> l = byType.getOrDefault("jdk.ZOldGarbageCollection", Map.of())
                    .getOrDefault(g.getKey(), List.of());
            oneOld &= l.size() == 1;
            for (RecordedEvent o : l) {
                oldInside &= start(o) >= start(g.getValue()) - 1_000_000 && end(o) <= end(g.getValue()) + 1_000_000;
            }
        }
        boolean serial = byType.containsKey("jdk.OldGarbageCollection") || byType.containsKey("jdk.YoungGarbageCollection");
        System.out.println("z_old one_per_gc=" + oneOld + " inside_gc=" + oldInside + " serial_kind_events=" + serial);

        // jdk.ZYoungGarbageCollection: present on every explicit collection,
        // threshold in [0, 15].
        boolean young = !gcs.isEmpty(), threshold = true;
        for (int id : gcs.keySet()) {
            List<RecordedEvent> l = byType.getOrDefault("jdk.ZYoungGarbageCollection", Map.of())
                    .getOrDefault(id, List.of());
            young &= !l.isEmpty();
            for (RecordedEvent y : l) {
                int th = y.getInt("tenuringThreshold");
                threshold &= th >= 0 && th <= 15;
            }
        }
        // zt t1/jf: on stdout again (zs s10/jf3 had moved it to stderr).
        // HotSpot 25 ZGC is generational only and every major runs a young
        // collection; this VM's major writes that young event too
        // (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`; `=0` is the kill control:
        // `per_gc=false`).
        System.out.println("z_young per_gc=" + young + " threshold_in_range=" + threshold);

        // jdk.GCReferenceStatistics: the four reference types per explicit
        // collection, and the workload's cleared weak references counted.
        boolean four = true, weakSeen = true;
        TreeSet<String> refTypes = new TreeSet<>();
        for (int id : gcs.keySet()) {
            List<RecordedEvent> l = byType.getOrDefault("jdk.GCReferenceStatistics", Map.of())
                    .getOrDefault(id, List.of());
            Set<String> here = new TreeSet<>();
            for (RecordedEvent rs : l) {
                here.add(str(rs, "type"));
                refTypes.add(str(rs, "type"));
            }
            four &= here.size() == 4;
        }
        weakSeen = !gcs.isEmpty();
        for (int id : gcs.keySet()) {
            long weak = -1;
            for (RecordedEvent rs : byType.getOrDefault("jdk.GCReferenceStatistics", Map.of())
                    .getOrDefault(id, List.of())) {
                if ("Weak reference".equals(str(rs, "type"))) {
                    weak = Math.max(weak, rs.getLong("count"));
                }
            }
            weakSeen &= weak >= 64;
            System.err.println("info weak_count gc=" + id + " " + weak);
        }
        System.out.println("ref_stats four_types_per_gc=" + four + " types=" + refTypes
                + " weak_count_ge_64_each=" + weakSeen);

        // jdk.ZRelocationSet: at least one per explicit collection, with a
        // relocation set no larger than its total.
        boolean rset = true, rsetSane = true;
        // ZRelocationSet has no gcId: attribute it by time.
        for (RecordedEvent g : gcs.values()) {
            int n = 0;
            for (RecordedEvent e : events) {
                if (type(e).equals("jdk.ZRelocationSet") && start(e) >= start(g) - 1_000_000
                        && start(e) <= end(g) + 1_000_000) {
                    n++;
                    rsetSane &= e.getLong("relocate") <= e.getLong("total") && e.getLong("empty") <= e.getLong("total");
                }
            }
            rset &= n >= 1;
        }
        System.out.println("relocation_set per_gc=" + rset + " sane=" + rsetSane);

        // jdk.SystemGC: one per System.gc() call, on the calling thread, with
        // the call in its stack, around its collection.
        int sys = 0;
        boolean onMain = true, stack = true, around = true, notConcurrent = true;
        TreeSet<String> topFrames = new TreeSet<>();
        List<RecordedEvent> gcList = new ArrayList<>(gcs.values());
        for (RecordedEvent e : events) {
            if (!type(e).equals("jdk.SystemGC")) {
                continue;
            }
            sys++;
            RecordedThread th = e.getThread();
            onMain &= th != null && "main".equals(th.getJavaName());
            RecordedStackTrace st = e.getStackTrace();
            boolean has = false;
            if (st != null && !st.getFrames().isEmpty()) {
                // zs s8/jf2: HotSpot's top frame is `java.lang.Runtime.gc` (the
                // native `JVM_GC` commits from).
                RecordedFrame top = st.getFrames().get(0);
                if (top.getMethod() != null) {
                    topFrames.add(top.getMethod().getType().getName() + "." + top.getMethod().getName());
                }
            }
            if (st != null) {
                for (RecordedFrame f : st.getFrames()) {
                    if (f.getMethod() != null && f.getMethod().getName().equals("gc")
                            && f.getMethod().getType().getName().equals("java.lang.System")) {
                        has = true;
                    }
                }
            }
            stack &= has;
            notConcurrent &= !e.getBoolean("invokedConcurrent");
            boolean covers = false;
            for (RecordedEvent g : gcList) {
                covers |= start(e) <= start(g) + 1_000_000 && end(e) >= end(g) - 1_000_000;
            }
            around &= covers;
        }
        System.out.println("system_gc count=" + sys + " on_main=" + onMain + " stack_has_system_gc=" + stack
                + " covers_its_gc=" + around + " invoked_concurrent=" + !notConcurrent);

        // zs s8/jf2: the thread each collection event is committed on.
        System.out.println("system_gc top_frame=" + topFrames);

        // zs s10/jf3: four all-zero chunk free-list rows per explicit
        // collection (Before/After GC x Metadata/Class).
        boolean fourChunks = !gcs.isEmpty(), zeros = true;
        TreeSet<String> chunkKinds = new TreeSet<>();
        for (int id : gcs.keySet()) {
            List<RecordedEvent> l = byType.getOrDefault("jdk.MetaspaceChunkFreeListSummary", Map.of())
                    .getOrDefault(id, List.of());
            fourChunks &= l.size() == 4;
            for (RecordedEvent m : l) {
                chunkKinds.add(str(m, "when") + "/" + str(m, "metadataType"));
                for (ValueDescriptor d : m.getFields()) {
                    if (d.getName().endsWith("Chunks") || d.getName().endsWith("TotalSize")) {
                        zeros &= m.getLong(d.getName()) == 0;
                    }
                }
            }
        }
        System.out.println("metaspace_chunks four_per_gc=" + fourChunks + " all_zero=" + zeros + " kinds=" + chunkKinds);
        System.out.println("gc_threads " + collectorThreads(events, gcs.keySet(), gcList,
                "jdk.GarbageCollection", "jdk.GCPhasePause", "jdk.ZOldGarbageCollection", "jdk.ZRelocationSet"));

        // Event types this VM's ZGC does not write (or writes in another shape);
        // informational only.
        for (String t : GC_TYPES) {
            System.err.println("info present " + t + "=" + events.stream().anyMatch(e -> type(e).equals(t)));
        }
        for (String t : new String[] {"jdk.GCPhaseConcurrent", "jdk.GCPhaseConcurrentLevel1"}) {
            TreeSet<String> n = new TreeSet<>();
            for (RecordedEvent e : events) {
                if (type(e).equals(t)) {
                    n.add(str(e, "name"));
                }
            }
            System.err.println("info names " + t + " " + n);
        }
    }

    // ------------------------------------------------------------------ fields

    static void reportFields(List<RecordedEvent> events) {
        Map<String, String> shape = new TreeMap<>();
        for (RecordedEvent e : events) {
            String t = type(e);
            boolean gc = false;
            for (String g : GC_TYPES) {
                gc |= g.equals(t);
            }
            if (!gc || shape.containsKey(t)) {
                continue;
            }
            StringBuilder sb = new StringBuilder();
            for (ValueDescriptor d : e.getEventType().getFields()) {
                if (sb.length() > 0) {
                    sb.append(',');
                }
                sb.append(d.getName()).append(':').append(d.getTypeName());
                for (jdk.jfr.AnnotationElement a : d.getAnnotationElements()) {
                    String an = a.getTypeName();
                    if (an.equals("jdk.jfr.Label") || an.equals("jdk.jfr.Description")) {
                        continue;
                    }
                    sb.append('@').append(an.substring(an.lastIndexOf('.') + 1));
                    Object v = a.getValues().isEmpty() ? null : a.getValues().get(0);
                    if (v instanceof String sv) {
                        sb.append('(').append(sv).append(')');
                    }
                }
            }
            shape.put(t, sb.toString());
        }
        for (Map.Entry<String, String> s : shape.entrySet()) {
            // Types this VM's default ZGC does not write on this workload
            // (structural: no concurrent phase) go to stderr, so the row
            // compares the rest. zt t1/jf: a major's young collection is
            // written now (`CRATONVM_ZGC_JFR_MAJOR_YOUNG`).
            boolean structural = s.getKey().equals("jdk.GCPhaseConcurrent");
            (structural ? System.err : System.out).println(
                    (structural ? "info " : "") + "fields " + s.getKey() + " " + s.getValue());
        }
    }

    // ------------------------------------------------------------------ minor

    /**
     * Young collections ({@code ZGC Minor}): each has one
     * {@code jdk.ZYoungGarbageCollection} (threshold in range) and no
     * {@code jdk.ZOldGarbageCollection}; its pauses are HotSpot's young ones.
     */
    static void reportMinor(List<RecordedEvent> events) {
        Map<Integer, RecordedEvent> minors = new TreeMap<>();
        for (RecordedEvent e : events) {
            if (type(e).equals("jdk.GarbageCollection") && "ZGC Minor".equals(str(e, "name"))) {
                minors.put(e.getInt("gcId"), e);
            }
        }
        Map<Integer, Integer> young = new HashMap<>();
        Map<Integer, Integer> old = new HashMap<>();
        Map<Integer, Set<String>> pauses = new HashMap<>();
        boolean threshold = true;
        for (RecordedEvent e : events) {
            if (!e.hasField("gcId") || !minors.containsKey(e.getInt("gcId"))) {
                continue;
            }
            int id = e.getInt("gcId");
            switch (type(e)) {
                case "jdk.ZYoungGarbageCollection" -> {
                    young.merge(id, 1, Integer::sum);
                    int th = e.getInt("tenuringThreshold");
                    threshold &= th >= 0 && th <= 15;
                }
                case "jdk.ZOldGarbageCollection" -> old.merge(id, 1, Integer::sum);
                case "jdk.GCPhasePause" -> pauses.computeIfAbsent(id, k -> new TreeSet<>()).add(str(e, "name"));
                default -> { }
            }
        }
        boolean oneYoung = true, noOld = true, hsPauses = true;
        for (int id : minors.keySet()) {
            oneYoung &= young.getOrDefault(id, 0) == 1;
            noOld &= old.getOrDefault(id, 0) == 0;
            for (String n : pauses.getOrDefault(id, Set.of())) {
                hsPauses &= n.equals("Pause Mark Start") || n.equals("Pause Mark End")
                        || n.equals("Pause Relocate Start");
            }
        }
        System.out.println("minor present=" + !minors.isEmpty() + " one_young_each=" + (!minors.isEmpty() && oneYoung)
                + " no_old=" + noOld + " threshold_in_range=" + threshold + " young_pause_names=" + hsPauses);
        // zs s8/jf2: minors' threads, and no reference statistics on a minor.
        boolean refOnMinor = false;
        for (RecordedEvent e : events) {
            refOnMinor |= type(e).equals("jdk.GCReferenceStatistics") && minors.containsKey(e.getInt("gcId"));
        }
        System.out.println("minor_threads " + collectorThreads(events, minors.keySet(), new ArrayList<>(minors.values()),
                "jdk.GarbageCollection", "jdk.GCPhasePause", "jdk.ZYoungGarbageCollection")
                + " ref_stats_on_minors=" + refOnMinor);
        System.err.println("info minors=" + minors.size());
    }

    // ------------------------------------------------------------------ cycles

    /**
     * zs s10/jf3: every collection of an allocation-driven run (HotSpot's
     * cycles are concurrent; this VM's under {@code CRATONVM_ZGC_CONC_START}):
     * each pause row of a collection lies inside its
     * {@code jdk.GarbageCollection}, the rows sum to {@code sumOfPauses},
     * their longest is {@code longestPause}, and every cycle that has a mark
     * start names it as HotSpot does.
     */
    static void reportCycles(List<RecordedEvent> events) {
        Map<Integer, RecordedEvent> gcs = new TreeMap<>();
        Map<Integer, List<RecordedEvent>> pauses = new HashMap<>();
        for (RecordedEvent e : events) {
            if (type(e).equals("jdk.GarbageCollection")) {
                gcs.put(e.getInt("gcId"), e);
            } else if (type(e).equals("jdk.GCPhasePause")) {
                pauses.computeIfAbsent(e.getInt("gcId"), k -> new ArrayList<>()).add(e);
            }
        }
        boolean inside = true, sum = true, longest = true, names = true, orphans = false;
        for (Map.Entry<Integer, List<RecordedEvent>> p : pauses.entrySet()) {
            RecordedEvent g = gcs.get(p.getKey());
            if (g == null) {
                orphans = true;
                continue;
            }
            long s = 0, l = 0;
            for (RecordedEvent pz : p.getValue()) {
                inside &= start(pz) >= start(g) - 1_000_000 && end(pz) <= end(g) + 1_000_000;
                long d = pz.getDuration().toNanos();
                s += d;
                l = Math.max(l, d);
                String n = str(pz, "name");
                names &= HOTSPOT_PAUSES.contains(n);
            }
            sum &= Math.abs(g.getDuration("sumOfPauses").toNanos() - s) <= 1_000L * (p.getValue().size() + 1);
            longest &= Math.abs(g.getDuration("longestPause").toNanos() - l) <= 1_000L;
        }
        System.out.println("cycles present=" + !gcs.isEmpty() + " pauses_inside_gc=" + inside
                + " sum_is_sum_of_pauses=" + sum + " longest_is_longest_pause=" + longest
                + " hotspot_pause_names=" + names + " orphan_pauses=" + orphans);
        System.err.println("info cycles=" + gcs.size());
    }

    // ------------------------------------------------------------------ stall

    static void reportStall(List<RecordedEvent> events) {
        int n = 0;
        boolean typed = true, sized = true, onMain = false, sizeMatchesType = true, stallFrame = false;
        Map<String, Integer> types = new LinkedHashMap<>();
        TreeSet<String> typeSet = new TreeSet<>();
        for (RecordedEvent e : events) {
            if (!type(e).equals("jdk.ZAllocationStall")) {
                continue;
            }
            n++;
            String ty = str(e, "type");
            typeSet.add(ty);
            types.merge(ty, 1, Integer::sum);
            typed &= ty.equals("Small") || ty.equals("Medium") || ty.equals("Large");
            long size = e.getLong("size");
            sized &= size > 0;
            // Small pages are 2 MiB; Large pages a multiple of the 2 MiB granule.
            if (ty.equals("Small")) {
                sizeMatchesType &= size == 2L * 1024 * 1024;
            } else if (ty.equals("Large") || ty.equals("Medium")) {
                sizeMatchesType &= size % (2L * 1024 * 1024) == 0;
            }
            RecordedThread th = e.getThread();
            onMain |= th != null && "main".equals(th.getJavaName());
            // zs s8/jf2: the stalling thread's stack names the allocating frame.
            RecordedStackTrace st = e.getStackTrace();
            if (st != null) {
                for (RecordedFrame f : st.getFrames()) {
                    stallFrame |= f.getMethod() != null && f.getMethod().getName().equals("stall")
                            && f.getMethod().getType().getName().equals("Zs7JfGcEvents");
                }
            }
        }
        System.out.println("stall_stack has_allocating_frame=" + stallFrame);
        System.out.println("stall present=" + (n > 0) + " type_is_page_type=" + (n > 0 && typed) + " size_gt_0="
                + (n > 0 && sized) + " size_matches_type=" + (n > 0 && sizeMatchesType) + " some_on_main="
                + onMain);
        System.err.println("info stall count=" + n + " types=" + types);
    }
}
