import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import jdk.jfr.Recording;
import jdk.jfr.consumer.RecordedEvent;
import jdk.jfr.consumer.RecordedThread;
import jdk.jfr.consumer.RecordingFile;

/**
 * gci i19/ob: two JFR GC rows of the {@code jdk.jfr} backend against HotSpot 25
 * (gci-i12ge-g1-event-model-review-findings-RETIRED-UNFIXED-20261018 residuals r1 and r3).
 *
 * <p>Modes (first argument):
 * <ul>
 *   <li>{@code sysgcthread}: a recording of {@code jdk.SystemGC}; two
 *       {@code System.gc()} on {@code main}, one on a platform thread named
 *       {@code gci-ob19-caller}. HotSpot's {@code JVM_GC} commits the event on
 *       the calling thread, so the sorted {@code eventThread} Java names are
 *       {@code [gci-ob19-caller, main, main]}.</li>
 *   <li>{@code cmfail}: a recording of {@code jdk.ConcurrentModeFailure}; a
 *       ~51 MB linked list (G1 marks it concurrently at -Xmx128m), then 40
 *       rounds of churn + {@code System.gc()}; a {@code System.gc()} that
 *       lands while a cycle marks aborts it. stdout says only that the run
 *       completed and the list is intact; the event count and ids go to
 *       stderr ({@code [gci-ob19] cmfail events=N ids=[...]}), to be matched
 *       against the {@code GC(c) Concurrent Mark Abort} lines of
 *       {@code -Xlog:gc+marking} on stderr.</li>
 * </ul>
 *
 * <p>stdout (deterministic):
 * <pre>
 * mode sysgcthread
 * sysgcthread SystemGC events 3
 * sysgcthread eventThreads [gci-ob19-caller, main, main]
 * DONE
 * </pre>
 * <pre>
 * mode cmfail
 * cmfail list intact true
 * DONE
 * </pre>
 */
public class GciOb19JfrGcThreadProbe {
    static volatile Object sink;

    static final int NODES = 1_600_000;
    static final int ROUNDS = 40;

    static final class Node {
        Node next;
        long a;
        long b;
    }

    static void sysgcThread() throws Exception {
        Path file = Files.createTempFile("gci-ob19s-", ".jfr");
        try (Recording r = new Recording()) {
            r.enable("jdk.SystemGC");
            r.start();
            System.gc();
            System.gc();
            Thread t = new Thread(new Runnable() {
                public void run() {
                    System.gc();
                }
            }, "gci-ob19-caller");
            t.start();
            t.join();
            r.stop();
            r.dump(file);
        }
        List<String> names = new ArrayList<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (e.getEventType().getName().equals("jdk.SystemGC")) {
                RecordedThread th = e.getThread();
                names.add(th == null ? "none" : String.valueOf(th.getJavaName()));
            }
        }
        Files.deleteIfExists(file);
        Collections.sort(names);
        System.out.println("sysgcthread SystemGC events " + names.size());
        System.out.println("sysgcthread eventThreads " + names);
    }

    static Node buildList() {
        Node head = null;
        for (int i = 0; i < NODES; i++) {
            Node n = new Node();
            n.next = head;
            n.a = i;
            head = n;
        }
        return head;
    }

    static boolean intact(Node head) {
        int n = 0;
        long expect = NODES - 1;
        for (Node at = head; at != null; at = at.next) {
            if (at.a != expect) {
                return false;
            }
            expect--;
            n++;
        }
        return n == NODES;
    }

    static void churn() {
        Object[] window = new Object[256];
        for (int i = 0; i < 32 * 1024; i++) {
            window[i % window.length] = new byte[256];
        }
        sink = window;
    }

    static void cmFail() throws Exception {
        Path file = Files.createTempFile("gci-ob19c-", ".jfr");
        Node head = null;
        try (Recording r = new Recording()) {
            r.enable("jdk.ConcurrentModeFailure");
            r.start();
            head = buildList();
            for (int round = 0; round < ROUNDS; round++) {
                churn();
                System.gc();
            }
            r.stop();
            r.dump(file);
        }
        List<Long> ids = new ArrayList<>();
        for (RecordedEvent e : RecordingFile.readAllEvents(file)) {
            if (e.getEventType().getName().equals("jdk.ConcurrentModeFailure")) {
                ids.add(e.getLong("gcId"));
            }
        }
        Files.deleteIfExists(file);
        Collections.sort(ids);
        System.out.println("cmfail list intact " + intact(head));
        System.err.println("[gci-ob19] cmfail events=" + ids.size() + " ids=" + ids);
        sink = head;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgcthread";
        System.out.println("mode " + mode);
        if (mode.equals("cmfail")) {
            cmFail();
        } else {
            sysgcThread();
        }
        System.out.println("DONE");
    }
}
