import java.nio.file.Files;
import java.nio.file.Path;
import java.time.Duration;
import java.util.TreeMap;
import java.util.TreeSet;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i20/ob: {@code jdk.ExecuteVMOperation.caller} of a collection against
 * HotSpot 25 (gci-i19ob-jfr-gc-row-field-residuals-20261014 item 1). HotSpot's
 * {@code VMThread} posts the event with {@code caller} = the VM operation's
 * calling thread.
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code sysgc}: two {@code System.gc()} on {@code main}, one on a
 *       platform thread named {@code gci-ob20-caller}. The DISTINCT caller
 *       names of the full-collection operations ({@code G1CollectFull},
 *       {@code SerialGCCollect}) are {@code [gci-ob20-caller, main]} (a
 *       distinct set, so a collector that runs several pauses per request
 *       still compares).</li>
 *   <li>{@code alloc}: a platform thread named {@code gci-ob20-alloc}
 *       allocates ~512 MB of short-lived arrays while {@code main} waits; the
 *       allocation operations ({@code G1CollectForAllocation},
 *       {@code SerialCollectForAllocation}) name that thread.</li>
 * </ul>
 * Diagnostics (every operation name with its callers) go to stderr as
 * {@code [gci-ob20] ...}.
 *
 * <p>stdout (deterministic):
 * <pre>
 * mode sysgc
 * sysgc full-collection ops present true
 * sysgc callers [gci-ob20-caller, main]
 * DONE
 * </pre>
 * <pre>
 * mode alloc
 * alloc allocation ops present true
 * alloc callers include gci-ob20-alloc true
 * DONE
 * </pre>
 */
public class GciOb20VmOpCallerProbe {
    static volatile Object sink;

    static boolean isFull(String op) {
        return op.equals("G1CollectFull") || op.equals("SerialGCCollect") || op.equals("GenCollectFull");
    }

    static boolean isAlloc(String op) {
        return op.equals("G1CollectForAllocation") || op.equals("SerialCollectForAllocation")
            || op.equals("GenCollectForAllocation");
    }

    static TreeMap<String, TreeSet<String>> callersByOp(Path file) throws Exception {
        TreeMap<String, TreeSet<String>> byOp = new TreeMap<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (!e.getEventType().getName().equals("jdk.ExecuteVMOperation")) {
                continue;
            }
            String op = String.valueOf(e.getString("operation"));
            RecordedThread th = e.getThread("caller");
            String who = th == null ? "none" : String.valueOf(th.getJavaName());
            byOp.computeIfAbsent(op, k -> new TreeSet<>()).add(who);
        }
        return byOp;
    }

    static void sysgc() throws Exception {
        Path file = Files.createTempFile("gci-ob20s-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ExecuteVMOperation").withThreshold(Duration.ZERO);
            r.start();
            System.gc();
            System.gc();
            Thread t = new Thread(new Runnable() {
                public void run() {
                    System.gc();
                }
            }, "gci-ob20-caller");
            t.start();
            t.join();
            r.stop();
            r.dump(file);
        }
        TreeMap<String, TreeSet<String>> byOp = callersByOp(file);
        Files.deleteIfExists(file);
        TreeSet<String> callers = new TreeSet<>();
        boolean present = false;
        for (String op : byOp.keySet()) {
            if (isFull(op)) {
                present = true;
                callers.addAll(byOp.get(op));
            }
        }
        System.out.println("sysgc full-collection ops present " + present);
        System.out.println("sysgc callers " + callers);
        System.err.println("[gci-ob20] sysgc ops=" + byOp);
    }

    static void alloc() throws Exception {
        Path file = Files.createTempFile("gci-ob20a-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.ExecuteVMOperation").withThreshold(Duration.ZERO);
            r.start();
            Thread t = new Thread(new Runnable() {
                public void run() {
                    Object[] window = new Object[512];
                    for (int i = 0; i < 512 * 1024; i++) {
                        window[i % window.length] = new byte[1024];
                    }
                    sink = window;
                }
            }, "gci-ob20-alloc");
            t.start();
            t.join();
            r.stop();
            r.dump(file);
        }
        TreeMap<String, TreeSet<String>> byOp = callersByOp(file);
        Files.deleteIfExists(file);
        TreeSet<String> callers = new TreeSet<>();
        boolean present = false;
        for (String op : byOp.keySet()) {
            if (isAlloc(op)) {
                present = true;
                callers.addAll(byOp.get(op));
            }
        }
        System.out.println("alloc allocation ops present " + present);
        System.out.println("alloc callers include gci-ob20-alloc " + callers.contains("gci-ob20-alloc"));
        System.err.println("[gci-ob20] alloc ops=" + byOp);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgc";
        System.out.println("mode " + mode);
        if (mode.equals("alloc")) {
            alloc();
        } else {
            sysgc();
        }
        System.out.println("DONE");
    }
}
