// SPDX-License-Identifier: Apache-2.0
// JIT round 17 wave 2 (lane ffm3): the residual EINTR race of the Linux
// take-over (r17-takeover-signal-residual-eintr-windows-20261005, section 1).
// The take-over reads /proc/self/task/<tid>/syscall and leaves a thread
// blocked in a syscall alone, but a thread it reads RUNNING in C can enter a
// blocking syscall before the signal lands, and that syscall fails with EINTR.
// HotSpot never signals a thread to reach a safepoint: it sees none.
//
// Linux only. SLEEPERS threads each run ROUNDS rounds of C work then a short
// sleep, all through compiled loops, while "alloc" forces collections:
//  * "busy-usleep":  memset of 256 KiB (C busy, no syscall), then usleep(30);
//  * "short-poll":   poll(NULL, 0, 1);
//  * "short-nanosleep": nanosleep of 20 us.
// Many short sleeps keep each thread crossing from running C into a blocking
// syscall thousands of times per second, which is the window. Each kind
// counts calls that returned -1 with errno EINTR (4) through
// captureCallState("errno").
//
// Lines (HotSpot prints the same): one per kind ("calls N, EINTR seen: 0,
// other errors: 0"), "memset check: ok", "allocator: ok", "PASS" / "FAIL
// <first reason>". No timings. About 5 s on HotSpot.
//
// Arms (CratonVM, Linux, --enable-native-access=ALL-UNNAMED -Xmx64m), each
// also with CRATONVM_DBG_XT_FORCE_TAKEOVER=1 to make the take-over fire:
//  * D (default): the defect -- "EINTR seen" may be above 0 (a race; 0 is
//    possible on a quiet run);
//  * M = CRATONVM_FFM_DOWNCALL_MASK_TAKEOVER_SIGNAL=1: PASS;
//  * M + CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL=0: PASS (the mask alone keeps
//    every signal off the sleepers' C code);
//  * D + CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL=0: the positive control, EINTR
//    above 0 and FAIL.
//
// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-6): progress on STDERR only
// (stdout, the HotSpot diff, is unchanged), so a battery timeout leaves a
// location instead of an empty file: "progress phase <p>" at each phase
// change, and every 10 s "progress t=<s>s phase=<p> calls=[..] alloc=<n>
// gc=<n> states=[..]" with each thread's per-call counter and
// Thread.getState(); "progress STALLED <s>s" when no counter moved for 30 s.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicIntegerArray;

public class R17Ffm3EintrRace {
    static final int SLEEPERS = 2;
    static final int ROUNDS = 6000;
    static final int WARM = 2000;
    static final int EINTR = 4;
    static final long BUSY = 256 * 1024;
    static volatile boolean done;
    static volatile String failure;
    static volatile Object sink;
    static VarHandle errnoHandle;

    // Progress (stderr only): slot 0..5 the sleepers' calls, 6 alloc rounds,
    // 7 System.gc() calls. Each slot has ONE writer.
    static final int ALLOC_SLOT = 6;
    static final int GC_SLOT = 7;
    static final AtomicIntegerArray PROGRESS = new AtomicIntegerArray(8);
    static final String[] SLOT_NAMES = new String[8];
    static volatile String phase = "start";

    static void tick(int slot) {
        PROGRESS.lazySet(slot, PROGRESS.get(slot) + 1);
    }

    static void phase(String p) {
        phase = p;
        System.err.println("progress phase " + p);
    }

    static void startProgress(Thread[] watched) {
        Thread t = new Thread(() -> {
            long t0 = System.nanoTime();
            int[] last = new int[8];
            long movedAt = t0;
            boolean stalledSaid = false;
            while (true) {
                try {
                    Thread.sleep(10_000);
                } catch (InterruptedException e) {
                    return;
                }
                long now = System.nanoTime();
                StringBuilder sb = new StringBuilder("progress t=")
                        .append((now - t0) / 1_000_000_000L).append("s phase=").append(phase)
                        .append(" calls=[");
                boolean moved = false;
                for (int i = 0; i < 8; i++) {
                    int v = PROGRESS.get(i);
                    if (v != last[i]) {
                        moved = true;
                    }
                    last[i] = v;
                    if (i < ALLOC_SLOT) {
                        sb.append(i == 0 ? "" : " ").append(SLOT_NAMES[i]).append('=').append(v);
                    }
                }
                sb.append("] alloc=").append(last[ALLOC_SLOT]).append(" gc=").append(last[GC_SLOT])
                        .append(" states=[");
                for (int i = 0; i < watched.length; i++) {
                    Thread w = watched[i];
                    sb.append(i == 0 ? "" : " ").append(w == null ? "-" : w.getName() + ":" + w.getState());
                }
                sb.append(']');
                System.err.println(sb);
                if (moved) {
                    movedAt = now;
                    stalledSaid = false;
                } else if (!stalledSaid && now - movedAt >= 30_000_000_000L) {
                    stalledSaid = true;
                    System.err.println("progress STALLED " + (now - movedAt) / 1_000_000_000L
                            + "s: no counter moved (phase=" + phase + ")");
                }
            }
        }, "progress");
        t.setDaemon(true);
        t.start();
    }

    static void fail(String what) {
        if (failure == null) {
            failure = what;
        }
    }

    /** [calls, eintr, other errors] per kind, summed over the sleepers. */
    static final int[] usleepStats = new int[3];
    static final int[] pollStats = new int[3];
    static final int[] nanoStats = new int[3];
    static volatile boolean memsetBad;

    static synchronized void merge(int[] into, int[] from) {
        for (int i = 0; i < into.length; i++) {
            into[i] += from[i];
        }
    }

    static void tally(int[] stats, int rc, MemorySegment st) {
        stats[0]++;
        if (rc == -1) {
            if ((int) errnoHandle.get(st, 0L) == EINTR) {
                stats[1]++;
            } else {
                stats[2]++;
            }
        }
    }

    static void busyUsleepLoop(MethodHandle memset, MethodHandle usleep, MemorySegment st,
            MemorySegment buf, int n, int[] stats, int slot) throws Throwable {
        for (int i = 0; i < n; i++) {
            tick(slot);
            MemorySegment r = (MemorySegment) memset.invokeExact(buf, i & 0x7f, BUSY);
            if (buf.get(ValueLayout.JAVA_BYTE, BUSY - 1) != (byte) (i & 0x7f)) {
                memsetBad = true;
            }
            int rc = (int) usleep.invokeExact(st, 30);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static void pollLoop(MethodHandle poll, MemorySegment st, int n, int ms, int[] stats, int slot)
            throws Throwable {
        for (int i = 0; i < n; i++) {
            tick(slot);
            int rc = (int) poll.invokeExact(st, MemorySegment.NULL, 0L, ms);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static void nanosleepLoop(MethodHandle nanosleep, MemorySegment st, MemorySegment req,
            int n, long ns, int[] stats, int slot) throws Throwable {
        for (int i = 0; i < n; i++) {
            tick(slot);
            req.set(ValueLayout.JAVA_LONG, 0, 0L);
            req.set(ValueLayout.JAVA_LONG, 8, ns);
            int rc = (int) nanosleep.invokeExact(st, req, MemorySegment.NULL);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static boolean allocLoop() {
        byte[][] ring = new byte[64][];
        int round = 0;
        boolean ok = true;
        while (!done) {
            int slot = round & 63;
            byte[] old = ring[slot];
            if (old != null) {
                byte want = (byte) (round - 64);
                for (int i = 0; i < old.length; i += 97) {
                    if (old[i] != want) {
                        ok = false;
                    }
                }
            }
            byte[] a = new byte[16 * 1024];
            java.util.Arrays.fill(a, (byte) round);
            ring[slot] = a;
            sink = new long[64];
            if ((round % 40) == 39) {
                tick(GC_SLOT);
                System.gc();
            }
            round++;
            tick(ALLOC_SLOT);
        }
        return ok && round > 0;
    }

    interface Body {
        void run() throws Throwable;
    }

    static Thread start(String name, Body body) {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (Throwable e) {
                fail(name + " " + e);
            }
        }, name);
        t.start();
        return t;
    }

    static String line(String kind, int[] s) {
        return kind + ": calls " + s[0] + ", EINTR seen: " + s[1] + ", other errors: " + s[2];
    }

    public static void main(String[] args) throws Throwable {
        if (!System.getProperty("os.name").toLowerCase().contains("linux")) {
            System.out.println("not Linux: skipped");
            System.out.println("PASS");
            return;
        }
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        Arena arena = Arena.ofShared();
        Linker.Option errnoOpt = Linker.Option.captureCallState("errno");
        errnoHandle = Linker.Option.captureStateLayout()
                .varHandle(MemoryLayout.PathElement.groupElement("errno"));
        MethodHandle usleep = linker.downcallHandle(std.find("usleep").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT), errnoOpt);
        MethodHandle memset = linker.downcallHandle(std.find("memset").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.JAVA_INT,
                        ValueLayout.JAVA_LONG));
        MethodHandle poll = linker.downcallHandle(std.find("poll").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_INT),
                errnoOpt);
        MethodHandle nanosleep = linker.downcallHandle(std.find("nanosleep").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS),
                errnoOpt);

        boolean[] allocOk = new boolean[1];
        Thread[] sleepers = new Thread[SLEEPERS * 3];
        Thread[] watched = new Thread[SLEEPERS * 3 + 2];
        watched[0] = Thread.currentThread();
        startProgress(watched);
        phase("alloc-starting");
        Thread alloc = start("alloc", () -> allocOk[0] = allocLoop());
        watched[1] = alloc;
        phase("sleepers-starting");
        for (int t = 0; t < SLEEPERS; t++) {
            MemorySegment buf = arena.allocate(BUSY, 64);
            MemorySegment stU = arena.allocate(Linker.Option.captureStateLayout());
            MemorySegment stP = arena.allocate(Linker.Option.captureStateLayout());
            MemorySegment stN = arena.allocate(Linker.Option.captureStateLayout());
            MemorySegment req = arena.allocate(16, 8);
            final int slotU = 3 * t, slotP = 3 * t + 1, slotN = 3 * t + 2;
            SLOT_NAMES[slotU] = "usleep" + t;
            SLOT_NAMES[slotP] = "poll" + t;
            SLOT_NAMES[slotN] = "nanosleep" + t;
            sleepers[3 * t] = start("busy-usleep", () -> {
                int[] s = new int[3];
                busyUsleepLoop(memset, usleep, stU, buf, WARM, null, slotU);
                busyUsleepLoop(memset, usleep, stU, buf, ROUNDS, s, slotU);
                merge(usleepStats, s);
            });
            sleepers[3 * t + 1] = start("short-poll", () -> {
                int[] s = new int[3];
                pollLoop(poll, stP, WARM, 0, null, slotP);
                pollLoop(poll, stP, ROUNDS / 3, 1, s, slotP);
                merge(pollStats, s);
            });
            sleepers[3 * t + 2] = start("short-nanosleep", () -> {
                int[] s = new int[3];
                nanosleepLoop(nanosleep, stN, req, WARM, 1_000L, null, slotN);
                nanosleepLoop(nanosleep, stN, req, ROUNDS, 20_000L, s, slotN);
                merge(nanoStats, s);
            });
        }
        for (int i = 0; i < sleepers.length; i++) {
            watched[2 + i] = sleepers[i];
        }
        phase("sleepers-joining");
        for (Thread t : sleepers) {
            t.join();
        }
        phase("alloc-joining");
        done = true;
        alloc.join();
        phase("printing");

        System.out.println(line("busy-usleep", usleepStats));
        System.out.println(line("short-poll", pollStats));
        System.out.println(line("short-nanosleep", nanoStats));
        System.out.println("memset check: " + (memsetBad ? "FAILED" : "ok"));
        System.out.println("allocator: " + (allocOk[0] ? "ok" : "FAILED"));

        int[][] all = {usleepStats, pollStats, nanoStats};
        String[] names = {"busy-usleep", "short-poll", "short-nanosleep"};
        int[] want = {SLEEPERS * ROUNDS, SLEEPERS * (ROUNDS / 3), SLEEPERS * ROUNDS};
        for (int k = 0; k < all.length; k++) {
            if (all[k][0] != want[k]) {
                fail(names[k] + " calls");
            }
            if (all[k][1] != 0) {
                fail(names[k] + " EINTR");
            }
            if (all[k][2] != 0) {
                fail(names[k] + " errors");
            }
        }
        if (memsetBad) {
            fail("memset");
        }
        if (!allocOk[0]) {
            fail("allocator");
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
        arena.close();
        phase("done");
    }
}
