import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;
public class Sbr5JfrS {
    static long work(int n) { long x = 0; for (int i = 0; i < n; i++) x += i % 7; return x; }
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5s", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ExecutionSample").withPeriod(java.time.Duration.ofMillis(10));
            r.enable("jdk.NativeMethodSample").withPeriod(java.time.Duration.ofMillis(10));
            r.start();
            long end = System.nanoTime() + 1_500_000_000L; long s = 0;
            while (System.nanoTime() < end) s += work(100000);
            r.stop(); r.dump(f);
            System.out.println(s > 0);
        }
        for (Thread t : Thread.getAllStackTraces().keySet()) if (t.getName().contains("Sampler")) System.out.println("sampler thread present");
        int n = 0; Set<String> top = new TreeSet<>(); Set<String> types = new TreeSet<>();
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) { RecordedEvent e = rf.readEvent();
            String nm = e.getEventType().getName(); if (nm.endsWith("Sample")) { n++; RecordedStackTrace st = e.getStackTrace(); if (st != null && !st.getFrames().isEmpty()) { RecordedFrame fr = st.getFrames().get(0); top.add(fr.getMethod().getType().getName() + "." + fr.getMethod().getName()); types.add(fr.getType()); } } } }
        System.out.println("samples>0: " + (n > 10) + " " + top.contains("Sbr5JfrS.work") + " " + types);
        Files.delete(f);
    }
}
