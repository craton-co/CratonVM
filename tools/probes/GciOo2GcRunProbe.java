// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i2/oo: {@code jcmd <this pid> GC.run} must run HotSpot's
 * {@code _dcmd_gc_run} -- a FULL collection, cause {@code Diagnostic Command}
 * -- before jcmd returns, and print nothing of its own, so that jcmd prints
 * {@code Command executed successfully}. Page:
 * {@code docs/known-issues/gc/gci-i1pp-jcmd-gc-run-is-a-deferred-young-collection-reported-as-completed-20261011.md}.
 *
 * <p>The probe spawns the JDK's own {@code jcmd} (from {@code java.home}, or
 * the {@code -Dgci.jcmd=<path>} override) against its own pid while a daemon
 * thread keeps allocating, then waits up to 10 s for the GC notification of
 * the collection. stdout is deterministic (HotSpot 25 {@code -XX:+UseSerialGC}):
 *
 * <pre>
 * jcmd rc 0
 * jcmd says Command executed successfully true
 * full collection by return true
 * dcmd cause seen true
 * </pre>
 *
 * Diagnostics (jcmd path and output, old-collector counts, every notification's
 * collector/action/cause, the jcmd round trip in ms) go to stderr.
 */
public final class GciOo2GcRunProbe {
    static volatile boolean stop;
    static volatile Object sink;

    /** The collector whose pools include the old generation (Serial: MarkSweepCompact). */
    static GarbageCollectorMXBean oldBean() {
        GarbageCollectorMXBean old = null;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            for (String pool : b.getMemoryPoolNames()) {
                if (pool.contains("Old") || pool.contains("Tenured")) {
                    old = b;
                }
            }
        }
        return old;
    }

    public static void main(String[] args) throws Exception {
        GarbageCollectorMXBean old = oldBean();
        List<String> seenCauses = new CopyOnWriteArrayList<>();
        NotificationListener listener = (Notification n, Object handback) -> {
            if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                seenCauses.add(info.getGcName() + "/" + info.getGcAction() + "/" + info.getGcCause());
            }
        };
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) b).addNotificationListener(listener, null, null);
        }
        // A running mutator: CratonVM runs the requested collection on the next
        // thread that allocates, and this thread is about to block in jcmd.
        Thread alloc = new Thread(() -> {
            while (!stop) {
                sink = new byte[256];
            }
        }, "gci-oo2-alloc");
        alloc.setDaemon(true);
        alloc.start();

        long before = old == null ? -1 : old.getCollectionCount();
        String exe = System.getProperty("os.name").toLowerCase().contains("win") ? "jcmd.exe" : "jcmd";
        String jcmd = System.getProperty("gci.jcmd");
        if (jcmd == null) {
            Path home = Path.of(System.getProperty("java.home"), "bin", exe);
            String envHome = System.getenv("JAVA_HOME");
            if (!Files.isRegularFile(home) && envHome != null) {
                home = Path.of(envHome, "bin", exe);
            }
            jcmd = home.toString();
        }
        if (!Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-oo2] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();
        ProcessBuilder pb = new ProcessBuilder(jcmd, Long.toString(pid), "GC.run");
        pb.redirectErrorStream(true);
        long t0 = System.nanoTime();
        Process p = pb.start();
        byte[] out = p.getInputStream().readAllBytes();
        int rc = p.waitFor();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long after = old == null ? -1 : old.getCollectionCount();
        String text = new String(out, StandardCharsets.UTF_8);
        boolean ok = false;
        for (String line : text.split("\\R")) {
            if (line.trim().equals("Command executed successfully")) {
                ok = true;
            }
        }
        System.out.println("jcmd rc " + rc);
        System.out.println("jcmd says Command executed successfully " + ok);
        System.out.println("full collection by return " + (old != null && after > before));

        // GC notifications are asynchronous on HotSpot too.
        boolean seen = false;
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (!seen && System.nanoTime() < deadline) {
            for (String c : seenCauses) {
                if (c.endsWith("/Diagnostic Command")) {
                    seen = true;
                }
            }
            if (!seen) {
                Thread.sleep(20);
            }
        }
        stop = true;
        System.out.println("dcmd cause seen " + seen);

        System.err.println("[gci-oo2] jcmd=" + jcmd + " ms=" + ms + " old="
                + (old == null ? "none" : old.getName()) + " " + before + "->" + after);
        System.err.println("[gci-oo2] jcmd output: " + text.replace('\r', ' ').replace('\n', '|'));
        StringBuilder causes = new StringBuilder();
        for (String c : seenCauses) {
            if (!c.endsWith("/Allocation Failure")) {
                causes.append(c).append(';');
            }
        }
        System.err.println("[gci-oo2] non-allocation notifications: " + causes);
    }
}
