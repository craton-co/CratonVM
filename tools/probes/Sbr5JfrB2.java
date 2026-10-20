import jdk.jfr.*;
import java.nio.file.*;
public class Sbr5JfrB2 {
    public static void main(String[] a) throws Exception {
        try (Recording r = new Recording()) {
            r.enable("jdk.ThreadStart"); r.enable("jdk.ThreadEnd"); r.enable("jdk.ClassLoad").withThreshold(java.time.Duration.ZERO); r.enable("jdk.GarbageCollection"); r.enable("jdk.GCPhasePause").withThreshold(java.time.Duration.ZERO);
            r.enable("jdk.JVMInformation"); r.enable("jdk.CPULoad").withPeriod(java.time.Duration.ofMillis(200));
            r.start();
            Thread t = new Thread(() -> {}, "b-worker"); t.start(); t.join();
            Class.forName("java.util.concurrent.ConcurrentSkipListMap");
            System.gc(); Thread.sleep(500);
            r.stop(); r.dump(Path.of(a[0]));
        }
    }
}
