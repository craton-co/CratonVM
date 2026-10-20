// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.management.ManagementFactory;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * gci i39/rw -- {@code GC.finalizer_info} names a HIDDEN finalizable class as
 * {@code Class.getName()} does ({@code FinalizerHistogram} keys its rows by
 * {@code getClass().getName()}): {@code GciRw39HFin/0x<hex>}, the hidden
 * suffix after a slash. The i33/jf text turned every {@code /} into
 * {@code .} ({@code GciRw39HFin.0x<hex>}).
 *
 * <p>Shape of {@code GciJf33FinalizerInfoProbe alloc}: a {@code Blocker}'s
 * {@code finalize()} holds the finalizing thread, then three instances of the
 * hidden class (defined from {@code GciRw39HFin.class}'s bytes) and two
 * {@code Plain}s die and wait in the queue; allocation-driven collections only.
 * The hidden suffix is masked ({@code /0xH}, {@code .0xH}) so stdout is
 * deterministic. Run CratonVM with {@code CRATONVM_JMX_DIAGNOSTIC_COMMAND_MBEAN=1
 * CRATONVM_FINALIZER_THREAD=1}.
 *
 * <pre>
 *   java -Xmx64m GciRw39HiddenFinalizerProbe
 * </pre>
 *
 * stdout: {@code registered true}, {@code hidden GciRw39HFin/0xH true},
 * {@code blocker started true}, {@code == queued} and the command's text,
 * {@code finalized 6}, {@code done}. stderr: {@code [rw39] ...} poll counts.
 */
public class GciRw39HiddenFinalizerProbe {
    static final CountDownLatch STARTED = new CountDownLatch(1);
    static final CountDownLatch RELEASE = new CountDownLatch(1);
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final int QUEUED = 5;

    static class Blocker {
        @SuppressWarnings("removal")
        @Override
        protected void finalize() throws InterruptedException {
            STARTED.countDown();
            RELEASE.await(20, TimeUnit.SECONDS);
            FINALIZED.incrementAndGet();
        }
    }

    static class Plain {
        final byte[] pad = new byte[32];

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            FINALIZED.incrementAndGet();
        }
    }

    static volatile Object sink;

    /** 8 MB of short-lived garbage: allocation-driven collections only. */
    static void collect() {
        for (int i = 0; i < 8 * 1024; i++) {
            sink = new byte[1024];
        }
        sink = null;
    }

    static Class<?> defineHidden() throws Exception {
        byte[] bytes;
        try (InputStream in = GciRw39HiddenFinalizerProbe.class.getResourceAsStream("/GciRw39HFin.class")) {
            bytes = in.readAllBytes();
        }
        return MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
    }

    static void makeBlocker() {
        sink = new Blocker();
        sink = null;
    }

    static void makeQueued(Class<?> hidden) throws Exception {
        for (int i = 0; i < 3; i++) {
            sink = hidden.getDeclaredConstructor().newInstance();
        }
        for (int i = 0; i < 2; i++) {
            sink = new Plain();
        }
        sink = null;
    }

    static String mask(String text) {
        return text.replaceAll("/0x[0-9a-fA-F]+", "/0xH").replaceAll("\\.0x[0-9a-fA-F]+", ".0xH");
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
        Class<?> hidden = defineHidden();
        System.out.println("hidden " + mask(hidden.getName()) + " " + hidden.isHidden());
        makeBlocker();
        boolean started = false;
        for (int i = 0; i < 100 && !started; i++) {
            collect();
            started = STARTED.await(100, TimeUnit.MILLISECONDS);
        }
        System.out.println("blocker started " + started);
        makeQueued(hidden);
        String queued = "";
        for (int i = 0; i < 100; i++) {
            collect();
            queued = run(server, on, "gcFinalizerInfo");
            int total = rowsTotal(queued);
            System.err.println("[rw39] poll " + i + " total " + total);
            if (total >= QUEUED) {
                break;
            }
            Thread.sleep(100);
        }
        System.out.println("== queued");
        System.out.print(mask(queued));
        RELEASE.countDown();
        for (int i = 0; i < 200 && FINALIZED.get() < QUEUED + 1; i++) {
            collect();
            Thread.sleep(50);
        }
        System.out.println("finalized " + FINALIZED.get());
        System.out.println("done");
    }
}

/** Defined as a hidden class by {@link GciRw39HiddenFinalizerProbe#defineHidden}; never loaded by name. */
class GciRw39HFin {
    final byte[] pad = new byte[32];

    public GciRw39HFin() {
    }

    @SuppressWarnings("removal")
    @Override
    protected void finalize() {
        GciRw39HiddenFinalizerProbe.FINALIZED.incrementAndGet();
    }
}
