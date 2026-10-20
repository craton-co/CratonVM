// SPDX-License-Identifier: Apache-2.0
// JIT round 17 wave 1 (lane eintr): blocking system calls made from C through
// FFM must not fail with EINTR because the VM signalled the thread
// (r16-ffm-takeover-signal-interrupts-blocking-syscalls-in-native-20261002).
// HotSpot never signals a thread to reach a safepoint.
//
// Linux only. Five threads at once:
//  * "usleep", "nanosleep", "poll": each sits in C (usleep(150000),
//    nanosleep 150 ms, poll(NULL, 0, 150)) EPISODES times, after a warm-up of
//    short calls through the same method (so the method is likely compiled and
//    the take-over sees a non-zero JIT depth for the sleeper), and counts the
//    calls that returned -1 with errno EINTR (4). None of the three is
//    restarted by SA_RESTART: one take-over signal per sleep is one EINTR;
//  * "read": read(2) of one byte from a pipe that "writer" fills one byte
//    every 150 ms -- SA_RESTART restarts it, so it is the NEGATIVE control (no
//    arm may see EINTR there);
//  * "alloc": allocates and verifies byte arrays and calls System.gc() so
//    that pauses run while the others are in C.
//
// Lines (HotSpot prints the same): one per call kind with its EINTR count,
// "allocator: ok", and "PASS" / "FAIL <first reason>". No timings.
//
// Arms (CratonVM, Linux, --enable-native-access=ALL-UNNAMED -Xmx64m):
//  * default: PASS (the take-over leaves a peer blocked in a syscall outside
//    compiled code unsignalled, CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL);
//  * CRATONVM_XT_TAKEOVER_SKIP_IN_SYSCALL=0 (+ CRATONVM_DBG_XT_FORCE_TAKEOVER=1):
//    the positive control, "EINTR seen" above 0 on usleep / nanosleep / poll
//    and FAIL. If it passes, no pass signalled a sleeper and the default arm
//    proves nothing.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;

public class R17EintrNative {
    static final int EPISODES = 8;
    static final int WARM = 2000;
    static final int EINTR = 4;
    static volatile boolean done;
    static volatile String failure;
    static volatile Object sink;

    static void fail(String what) {
        if (failure == null) {
            failure = what;
        }
    }

    /** [calls, eintr, other errors] per kind. */
    static final int[] usleepStats = new int[3];
    static final int[] nanoStats = new int[3];
    static final int[] pollStats = new int[3];
    static final int[] readStats = new int[4]; // calls, eintr, other, bytes

    static VarHandle errnoHandle;

    static int errno(MemorySegment state) {
        return (int) errnoHandle.get(state, 0L);
    }

    static void tally(int[] stats, int rc, MemorySegment state) {
        stats[0]++;
        if (rc == -1) {
            if (errno(state) == EINTR) {
                stats[1]++;
            } else {
                stats[2]++;
            }
        }
    }

    static void usleepLoop(MethodHandle usleep, MemorySegment st, int n, int us, int[] stats)
            throws Throwable {
        for (int i = 0; i < n; i++) {
            int rc = (int) usleep.invokeExact(st, us);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static void nanosleepLoop(MethodHandle nanosleep, MemorySegment st, MemorySegment req,
            int n, long ns, int[] stats) throws Throwable {
        for (int i = 0; i < n; i++) {
            req.set(ValueLayout.JAVA_LONG, 0, ns / 1_000_000_000L);
            req.set(ValueLayout.JAVA_LONG, 8, ns % 1_000_000_000L);
            int rc = (int) nanosleep.invokeExact(st, req, MemorySegment.NULL);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static void pollLoop(MethodHandle poll, MemorySegment st, int n, int ms, int[] stats)
            throws Throwable {
        for (int i = 0; i < n; i++) {
            int rc = (int) poll.invokeExact(st, MemorySegment.NULL, 0L, ms);
            if (stats != null) {
                tally(stats, rc, st);
            }
        }
    }

    static void readLoop(MethodHandle read, MemorySegment st, int fd, MemorySegment buf,
            int n, int[] stats) throws Throwable {
        for (int i = 0; i < n; i++) {
            long rc = (long) read.invokeExact(st, fd, buf, 1L);
            if (stats != null) {
                stats[0]++;
                if (rc == 1) {
                    stats[3]++;
                } else if (rc == -1 && errno(st) == EINTR) {
                    stats[1]++;
                } else {
                    stats[2]++;
                }
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
            byte[] a = new byte[32 * 1024];
            java.util.Arrays.fill(a, (byte) round);
            ring[slot] = a;
            sink = new long[64];
            if ((round % 50) == 49) {
                System.gc();
            }
            round++;
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
        MethodHandle nanosleep = linker.downcallHandle(std.find("nanosleep").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.ADDRESS),
                errnoOpt);
        MethodHandle poll = linker.downcallHandle(std.find("poll").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_INT),
                errnoOpt);
        MethodHandle pipe = linker.downcallHandle(std.find("pipe").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
        MethodHandle read = linker.downcallHandle(std.find("read").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                        ValueLayout.JAVA_LONG),
                errnoOpt);
        MethodHandle write = linker.downcallHandle(std.find("write").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                        ValueLayout.JAVA_LONG));
        MethodHandle close = linker.downcallHandle(std.find("close").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));

        MemorySegment fds = arena.allocate(ValueLayout.JAVA_INT, 2);
        if ((int) pipe.invokeExact(fds) != 0) {
            System.out.println("FAIL pipe");
            return;
        }
        final int rfd = fds.getAtIndex(ValueLayout.JAVA_INT, 0);
        final int wfd = fds.getAtIndex(ValueLayout.JAVA_INT, 1);
        final MemorySegment one = arena.allocate(1);
        one.set(ValueLayout.JAVA_BYTE, 0, (byte) 'x');
        // The read warm-up's bytes, written up front (the pipe holds 64 KiB).
        for (int i = 0; i < WARM; i++) {
            if ((long) write.invokeExact(wfd, one, 1L) != 1L) {
                fail("pipe warm-up write");
            }
        }

        final MemorySegment stU = arena.allocate(Linker.Option.captureStateLayout());
        final MemorySegment stN = arena.allocate(Linker.Option.captureStateLayout());
        final MemorySegment stP = arena.allocate(Linker.Option.captureStateLayout());
        final MemorySegment stR = arena.allocate(Linker.Option.captureStateLayout());
        final MemorySegment req = arena.allocate(16, 8);
        final MemorySegment rbuf = arena.allocate(1);

        boolean[] allocOk = new boolean[1];
        Thread alloc = start("alloc", () -> allocOk[0] = allocLoop());
        Thread tu = start("usleep", () -> {
            usleepLoop(usleep, stU, WARM, 1, null);
            usleepLoop(usleep, stU, EPISODES, 150_000, usleepStats);
        });
        Thread tn = start("nanosleep", () -> {
            nanosleepLoop(nanosleep, stN, req, WARM, 1_000L, null);
            nanosleepLoop(nanosleep, stN, req, EPISODES, 150_000_000L, nanoStats);
        });
        Thread tp = start("poll", () -> {
            pollLoop(poll, stP, WARM, 0, null);
            pollLoop(poll, stP, EPISODES, 150, pollStats);
        });
        Thread tr = start("read", () -> {
            readLoop(read, stR, rfd, rbuf, WARM, null);
            readLoop(read, stR, rfd, rbuf, EPISODES, readStats);
        });
        Thread tw = start("writer", () -> {
            for (int i = 0; i < EPISODES; i++) {
                Thread.sleep(150);
                if ((long) write.invokeExact(wfd, one, 1L) != 1L) {
                    fail("pipe write");
                }
            }
        });
        tu.join();
        tn.join();
        tp.join();
        tr.join();
        tw.join();
        done = true;
        alloc.join();
        int rc1 = (int) close.invokeExact(rfd);
        int rc2 = (int) close.invokeExact(wfd);

        System.out.println(line("usleep", usleepStats));
        System.out.println(line("nanosleep", nanoStats));
        System.out.println(line("poll", pollStats));
        System.out.println("read on a pipe: calls " + readStats[0] + ", bytes " + readStats[3]
                + ", EINTR seen: " + readStats[1] + ", other errors: " + readStats[2]);
        System.out.println("allocator: " + (allocOk[0] ? "ok" : "FAILED"));

        int[][] all = {usleepStats, nanoStats, pollStats};
        String[] names = {"usleep", "nanosleep", "poll"};
        for (int k = 0; k < all.length; k++) {
            if (all[k][0] != EPISODES) {
                fail(names[k] + " calls");
            }
            if (all[k][1] != 0) {
                fail(names[k] + " EINTR");
            }
            if (all[k][2] != 0) {
                fail(names[k] + " errors");
            }
        }
        if (readStats[0] != EPISODES || readStats[3] != EPISODES || readStats[1] != 0
                || readStats[2] != 0) {
            fail("read");
        }
        if (!allocOk[0]) {
            fail("allocator");
        }
        if (rc1 != 0 || rc2 != 0) {
            fail("close");
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
