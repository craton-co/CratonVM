import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;

/** SBR5 jfr milestone B: JDK built-in events written by the VM. */
public class Sbr5JfrB {
    static void p(String s) { System.out.println(s); }
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5b", ".jfr");
        List<String> names = List.of("jdk.JVMInformation", "jdk.OSInformation", "jdk.InitialSystemProperty", "jdk.CPUInformation",
            "jdk.PhysicalMemory", "jdk.JavaThreadStatistics", "jdk.ClassLoadingStatistics", "jdk.ThreadStart", "jdk.ThreadEnd",
            "jdk.GarbageCollection", "jdk.CPULoad", "jdk.GCPhasePause", "jdk.ClassLoad", "jdk.ThreadAllocationStatistics");
        try (Recording r = new Recording()) {
            for (String n : names) {
                var s = r.enable(n);
                if (n.equals("jdk.CPULoad") || n.equals("jdk.ThreadAllocationStatistics")) s.withPeriod(java.time.Duration.ofMillis(200));
                if (n.equals("jdk.ClassLoad") || n.equals("jdk.GarbageCollection")) s.withThreshold(java.time.Duration.ZERO);
            }
            r.start();
            Thread t = new Thread(() -> { try { Thread.sleep(10); } catch (Exception e) {} }, "b-worker"); t.start(); t.join();
            Class.forName("java.util.concurrent.ConcurrentSkipListMap");
            System.gc();
            Thread.sleep(700);
            r.stop();
            r.dump(f);
        }
        Map<String, Integer> counts = new TreeMap<>();
        List<String> detail = new ArrayList<>();
        try (RecordingFile rf = new RecordingFile(f)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                String n = e.getEventType().getName();
                if (!names.contains(n)) continue;
                counts.merge(n, 1, Integer::sum);
                switch (n) {
                    case "jdk.JVMInformation" -> detail.add(n + " pid=" + (e.getLong("pid") == ProcessHandle.current().pid()) + " name=" + e.getString("jvmName") + " startTime>0=" + (e.getLong("jvmStartTime") > 0) + " args=" + (e.getString("javaArguments") != null));
                    case "jdk.OSInformation" -> detail.add(n + " osVersion.nonempty=" + !e.getString("osVersion").isEmpty());
                    case "jdk.InitialSystemProperty" -> { if (e.getString("key").equals("java.version")) detail.add(n + " java.version=" + e.getString("value")); }
                    case "jdk.CPUInformation" -> detail.add(n + " cores>0=" + (e.getInt("cores") > 0) + " hwThreads>0=" + (e.getInt("hwThreads") > 0));
                    case "jdk.PhysicalMemory" -> detail.add(n + " total>0=" + (e.getLong("totalSize") > 0));
                    case "jdk.ThreadStart" -> { if ("b-worker".equals(e.getThread("thread").getJavaName())) detail.add(n + " b-worker parent=" + e.getThread("parentThread").getJavaName()); }
                    case "jdk.ThreadEnd" -> { if ("b-worker".equals(e.getThread("thread").getJavaName())) detail.add(n + " b-worker"); }
                    case "jdk.ClassLoad" -> { if (e.getClass("loadedClass").getName().equals("java.util.concurrent.ConcurrentSkipListMap")) detail.add(n + " CSLM"); }
                    case "jdk.GarbageCollection" -> detail.add(n + " cause=" + e.getString("cause") + " gcId>=0=" + (e.getInt("gcId") >= 0));
                    default -> {}
                }
            }
        }
        Files.delete(f);
        counts.forEach((k, v) -> p(k + " " + (v > 0 ? ">0" : "0") + (k.equals("jdk.CPULoad") ? " multi=" + (v >= 2) : "")));
        Collections.sort(detail);
        for (String d : new LinkedHashSet<>(detail)) p(d);
    }
}
