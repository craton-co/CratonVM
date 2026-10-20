import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.concurrent.atomic.*;

/** SBR5 jfr: chunk rotation by size, multi-chunk dump, streaming across a rotation. */
public class Sbr5JfrH {
    @Name("probe.H") @StackTrace(false) static class H extends Event { int n; String s; }
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5h", ".jfr");
        String payload = "p".repeat(1000);
        AtomicInteger streamed = new AtomicInteger();
        try (Recording r = new Recording(); RecordingStream rs = new RecordingStream()) {
            rs.enable("probe.H").withoutStackTrace();
            rs.onEvent("probe.H", e -> streamed.incrementAndGet());
            rs.startAsync();
            r.enable("probe.H").withoutStackTrace(); r.setMaxSize(500_000_000L); r.setToDisk(true);
            r.start();
            for (int i = 0; i < 40_000; i++) { H h = new H(); h.n = i; h.s = payload + i; h.commit(); if (i % 10_000 == 9_999) Thread.sleep(300); }
            Thread.sleep(1500);
            r.stop(); r.dump(f);
        }
        int n = 0; long sum = 0;
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) { var e = rf.readEvent(); if (e.getEventType().getName().equals("probe.H")) { n++; sum += e.getInt("n"); } } }
        System.out.println("H count=" + n + " sum=" + sum + " streamed=" + streamed.get());
        Files.delete(f);
    }
}
