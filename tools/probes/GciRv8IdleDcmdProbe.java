import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i8/rv: {@code jcmd GC.class_histogram} (live objects, no {@code -all})
 * on a target whose only Java thread is BLOCKED (waiting for jcmd), the
 * shape of an idle server. {@code CRATONVM_DCMD_LIVE_HEAP_WALKS} went
 * default ON on 2026-10-13 on vi5-hd Q, whose rows all had a trickle thread
 * allocating (the request is taken by the next allocating mutator).
 *
 * <p>Modes: {@code idle} (no other thread), {@code busy} (a trickle thread
 * allocates during the walk, as vi5-hd Q). HotSpot collects first inside its
 * own VM operation, so on HotSpot both modes print the same lines: the walk
 * is prompt, the histogram holds exactly the live {@code Held8} objects, the
 * collection during the walk has the cause {@code Heap Inspection Initiated GC},
 * and no collection follows the walk.
 *
 * <p>stdout (deterministic):
 * <pre>
 * &lt;mode&gt; live histogram rc 0
 * &lt;mode&gt; live histogram held exact true
 * &lt;mode&gt; walk prompt true
 * &lt;mode&gt; causes during walk [Heap Inspection Initiated GC]
 * &lt;mode&gt; extra collections after walk 0
 * kept 5000 first 0 last 4999
 * </pre>
 * stderr: {@code [gci-rv8] ...} with the round trip, the counts and the causes.
 * jcmd: {@code -Dgci.jcmd=<path>}, else {@code java.home/bin}, else
 * {@code JAVA_HOME/bin}.
 */
public class GciRv8IdleDcmdProbe {
    static final class Held8 {
        final long v;

        Held8(long v) {
            this.v = v;
        }
    }

    static final Pattern ROW = Pattern.compile("^ {0,3}\\d+: +(\\d+) +(\\d+)  (.+)$");
    static final long JCMD_TIMEOUT_S = 120;
    static final long PROMPT_MS = 5000;

    static Held8[] KEPT;
    static Held8[] DROPPED;
    static volatile boolean stop;
    static volatile Object sink;

    static final List<String> CAUSES = new ArrayList<>();
    static volatile boolean recording;

    static String jcmdPath() {
        String exe = System.getProperty("os.name").toLowerCase().contains("win") ? "jcmd.exe" : "jcmd";
        String jcmd = System.getProperty("gci.jcmd");
        if (jcmd != null) {
            return jcmd;
        }
        Path home = Path.of(System.getProperty("java.home"), "bin", exe);
        String envHome = System.getenv("JAVA_HOME");
        if (!Files.isRegularFile(home) && envHome != null) {
            home = Path.of(envHome, "bin", exe);
        }
        return home.toString();
    }

    /** Runs jcmd with its output in a file; returns {rc, output, ms}. rc -1: killed. */
    static Object[] jcmd(String jcmd, long pid, String... command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        for (String c : command) {
            cmd.add(c);
        }
        Path out = Files.createTempFile("gci-rv8-jcmd-", ".txt");
        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(true);
        pb.redirectOutput(ProcessBuilder.Redirect.to(out.toFile()));
        long t0 = System.nanoTime();
        Process p = pb.start();
        p.getOutputStream().close();
        int rc;
        if (p.waitFor(JCMD_TIMEOUT_S, TimeUnit.SECONDS)) {
            rc = p.exitValue();
        } else {
            p.destroyForcibly();
            p.waitFor();
            rc = -1;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        String text = new String(Files.readAllBytes(out), StandardCharsets.UTF_8);
        Files.deleteIfExists(out);
        return new Object[] {rc, text, ms};
    }

    static long rowCount(String text, String name) {
        for (String line : text.split("\\R")) {
            Matcher m = ROW.matcher(line);
            if (m.matches() && m.group(3).equals(name)) {
                return Long.parseLong(m.group(1));
            }
        }
        return -1;
    }

    static Held8[] fill(int n, long sign) {
        Held8[] a = new Held8[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Held8(sign * i);
        }
        return a;
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = gc.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    static void listen() {
        NotificationListener l = (Notification n, Object handback) -> {
            if (!recording) {
                return;
            }
            if (!GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                return;
            }
            GarbageCollectionNotificationInfo info =
                    GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
            synchronized (CAUSES) {
                CAUSES.add(info.getGcCause());
            }
        };
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc instanceof NotificationEmitter) {
                ((NotificationEmitter) gc).addNotificationListener(l, null, null);
            }
        }
    }

    /** As GciHd4HeapWalkProbe's trickle: ~16 KB per 10 ms, keeps nothing. */
    static void trickle() {
        while (!stop) {
            for (int i = 0; i < 16; i++) {
                sink = new byte[1024];
            }
            try {
                Thread.sleep(10);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "idle";
        boolean busy = mode.equals("busy");
        String jcmd = jcmdPath();
        if (!Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-rv8] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();
        gcCount(); // loads the management classes before the window
        listen();
        KEPT = fill(5000, 1);
        DROPPED = fill(20000, -1);
        System.gc();
        System.gc();
        DROPPED = null;
        Held8[] kept = KEPT;
        pause(300); // the System.gc() notifications land before the window
        Thread t = null;
        if (busy) {
            t = new Thread(GciRv8IdleDcmdProbe::trickle, "gci-rv8-trickle");
            t.setDaemon(true);
            t.start();
        }
        long g0 = gcCount();
        recording = true;
        Object[] live = jcmd(jcmd, pid, "GC.class_histogram");
        long g1 = gcCount();
        stop = true;
        if (t != null) {
            t.join();
        }
        pause(500); // the walk's notification, if any, is delivered
        recording = false;
        TreeSet<String> causes;
        synchronized (CAUSES) {
            causes = new TreeSet<>(CAUSES);
            CAUSES.clear();
        }
        long liveCount = rowCount((String) live[1], "GciRv8IdleDcmdProbe$Held8");
        long ms = (Long) live[2];
        // After the walk: a little allocation (far below any young
        // generation the rows use) and a pause. HotSpot runs no collection
        // here; a request the walk left pending runs one.
        long c1 = gcCount();
        recording = true;
        for (int i = 0; i < 4096; i++) {
            sink = new byte[1024];
        }
        pause(500);
        recording = false;
        long c2 = gcCount();
        TreeSet<String> after;
        synchronized (CAUSES) {
            after = new TreeSet<>(CAUSES);
        }
        System.out.println(mode + " live histogram rc " + live[0]);
        System.out.println(mode + " live histogram held exact " + (liveCount == kept.length));
        System.out.println(mode + " walk prompt " + (ms < PROMPT_MS));
        System.out.println(mode + " causes during walk " + causes);
        System.out.println(mode + " extra collections after walk " + (c2 - c1));
        System.out.println("kept " + kept.length + " first " + kept[0].v + " last " + kept[kept.length - 1].v);
        System.err.println("[gci-rv8] " + mode + " held8 live=" + liveCount + " walk_ms=" + ms
                + " gcs_in_window=" + (g1 - g0) + " extra=" + (c2 - c1) + " causes_after=" + after);
    }
}
