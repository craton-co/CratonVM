import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.*;

/** SBR5 jfr: volume, large payloads, buffer growth. */
public class Sbr5JfrF {
    @Name("probe.F") @StackTrace(false) static class F extends Event { int n; String s; }
    @Name("probe.FS") @StackTrace(true) static class FS extends Event { int n; }
    static void p(String s) { System.out.println(s); }
    public static void main(String[] a) throws Exception {
        Path f = Files.createTempFile("sbr5f", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("probe.F"); r.enable("probe.FS"); r.setToDisk(true);
            r.start();
            for (int i = 0; i < 100_000; i++) { F e = new F(); e.n = i; e.s = (i % 1000 == 0) ? "x".repeat(40_000) : "v" + i; e.commit(); }
            F big = new F(); big.n = -1; big.s = "y".repeat(120_000); big.commit();
            for (int i = 0; i < 3000; i++) { FS e = new FS(); e.n = i; e.commit(); }
            r.stop(); r.dump(f);
        }
        int n = 0, big = 0, fs = 0; long sum = 0; boolean allStacks = true;
        try (RecordingFile rf = new RecordingFile(f)) {
            while (rf.hasMoreEvents()) {
                RecordedEvent e = rf.readEvent();
                if (e.getEventType().getName().equals("probe.F")) { n++; sum += e.getInt("n"); if (e.getString("s").length() >= 40_000) big++; }
                if (e.getEventType().getName().equals("probe.FS")) { fs++; if (e.getStackTrace() == null || e.getStackTrace().getFrames().isEmpty()) allStacks = false; }
            }
        }
        p("F count=" + n + " sum=" + sum + " big=" + big + " FS=" + fs + " allStacks=" + allStacks);
        Files.delete(f);
    }
}
