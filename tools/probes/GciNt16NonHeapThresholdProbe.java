// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryNotificationInfo;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.function.IntUnaryOperator;
import javax.management.Notification;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;
import javax.management.openmbean.CompositeData;

/**
 * gci i16/nt -- a usage threshold on a NON-heap pool ({@code Metaspace}, the
 * code heaps) sends {@code MEMORY_THRESHOLD_EXCEEDED} on crossing and is
 * counted, as HotSpot 25's low-memory detector does. Page:
 * {@code docs/known-issues/gc/gci-i14mp-memory-pool-and-gc-mxbean-review-findings-20261013.md}
 * item 7's residual.
 *
 * <p>Modes (first argument):
 * <ul>
 * <li>{@code setnow} -- {@code Metaspace}'s threshold set to 1 byte (already
 *     exceeded: HotSpot checks at the set), then raised far above the usage,
 *     then set to 1 byte again: two crossings, two notifications, count 2.
 * <li>{@code load} -- {@code Metaspace}'s threshold set 512 KiB above the
 *     usage, classes defined by fresh loaders until the usage reaches it: one
 *     notification, count 1, still 1 after more loading (a gauge).
 * <li>{@code code} -- {@code CodeHeap 'non-profiled nmethods'}'s threshold set
 *     64 KiB above its usage, distinct hot classes run until the usage reaches
 *     it: one notification, count 1.
 * <li>{@code codeonly} (gci i18/dk) -- as {@code code}, with every class
 *     defined before the threshold is set: the crossing is by compilation
 *     alone.
 * </ul>
 *
 * <p>Every definition is {@code GciNt16Payload} (its own source file, compiled
 * into the same class dir), read back as a resource and defined by a fresh
 * {@link OneShot} loader; every defined class is kept reachable.
 *
 * <p>Waiting for a delivery allocates (64 KiB arrays) and sleeps 20 ms per
 * round, at most 10 s; after 3 s each round also calls {@code System.gc()}
 * (recorded on stderr: it only matters to a VM whose delivery waits for a
 * drain point). stdout is deterministic; sizes, rounds and timings go to
 * stderr ({@code [gci-nt16] ...}).
 */
public final class GciNt16NonHeapThresholdProbe {
    static final String META = "Metaspace";
    static final String CODE = "CodeHeap 'non-profiled nmethods'";
    static final List<Object> KEEP = new ArrayList<>();
    static final AtomicInteger NOTIFIED = new AtomicInteger();
    static final AtomicLong LAST_USED = new AtomicLong(-1);
    static final AtomicLong LAST_COUNT = new AtomicLong(-1);
    static volatile String watched = META;
    static volatile Object sink;
    static volatile long acc;
    static byte[] payload;
    static boolean gcFallback;

    /** One loader per definition, so every define is a new class. */
    static final class OneShot extends ClassLoader {
        OneShot() {
            super(null);
        }

        Class<?> define(byte[] b) {
            return defineClass("GciNt16Payload", b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "setnow";
        try (InputStream in =
                GciNt16NonHeapThresholdProbe.class.getResourceAsStream("/GciNt16Payload.class")) {
            payload = in.readAllBytes();
        }
        NotificationEmitter emitter = (NotificationEmitter) ManagementFactory.getMemoryMXBean();
        NotificationListener listener = (Notification n, Object hb) -> {
            if (!MemoryNotificationInfo.MEMORY_THRESHOLD_EXCEEDED.equals(n.getType())) {
                return;
            }
            MemoryNotificationInfo info =
                    MemoryNotificationInfo.from((CompositeData) n.getUserData());
            if (info.getPoolName().equals(watched)) {
                LAST_USED.set(info.getUsage().getUsed());
                LAST_COUNT.set(info.getCount());
                NOTIFIED.incrementAndGet();
            }
        };
        emitter.addNotificationListener(listener, null, null);
        System.out.println("mode " + mode);
        if (mode.equals("setnow")) {
            setnow();
        } else if (mode.equals("load")) {
            load();
        } else if (mode.equals("code")) {
            code();
        } else if (mode.equals("codeonly")) {
            codeonly();
        } else {
            throw new IllegalArgumentException(mode);
        }
        System.err.println("[gci-nt16] gc-fallback-used=" + gcFallback);
        System.out.println("DONE");
    }

    static MemoryPoolMXBean pool(String name) {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.NON_HEAP && p.getName().equals(name)) {
                return p;
            }
        }
        throw new IllegalStateException("no non-heap pool " + name);
    }

    /** Wait (allocating) until {@code NOTIFIED >= want}; returns the rounds taken. */
    static int awaitNotified(int want, String what) throws Exception {
        long t0 = System.nanoTime();
        int rounds = 0;
        while (NOTIFIED.get() < want && System.nanoTime() - t0 < 10_000_000_000L) {
            for (int i = 0; i < 16; i++) {
                sink = new byte[64 * 1024];
            }
            if (System.nanoTime() - t0 > 3_000_000_000L) {
                gcFallback = true;
                System.gc();
            }
            Thread.sleep(20);
            rounds++;
        }
        System.err.println("[gci-nt16] " + what + " waited_ms=" + (System.nanoTime() - t0) / 1_000_000
                + " rounds=" + rounds + " notified=" + NOTIFIED.get());
        return rounds;
    }

    /** Settle: let a late (duplicate) delivery arrive before a count is read. */
    static void settle() throws Exception {
        for (int r = 0; r < 10; r++) {
            for (int i = 0; i < 16; i++) {
                sink = new byte[64 * 1024];
            }
            Thread.sleep(20);
        }
    }

    static Class<?> defineOne() {
        OneShot l = new OneShot();
        Class<?> c = l.define(payload);
        KEEP.add(c);
        return c;
    }

    static void setnow() throws Exception {
        watched = META;
        MemoryPoolMXBean p = pool(META);
        System.out.println("setnow usage threshold supported: " + p.isUsageThresholdSupported());
        p.setUsageThreshold(1);
        awaitNotified(1, "setnow-1");
        settle();
        System.out.println("setnow notified once: " + (NOTIFIED.get() == 1));
        System.out.println("setnow count after the first crossing: " + p.getUsageThresholdCount());
        System.out.println("setnow exceeded: " + p.isUsageThresholdExceeded());
        long used = p.getUsage().getUsed();
        p.setUsageThreshold(used + (1L << 30));
        settle();
        System.out.println("setnow exceeded after raising: " + p.isUsageThresholdExceeded());
        System.out.println("setnow no notification on raising: " + (NOTIFIED.get() == 1));
        p.setUsageThreshold(1);
        awaitNotified(2, "setnow-2");
        settle();
        System.out.println("setnow notified twice: " + (NOTIFIED.get() == 2));
        System.out.println("setnow count after the second crossing: " + p.getUsageThresholdCount());
        System.out.println("setnow notification count field: " + LAST_COUNT.get());
        p.setUsageThreshold(0);
    }

    static void load() throws Exception {
        watched = META;
        MemoryPoolMXBean p = pool(META);
        long used0 = p.getUsage().getUsed();
        long threshold = used0 + 512 * 1024;
        p.setUsageThreshold(threshold);
        settle();
        System.out.println("load no notification below the threshold: " + (NOTIFIED.get() == 0));
        int loaded = 0;
        long used = used0;
        while (used < threshold && loaded < 4000) {
            defineOne();
            loaded++;
            if (loaded % 10 == 0) {
                used = p.getUsage().getUsed();
            }
        }
        boolean crossed = used >= threshold;
        System.err.println("[gci-nt16] load used0=" + used0 + " threshold=" + threshold + " used=" + used
                + " loaded=" + loaded);
        System.out.println("load crossed by class loading: " + crossed);
        // A VM that samples Metaspace per expansion (HotSpot) has sent it by
        // now; one that samples on a growth cadence sees it within more
        // defines. Keep defining (bounded) while waiting.
        int more = 0;
        while (NOTIFIED.get() < 1 && more < 3000) {
            defineOne();
            more++;
            if (more % 20 == 0) {
                for (int i = 0; i < 16; i++) {
                    sink = new byte[64 * 1024];
                }
            }
        }
        System.err.println("[gci-nt16] load extra-defines-before-notified=" + more);
        awaitNotified(1, "load");
        settle();
        System.out.println("load notified once: " + (NOTIFIED.get() == 1));
        System.out.println("load count: " + p.getUsageThresholdCount());
        System.out.println("load notification usage at or above the threshold: "
                + (LAST_USED.get() >= threshold));
        System.out.println("load notification count field: " + LAST_COUNT.get());
        for (int i = 0; i < 300; i++) {
            defineOne();
        }
        settle();
        System.out.println("load still one notification after more loading: " + (NOTIFIED.get() == 1));
        System.out.println("load count after more loading: " + p.getUsageThresholdCount());
        System.out.println("load exceeded: " + p.isUsageThresholdExceeded());
        p.setUsageThreshold(0);
    }

    static void code() throws Exception {
        watched = CODE;
        MemoryPoolMXBean p = pool(CODE);
        long used0 = p.getUsage().getUsed();
        long threshold = used0 + 64 * 1024;
        p.setUsageThreshold(threshold);
        settle();
        System.out.println("code no notification below the threshold: " + (NOTIFIED.get() == 0));
        long t0 = System.nanoTime();
        long used = used0;
        int classes = 0;
        while (used < threshold && System.nanoTime() - t0 < 30_000_000_000L) {
            IntUnaryOperator op = (IntUnaryOperator) defineOne().getDeclaredConstructor().newInstance();
            classes++;
            long a = 0;
            for (int i = 0; i < 40_000; i++) {
                a += op.applyAsInt(i);
            }
            acc += a;
            used = p.getUsage().getUsed();
        }
        boolean crossed = used >= threshold;
        System.err.println("[gci-nt16] code used0=" + used0 + " threshold=" + threshold + " used=" + used
                + " classes=" + classes + " crossed=" + crossed);
        System.out.println("code crossed by compilation: " + crossed);
        awaitNotified(1, "code");
        settle();
        System.out.println("code notified once: " + (NOTIFIED.get() == 1));
        System.out.println("code count: " + p.getUsageThresholdCount());
        System.out.println("code notification usage at or above the threshold: "
                + (LAST_USED.get() >= threshold));
        p.setUsageThreshold(0);
    }

    /**
     * gci i18/dk: as {@code code}, but every class is defined BEFORE the
     * threshold is set, so the crossing is by compilation alone (no define
     * after it). A VM that checks the code heaps only at a define or a
     * collection's end sees the crossing at the next collection; HotSpot (and
     * a VM that checks at compile) at once. The wait is on stderr
     * ({@code codeonly waited_ms=}).
     */
    static void codeonly() throws Exception {
        watched = CODE;
        MemoryPoolMXBean p = pool(CODE);
        List<IntUnaryOperator> ops = new ArrayList<>();
        for (int i = 0; i < 600; i++) {
            ops.add((IntUnaryOperator) defineOne().getDeclaredConstructor().newInstance());
        }
        settle();
        long used0 = p.getUsage().getUsed();
        long threshold = used0 + 64 * 1024;
        p.setUsageThreshold(threshold);
        settle();
        System.out.println("codeonly no notification below the threshold: " + (NOTIFIED.get() == 0));
        long t0 = System.nanoTime();
        long used = used0;
        int ran = 0;
        while (used < threshold && ran < ops.size() && System.nanoTime() - t0 < 30_000_000_000L) {
            IntUnaryOperator op = ops.get(ran);
            ran++;
            long a = 0;
            for (int i = 0; i < 40_000; i++) {
                a += op.applyAsInt(i);
            }
            acc += a;
            used = p.getUsage().getUsed();
        }
        boolean crossed = used >= threshold;
        System.err.println("[gci-nt16] codeonly used0=" + used0 + " threshold=" + threshold + " used=" + used
                + " ran=" + ran + " crossed=" + crossed);
        System.out.println("codeonly crossed by compilation: " + crossed);
        awaitNotified(1, "codeonly");
        settle();
        System.out.println("codeonly notified once: " + (NOTIFIED.get() == 1));
        System.out.println("codeonly count: " + p.getUsageThresholdCount());
        System.out.println("codeonly notification usage at or above the threshold: "
                + (LAST_USED.get() >= threshold));
        p.setUsageThreshold(0);
    }
}
