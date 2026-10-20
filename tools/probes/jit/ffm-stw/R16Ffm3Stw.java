// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 14 (lane ffm3): a stop-the-world pause while another
// thread is blocked in C through an FFM downcall, and the per-call cost of a
// plain downcall, for the arms of the lazy in-native transition
// (CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY) against the counted default and the
// deposit-based region (CRATONVM_FFM_DOWNCALL_GC_SAFE).
//
// java --enable-native-access=ALL-UNNAMED -Xmx128m R16Ffm3Stw
//
// stdout, one token per line, "<label> <value>" (labels carry no digits):
//   cost-llabs-ns            median ns per call of a plain llabs downcall
//                            (a static final handle in a compiled loop)
//   cost-llabs-critical-ns   the same through a Linker.Option.critical handle
//                            (never transitions: the control row)
//   pause-idle-ms            System.gc() with nobody in C (worst of three)
//   pause-while-blocked-ms   System.gc() while a worker sits in a C sleep
//                            (Linux poll(NULL, 0, 4000), Windows Sleep(10000))
//   alloc-idle-ms            an allocation loop that collects (-Xmx128m), idle
//   alloc-while-blocked-ms   the same loop while a worker sits in C
// and facts (HotSpot prints the same):
//   gc finished while the worker was in C: true
//   alloc finished while the worker was in C: true
//   worker state in C: RUNNABLE
//   worker sleep: ok          (Linux: "interrupted N times, errno E" when a
//                              take-over signal broke the poll with EINTR)
//   PASS                      (FAIL stall: a pause waited for the C call)
// The counted default stalls: pause-while-blocked-ms is most of the 4 s and the
// "while the worker was in C" facts read false. Engagement of the lazy arm:
// CRATONVM_DBG_FFM=1 prints "[ffm-lazy] claimed os_tid=.. band=.. roots=.."
// on stderr for each pause that claimed the worker.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.VarHandle;
import java.util.Arrays;

public class R16Ffm3Stw {
    static final boolean WINDOWS = System.getProperty("os.name").toLowerCase().contains("win");
    static final Linker LINKER = Linker.nativeLinker();
    static final MethodHandle LLABS = LINKER.downcallHandle(
            LINKER.defaultLookup().find("llabs").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG));
    static final MethodHandle LLABS_CRIT = LINKER.downcallHandle(
            LINKER.defaultLookup().find("llabs").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG),
            Linker.Option.critical(false));
    // The loaded Windows gate runs four full VM processes at once. Its
    // allocation arm can take over 5 s under that scheduler load, so give it
    // a 10 s native window while preserving the 4 s Linux probe interval.
    static final int SLEEP_MS = Integer.getInteger("cratonvm.ffm.stw.sleep-ms",
            WINDOWS ? 10_000 : 4000);
    static final int GC_SLEEP_MS = Integer.getInteger(
            "cratonvm.ffm.stw.gc-sleep-ms", SLEEP_MS);
    static final int ALLOC_SLEEP_MS = Integer.getInteger(
            "cratonvm.ffm.stw.alloc-sleep-ms", SLEEP_MS);

    static volatile Object sink;
    static volatile boolean inC;
    static volatile boolean workerDone;
    static volatile int interrupted;
    static volatile int lastErrno;
    static volatile String failure;

    static long labsLoop(int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) LLABS.invokeExact((long) -i);
        }
        return sum;
    }

    static long labsCritLoop(int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) LLABS_CRIT.invokeExact((long) -i);
        }
        return sum;
    }

    interface Loop {
        long run(int n) throws Throwable;
    }

    static long medianNs(Loop loop) throws Throwable {
        final int n = 200_000;
        long expect = (long) n * (n - 1) / 2;
        for (int w = 0; w < 5; w++) {
            if (loop.run(n) != expect) {
                failure = "llabs answer";
            }
        }
        long[] t = new long[7];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            long s = loop.run(n);
            t[r] = (System.nanoTime() - t0) / n;
            if (s != expect) {
                failure = "llabs answer";
            }
        }
        Arrays.sort(t);
        return t[t.length / 2];
    }

    static long allocLoopMs() {
        long t0 = System.nanoTime();
        Object[] keep = new Object[64];
        for (int i = 0; i < 400_000; i++) {
            int[] a = new int[256];
            a[0] = i;
            keep[i & 63] = a;
        }
        sink = keep;
        return (System.nanoTime() - t0) / 1_000_000;
    }

    /** A worker that sleeps in C for the requested interval, once. */
    static Thread startWorker(Arena arena, int sleepMs) throws Throwable {
        final MethodHandle block;
        final MemorySegment state;
        final VarHandle errnoVh;
        if (WINDOWS) {
            SymbolLookup k32 = SymbolLookup.libraryLookup("kernel32", arena);
            block = LINKER.downcallHandle(k32.find("Sleep").orElseThrow(),
                    FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
            state = null;
            errnoVh = null;
        } else {
            StructLayout capture = Linker.Option.captureStateLayout();
            block = LINKER.downcallHandle(LINKER.defaultLookup().find("poll").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                            ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT),
                    Linker.Option.captureCallState("errno"));
            state = arena.allocate(capture);
            errnoVh = capture.varHandle(MemoryLayout.PathElement.groupElement("errno"));
        }
        inC = false;
        workerDone = false;
        Thread w = new Thread(() -> {
            try {
                inC = true;
                if (WINDOWS) {
                    block.invokeExact(sleepMs);
                } else {
                    int rc = (int) block.invokeExact(state, MemorySegment.NULL, 0L, sleepMs);
                    if (rc != 0) {
                        interrupted++;
                        lastErrno = (int) errnoVh.get(state, 0L);
                    }
                }
            } catch (Throwable t) {
                failure = "worker " + t;
            } finally {
                workerDone = true;
            }
        }, "in-C");
        w.start();
        while (!inC) {
            Thread.onSpinWait();
        }
        Thread.sleep(300); // well inside the C call
        return w;
    }

    public static void main(String[] args) throws Throwable {
        long cost = medianNs(R16Ffm3Stw::labsLoop);
        long costCrit = medianNs(R16Ffm3Stw::labsCritLoop);

        long idle = 0;
        for (int i = 0; i < 3; i++) {
            long t0 = System.nanoTime();
            System.gc();
            idle = Math.max(idle, (System.nanoTime() - t0) / 1_000_000);
        }
        allocLoopMs(); // warm
        long allocIdle = allocLoopMs();

        boolean gcInC;
        boolean allocInC;
        Thread.State seen;
        long pause;
        long allocBlocked;
        try (Arena arena = Arena.ofShared()) {
            Thread w = startWorker(arena, GC_SLEEP_MS);
            seen = w.getState();
            long t0 = System.nanoTime();
            System.gc();
            pause = (System.nanoTime() - t0) / 1_000_000;
            gcInC = !workerDone;
            w.join();

            w = startWorker(arena, ALLOC_SLEEP_MS);
            allocBlocked = allocLoopMs();
            allocInC = !workerDone;
            w.join();
        }

        System.out.println("cost-llabs-ns " + cost);
        System.out.println("cost-llabs-critical-ns " + costCrit);
        System.out.println("pause-idle-ms " + idle);
        System.out.println("pause-while-blocked-ms " + pause);
        System.out.println("alloc-idle-ms " + allocIdle);
        System.out.println("alloc-while-blocked-ms " + allocBlocked);
        System.out.println("gc finished while the worker was in C: " + gcInC);
        System.out.println("alloc finished while the worker was in C: " + allocInC);
        System.out.println("worker state in C: " + seen);
        System.out.println(interrupted == 0 ? "worker sleep: ok"
                : "worker sleep: interrupted " + interrupted + " times, errno " + lastErrno);
        if (failure != null) {
            System.out.println("FAIL " + failure);
        } else if (!gcInC || !allocInC) {
            System.out.println("FAIL stall");
        } else {
            System.out.println("PASS");
        }
    }
}
