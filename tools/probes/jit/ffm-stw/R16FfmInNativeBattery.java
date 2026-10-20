// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 9 (lane ffm): the loaded battery for the GC-safe FFM
// downcall (CRATONVM_FFM_DOWNCALL_GC_SAFE, and its quiet in-native transition
// CRATONVM_FFM_DOWNCALL_IN_NATIVE_QUIET).
//
// Four threads at once, for the length of four C sleeps:
//  * "sleeper" sits in a blocking downcall (usleep(4999999) / Sleep(5000)) four
//    times, flagging each episode;
//  * "gc" builds and verifies linked structures (a rolling window, so the
//    collector moves survivors) and runs System.gc() every round; the sleeper
//    reads the round count before and after each C sleep (a counted downcall
//    lets at most the one stalled pause finish per sleep);
//  * "fast" runs llabs and strlen downcalls (arguments in arena segments) and
//    checks every answer;
//  * "upcalls" runs qsort with a comparator that allocates (and collects every
//    256th call) from inside the upcall, and checks the order.
// Main samples the sleeper's Thread.getState() while it is in C.
//
// Lines (HotSpot prints the same): "gc structures verified: ok",
// "gc progress while a thread sat in C: ok" (>= 3 rounds in one sleep; the counted
// downcall prints "stalled"), "sleeper state in C: RUNNABLE", "llabs: ok",
// "strlen: ok", "qsort with allocating upcalls: ok", "PASS". stderr:
// "[r16-ffm-cost] <case> median=<n> ns/call" (normalised by run16.sh) for the
// cost A/B, measured on main AFTER the loaded part, from a method that is
// called repeatedly (compiled: check with CRATONVM_DBG_JITC=1 for
// R16FfmInNativeBattery.labsLoop / strlenLoop).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class R16FfmInNativeBattery {
    static final int EPISODES = 4;
    static volatile boolean done;
    static volatile int episode; // odd while the sleeper is in C
    static volatile int gcRounds; // collections the "gc" thread completed
    static volatile int bestEpisode; // most collections inside one C sleep
    static volatile Thread.State stateInC;
    static volatile Object sink;
    static volatile String failure;
    // Per-part verdicts, so one part's failure does not mark the others.
    static volatile boolean gcBad, labsBad, strlenBad, qsortBad;
    static volatile int sleepInterrupted;
    static volatile int sleepErrno;
    static int upcalls;

    static void fail(String what) {
        if (failure == null) {
            failure = what;
        }
    }

    static final class Node {
        final int v;
        final Node next;
        final int[] payload;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.payload = new int[] {v, ~v, v * 31};
        }
    }

    static Node build(int seed, int n) {
        Node h = null;
        for (int i = 0; i < n; i++) {
            h = new Node(seed + i, h);
        }
        return h;
    }

    static boolean check(Node h, int seed, int n) {
        int i = n - 1;
        for (Node x = h; x != null; x = x.next, i--) {
            int v = seed + i;
            if (x.v != v || x.payload[0] != v || x.payload[1] != ~v || x.payload[2] != v * 31) {
                return false;
            }
        }
        return i == -1;
    }

    static int gcLoop() {
        Node[] window = new Node[8];
        int[] seeds = new int[8];
        int round = 0;
        while (!done) {
            int slot = round & 7;
            if (window[slot] != null && !check(window[slot], seeds[slot], 2000)) {
                gcBad = true;
                fail("gc structure " + round);
            }
            seeds[slot] = round * 7919;
            window[slot] = build(seeds[slot], 2000);
            System.gc();
            gcRounds++;
            round++;
        }
        for (int s = 0; s < 8; s++) {
            if (window[s] != null && !check(window[s], seeds[s], 2000)) {
                gcBad = true;
                fail("gc structure final");
            }
        }
        return round;
    }

    static long labsLoop(MethodHandle labs, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) labs.invokeExact((long) -i);
            if (v != i) {
                labsBad = true;
                fail("llabs " + i + " -> " + v);
            }
            sum += v;
        }
        return sum;
    }

    static long strlenLoop(MethodHandle strlen, MemorySegment[] strs, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            MemorySegment s = strs[i % strs.length];
            long len = (long) strlen.invokeExact(s);
            if (len != (i % strs.length) + 1) {
                strlenBad = true;
                fail("strlen " + i + " -> " + len);
            }
            sum += len;
        }
        return sum;
    }

    static int compare(MemorySegment a, MemorySegment b) {
        upcalls++;
        sink = new long[16];
        if ((upcalls & 255) == 0) {
            System.gc();
        }
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    static void cost(String name, MethodHandle labs, int n) throws Throwable {
        long[] ns = new long[5];
        for (int r = 0; r < ns.length; r++) {
            long t0 = System.nanoTime();
            labsLoop(labs, n);
            ns[r] = (System.nanoTime() - t0) / n;
        }
        long[] s = ns.clone();
        Arrays.sort(s);
        System.err.println("[r16-ffm-cost] " + name + " median=" + s[2] + " ns/call");
    }

    static void costStrlen(MethodHandle strlen, MemorySegment[] strs, int n) throws Throwable {
        long[] ns = new long[5];
        for (int r = 0; r < ns.length; r++) {
            long t0 = System.nanoTime();
            strlenLoop(strlen, strs, n);
            ns[r] = (System.nanoTime() - t0) / n;
        }
        long[] s = ns.clone();
        Arrays.sort(s);
        System.err.println("[r16-ffm-cost] strlen median=" + s[2] + " ns/call");
    }

    public static void main(String[] args) throws Throwable {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        Arena arena = Arena.ofShared();
        MethodHandle labs = linker.downcallHandle(std.find("llabs").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));
        MethodHandle labsCritical = linker.downcallHandle(std.find("llabs").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG),
                Linker.Option.critical(false));
        MethodHandle strlen = linker.downcallHandle(std.find("strlen").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        MethodHandle qsort = linker.downcallHandle(std.find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG,
                        ValueLayout.JAVA_LONG, ValueLayout.ADDRESS));
        MethodHandle sleep;
        if (windows) {
            sleep = linker.downcallHandle(
                    SymbolLookup.libraryLookup("kernel32", arena).find("Sleep").orElseThrow(),
                    FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        } else {
            // errno captured: an interrupted sleep (EINTR, 4) is reported by
            // number and the run goes on, so the cost lines still print.
            sleep = linker.downcallHandle(std.find("usleep").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT),
                    Linker.Option.captureCallState("errno"));
        }
        MemorySegment[] strs = new MemorySegment[7];
        for (int i = 0; i < strs.length; i++) {
            strs[i] = arena.allocateFrom("x".repeat(i + 1));
        }
        MethodHandle cmp = MethodHandles.lookup().findStatic(R16FfmInNativeBattery.class, "compare",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        MemorySegment stub = linker.upcallStub(cmp, FunctionDescriptor.of(ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT),
                ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_INT)), arena);

        // Warm the measured methods before the loaded part.
        for (int r = 0; r < 20; r++) {
            labsLoop(labs, 1000);
            strlenLoop(strlen, strs, 1000);
        }

        final MethodHandle sleepCall = sleep;
        final MemorySegment errnoState = arena.allocate(Linker.Option.captureStateLayout());
        final java.lang.invoke.VarHandle errnoHandle = Linker.Option.captureStateLayout()
                .varHandle(MemoryLayout.PathElement.groupElement(windows ? "GetLastError" : "errno"));
        Thread sleeper = new Thread(() -> {
            try {
                for (int i = 0; i < EPISODES; i++) {
                    int r0 = gcRounds;
                    episode++; // odd: in C
                    if (windows) {
                        sleepCall.invokeExact(5000);
                    } else {
                        int rc = (int) sleepCall.invokeExact(errnoState, 4_999_999);
                        if (rc != 0) {
                            sleepInterrupted++;
                            sleepErrno = (int) errnoHandle.get(errnoState, 0L);
                            fail("usleep rc " + rc + " errno " + sleepErrno);
                        }
                    }
                    episode++; // even: back
                    bestEpisode = Math.max(bestEpisode, gcRounds - r0);
                    Thread.sleep(20);
                }
            } catch (Throwable t) {
                fail("sleeper " + t);
            }
            done = true;
        }, "sleeper");
        int[] rounds = new int[1];
        Thread gc = new Thread(() -> rounds[0] = gcLoop(), "gc");
        long[] fastSum = new long[2];
        Thread fast = new Thread(() -> {
            try {
                while (!done) {
                    fastSum[0] += labsLoop(labs, 500);
                    fastSum[1] += strlenLoop(strlen, strs, 500);
                }
            } catch (Throwable t) {
                labsBad = true;
                fail("fast " + t);
            }
        }, "fast");
        int[] sorts = new int[1];
        Thread up = new Thread(() -> {
            try (Arena local = Arena.ofConfined()) {
                int n = 512;
                MemorySegment array = local.allocate(ValueLayout.JAVA_INT, n);
                while (!done) {
                    for (int i = 0; i < n; i++) {
                        array.setAtIndex(ValueLayout.JAVA_INT, i, (i * 7919 + sorts[0]) % n);
                    }
                    qsort.invokeExact(array, (long) n, 4L, stub);
                    for (int i = 0; i < n; i++) {
                        if (array.getAtIndex(ValueLayout.JAVA_INT, i) != i) {
                            qsortBad = true;
                            fail("qsort order");
                        }
                    }
                    sorts[0]++;
                }
            } catch (Throwable t) {
                qsortBad = true;
                fail("upcalls " + t);
            }
        }, "upcalls");
        gc.start();
        fast.start();
        up.start();
        sleeper.start();
        while (!done) {
            if ((episode & 1) == 1 && stateInC == null) {
                Thread.sleep(100);
                if ((episode & 1) == 1) {
                    stateInC = sleeper.getState();
                }
            }
            Thread.sleep(5);
        }
        sleeper.join();
        gc.join();
        fast.join();
        up.join();
        System.out.println("sleeper usleep: " + (sleepInterrupted == 0 ? "ok"
                : "interrupted " + sleepInterrupted + " times, errno " + sleepErrno));
        System.out.println("gc structures verified: " + (gcBad ? "FAILED" : "ok"));
        System.out.println("gc progress while a thread sat in C: "
                + (bestEpisode >= 3 ? "ok" : "stalled best=" + bestEpisode));
        System.out.println("sleeper state in C: " + stateInC);
        System.out.println("llabs: " + (fastSum[0] > 0 && !labsBad ? "ok" : "FAILED"));
        System.out.println("strlen: " + (fastSum[1] > 0 && !strlenBad ? "ok" : "FAILED"));
        System.out.println("qsort with allocating upcalls: " + (sorts[0] > 0 && !qsortBad ? "ok" : "FAILED"));

        cost("llabs", labs, 20_000);
        cost("llabs-critical", labsCritical, 20_000);
        costStrlen(strlen, strs, 20_000);
        if (bestEpisode < 3 || rounds[0] == 0) {
            fail("stall");
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
