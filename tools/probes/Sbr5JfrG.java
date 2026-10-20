import jdk.jfr.*;
import jdk.jfr.consumer.*;
import java.nio.file.*;
import java.util.concurrent.atomic.*;

/** SBR5 jfr: instrumentation arrives while the call site is hot (JIT-compiled). */
public class Sbr5JfrG {
    @Name("probe.G") @StackTrace(false) static class G extends Event { int n; }
    static volatile boolean stop;
    static final AtomicLong committed = new AtomicLong();
    static void work() {
        long i = 0;
        while (!stop) { G g = new G(); i++; if (g.shouldCommit() && (i & 0xFFFF) == 0) { g.n = (int) i; g.commit(); } }
    }
    public static void main(String[] a) throws Exception {
        Thread t = new Thread(Sbr5JfrG::work); t.start();
        Thread.sleep(3000);   // hot, compiled, no recording
        Path f = Files.createTempFile("sbr5g", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("probe.G"); r.start();
            Thread.sleep(1000);
            r.stop(); r.dump(f);
        }
        stop = true; t.join();
        long n = 0;
        try (RecordingFile rf = new RecordingFile(f)) { while (rf.hasMoreEvents()) if (rf.readEvent().getEventType().getName().equals("probe.G")) n++; }
        System.out.println("events recorded while hot >0: " + (n > 0));
        Files.delete(f);
    }
}
