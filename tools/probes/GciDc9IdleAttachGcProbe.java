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
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i9/dc: the two other attach commands that collect before answering,
 * on a target whose only Java thread is BLOCKED waiting for jcmd (the
 * idle-server shape of GciRv8IdleDcmdProbe): {@code jcmd GC.run}
 * ({@code gcrun}) and {@code jcmd GC.heap_dump <file>} without {@code -all}
 * ({@code dump}). HotSpot runs either collection at once from its VM
 * thread, with the cause {@code Diagnostic Command} /
 * {@code Heap Dump Initiated GC}, and runs no collection afterwards.
 *
 * <p>stdout (deterministic):
 * <pre>
 * &lt;mode&gt; rc 0
 * &lt;mode&gt; prompt true
 * &lt;mode&gt; causes during command [Diagnostic Command]   (dump: [Heap Dump Initiated GC])
 * &lt;mode&gt; extra collections after command 0
 * &lt;mode&gt; dump file written true                     (gcrun: n/a)
 * kept 5000 first 0 last 4999
 * </pre>
 * stderr: {@code [gci-dc9] ...} with the round trip, the counts, the causes
 * and the jcmd output. jcmd: {@code -Dgci.jcmd=<path>}, else
 * {@code java.home/bin}, else {@code JAVA_HOME/bin}.
 */
public class GciDc9IdleAttachGcProbe {
    static final class Held9 {
        final long v;

        Held9(long v) {
            this.v = v;
        }
    }

    static final long JCMD_TIMEOUT_S = 120;
    static final long PROMPT_MS = 5000;

    static Held9[] KEPT;
    static Held9[] DROPPED;
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
        Path out = Files.createTempFile("gci-dc9-jcmd-", ".txt");
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

    static Held9[] fill(int n, long sign) {
        Held9[] a = new Held9[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Held9(sign * i);
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

    static void pause(long ms) {
        try {
            Thread.sleep(ms);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "gcrun";
        boolean dump = mode.equals("dump");
        String jcmd = jcmdPath();
        if (!Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-dc9] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();
        gcCount(); // loads the management classes before the window
        listen();
        KEPT = fill(5000, 1);
        DROPPED = fill(20000, -1);
        System.gc();
        System.gc();
        DROPPED = null;
        Held9[] kept = KEPT;
        Path dir = Files.createTempDirectory("gci-dc9-");
        Path file = dir.resolve("live.hprof");
        pause(300); // the System.gc() notifications land before the window
        long g0 = gcCount();
        recording = true;
        Object[] res = dump
                ? jcmd(jcmd, pid, "GC.heap_dump", file.toAbsolutePath().toString())
                : jcmd(jcmd, pid, "GC.run");
        long g1 = gcCount();
        pause(500); // the command's notification, if any, is delivered
        recording = false;
        TreeSet<String> causes;
        synchronized (CAUSES) {
            causes = new TreeSet<>(CAUSES);
            CAUSES.clear();
        }
        long ms = (Long) res[2];
        boolean written = Files.isRegularFile(file) && Files.size(file) > 0;
        Files.deleteIfExists(file);
        Files.deleteIfExists(dir);
        // After the command: a little allocation (far below any young
        // generation the rows use) and a pause. HotSpot runs no collection
        // here; a request the command left pending runs one.
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
        System.out.println(mode + " rc " + res[0]);
        System.out.println(mode + " prompt " + (ms < PROMPT_MS));
        System.out.println(mode + " causes during command " + causes);
        System.out.println(mode + " extra collections after command " + (c2 - c1));
        System.out.println(mode + " dump file written " + (dump ? String.valueOf(written) : "n/a"));
        System.out.println("kept " + kept.length + " first " + kept[0].v + " last " + kept[kept.length - 1].v);
        System.err.println("[gci-dc9] " + mode + " ms=" + ms + " gcs_in_window=" + (g1 - g0)
                + " extra=" + (c2 - c1) + " causes_after=" + after + " jcmd_out="
                + ((String) res[1]).replace('\n', '|'));
    }
}
