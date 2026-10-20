// SPDX-License-Identifier: Apache-2.0
// JIT round 19 wave 1 (lane eintr19, FFMEINTR18-1): a cross-thread
// Thread.getStackTrace() of a thread sitting in C through an FFM downcall
// (r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world, the frame-trace
// residual). HotSpot answers at once (a thread in native is walkable); before
// the fix CratonVM's frame-trace pause waited for the C call to return.
//
// A worker calls, from INTERPRETED code (each method runs once), a 3 s C sleep
// (Linux poll(NULL, 0, 3000); Windows kernel32 Sleep(3000)). Main waits until
// the worker is in C, takes worker.getStackTrace(), and reports whether the
// call returned while the worker was still in C, and the probe's own frames of
// that trace, innermost first.
//
// Lines (HotSpot prints the same):
//   trace returned while the worker was in C: true
//   worker frames: R18Eintr19TraceInC.sleepInC R18Eintr19TraceInC.workerBody R18Eintr19TraceInC$Worker.run
//   worker state in C: RUNNABLE
//   worker returned: true
//   PASS
// stderr: "time trace <n> ms" (how long getStackTrace took).
//
// Arms (CratonVM, --enable-native-access=ALL-UNNAMED, both modes; the worker is
// deliberately interpreted): the default and explicit
// CRATONVM_FFM_LAZY_CLAIM_FRAME_TRACE_PAUSE=0 arms both return the HotSpot lines.
// FrameTrace's ListableNative excusal publishes the held native thread's saved
// frames without waiting for C; the lazy-claim flag only controls the separate
// freeze-only claim path. CRATONVM_FRAME_TRACE=1 emits its diagnostics.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.util.ArrayList;
import java.util.List;

public class R18Eintr19TraceInC {
    static final int SLEEP_MS = 3000;
    static volatile boolean inC;
    static volatile boolean returned;
    static volatile String failure;
    static MethodHandle sleepMh;
    static boolean linux;

    static void sleepInC(int ms) throws Throwable {
        inC = true;
        if (linux) {
            int rc = (int) sleepMh.invokeExact(MemorySegment.NULL, 0L, ms);
            if (rc != 0) {
                failure = "poll rc " + rc;
            }
        } else {
            sleepMh.invokeExact(ms);
        }
        returned = true;
    }

    static void workerBody() throws Throwable {
        sleepInC(SLEEP_MS);
    }

    static final class Worker extends Thread {
        Worker() {
            super("worker");
        }

        @Override
        public void run() {
            try {
                workerBody();
            } catch (Throwable t) {
                failure = "worker " + t;
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        linux = System.getProperty("os.name").toLowerCase().contains("linux");
        Linker linker = Linker.nativeLinker();
        if (linux) {
            sleepMh = linker.downcallHandle(linker.defaultLookup().find("poll").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                            ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
        } else {
            SymbolLookup k32 = SymbolLookup.libraryLookup("kernel32.dll", Arena.global());
            sleepMh = linker.downcallHandle(k32.find("Sleep").orElseThrow(),
                    FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        }
        Worker w = new Worker();
        w.start();
        while (!inC) {
            Thread.sleep(1);
        }
        Thread.sleep(200);
        long t0 = System.nanoTime();
        StackTraceElement[] st = w.getStackTrace();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        boolean stillInC = !returned;
        Thread.State state = w.getState();
        List<String> own = new ArrayList<>();
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith("R18Eintr19TraceInC")) {
                own.add(e.getClassName() + "." + e.getMethodName());
            }
        }
        System.err.println("time trace " + ms + " ms");
        System.out.println("trace returned while the worker was in C: " + stillInC);
        System.out.println("worker frames: " + String.join(" ", own));
        System.out.println("worker state in C: " + (stillInC ? state : "(returned)"));
        w.join();
        System.out.println("worker returned: " + returned);
        if (failure == null && !stillInC) {
            failure = "waited";
        }
        if (failure == null && !own.equals(List.of("R18Eintr19TraceInC.sleepInC",
                "R18Eintr19TraceInC.workerBody", "R18Eintr19TraceInC$Worker.run"))) {
            failure = "frames";
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
