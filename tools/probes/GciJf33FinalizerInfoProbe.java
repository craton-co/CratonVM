// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i33/jf -- {@code GC.finalizer_info} and {@code GC.run_finalization}
 * (page {@code gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-20261015},
 * item 2), through the DiagnosticCommand MBean (on CratonVM only under
 * {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1}).
 *
 * <p>One {@code Blocker}'s {@code finalize()} holds the finalizing thread on a
 * latch; then 15 more finalizables die ({@code Bee} x5, {@code Cee} x3,
 * {@code Dee} x2, {@code Eee} x2, {@code Fee}, {@code Gee}, {@code Hee} x1)
 * and wait in the queue. HotSpot 25's {@code FinalizerInfoDCmd} prints them
 * as {@code FinalizerHistogram} sorts them: by count, descending, ties in its
 * {@code HashMap}'s order (Dee before Eee; Hee, Gee, Fee: bins 0, 15 and 12,
 * 13, 14 of 16). After the latch opens and every finalizer ran, the queue is
 * empty. Run CratonVM with {@code CRATONVM_FINALIZER_THREAD=1} (every
 * finalizer on the VM's daemon, as HotSpot's "Finalizer" thread): under the
 * default policy the blocker runs inline on main, which then waits out the
 * latch.
 *
 * <pre>
 *   java -Xmx64m GciJf33FinalizerInfoProbe [gc|alloc|runfin]   (gc, the default: System.gc() polls; alloc: garbage;
 *                                                               runfin: alloc + System.runFinalization() before release)
 *   CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1 CRATONVM_FINALIZER_THREAD=1 cratonvm -Xmx64m GciJf33FinalizerInfoProbe
 * </pre>
 *
 * stdout: {@code registered true}, {@code blocker started true},
 * {@code == queued} and the command's text, {@code finalized before release 0} (hand-back 1),
 * ({@code runfin}: {@code runFinalization ran 15}), {@code finalized 16},
 * {@code == run_finalization [<output>]}, {@code == drained} and the text,
 * {@code done}. stderr: {@code [jf33] ...} polling counts and times.
 */
public class GciJf33FinalizerInfoProbe {
    static final CountDownLatch STARTED = new CountDownLatch(1);
    static final CountDownLatch RELEASE = new CountDownLatch(1);
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final int QUEUED = 15;

    static class Blocker {
        @SuppressWarnings("removal")
        @Override
        protected void finalize() throws InterruptedException {
            STARTED.countDown();
            RELEASE.await(20, TimeUnit.SECONDS);
            FINALIZED.incrementAndGet();
        }
    }

    static class Counted {
        final byte[] pad = new byte[32];

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static class Bee extends Counted {}
    static class Cee extends Counted {}
    static class Dee extends Counted {}
    static class Eee extends Counted {}
    static class Fee extends Counted {}
    static class Gee extends Counted {}
    static class Hee extends Counted {}

    static volatile Object sink;
    static boolean alloc;

    /**
     * One collection request. {@code gc} mode: {@code System.gc()}. {@code alloc} mode (hand-back 1): 8 MB of
     * short-lived garbage instead, so only allocation-driven collections run -- on CratonVM {@code System.gc()}
     * runs queued finalizers on the CALLER once its 2 s wait for a busy delivery thread runs out (page
     * {@code gci-i35jf-system-gc-runs-queued-finalizers-on-the-caller-20261016}), which HotSpot never does.
     */
    static void collect() {
        if (!alloc) {
            System.gc();
            return;
        }
        for (int i = 0; i < 8 * 1024; i++) {
            sink = new byte[1024];
        }
        sink = null;
    }

    static void makeBlocker() {
        sink = new Blocker();
        sink = null;
    }

    static void makeQueued() {
        for (int i = 0; i < 5; i++) sink = new Bee();
        for (int i = 0; i < 3; i++) sink = new Cee();
        for (int i = 0; i < 2; i++) sink = new Dee();
        for (int i = 0; i < 2; i++) sink = new Eee();
        sink = new Fee();
        sink = new Gee();
        sink = new Hee();
        sink = null;
    }

    static String run(MBeanServer server, ObjectName on, String op) {
        try {
            Object out = server.invoke(on, op, new Object[] {new String[0]},
                    new String[] {String[].class.getName()});
            return String.valueOf(out);
        } catch (Exception e) {
            return "threw " + e.getClass().getName() + "\n";
        }
    }

    /** The instances the command's rows count ({@code %10d  %s}). */
    static int rowsTotal(String text) {
        int total = 0;
        for (String line : text.split("\n")) {
            String t = line.trim();
            int sp = t.indexOf("  ");
            if (sp > 0) {
                try {
                    total += Integer.parseInt(t.substring(0, sp));
                } catch (NumberFormatException e) {
                    // a header line
                }
            }
        }
        return total;
    }

    public static void main(String[] args) throws Exception {
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName on = new ObjectName("com.sun.management:type=DiagnosticCommand");
        boolean registered = server.isRegistered(on);
        System.out.println("registered " + registered);
        if (!registered) {
            System.out.println("done");
            return;
        }
        String mode = args.length > 0 ? args[0] : "gc";
        alloc = mode.equals("alloc") || mode.equals("runfin");
        long t0 = System.nanoTime();
        makeBlocker();
        boolean started = false;
        for (int i = 0; i < 100 && !started; i++) {
            collect();
            started = STARTED.await(100, TimeUnit.MILLISECONDS);
        }
        System.out.println("blocker started " + started);
        makeQueued();
        String queued = "";
        for (int i = 0; i < 100; i++) {
            collect();
            queued = run(server, on, "gcFinalizerInfo");
            int total = rowsTotal(queued);
            System.err.println("[jf33] poll " + i + " total " + total);
            if (total >= QUEUED) {
                break;
            }
            Thread.sleep(100);
        }
        System.out.println("== queued");
        System.out.print(queued);
        System.out.println("finalized before release " + FINALIZED.get());
        if (mode.equals("runfin")) {
            // hand-back 2: runFinalization must still run the queue while the
            // finalizing thread is busy (HotSpot: a secondary finalizer).
            System.runFinalization();
            System.out.println("runFinalization ran " + FINALIZED.get());
        }
        RELEASE.countDown();
        for (int i = 0; i < 200 && FINALIZED.get() < QUEUED + 1; i++) {
            collect();
            Thread.sleep(50);
        }
        System.out.println("finalized " + FINALIZED.get());
        System.out.println("== run_finalization [" + run(server, on, "gcRunFinalization") + "]");
        System.out.println("== drained");
        System.out.print(run(server, on, "gcFinalizerInfo"));
        System.err.println("[jf33] ms " + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("done");
    }
}
