// SPDX-License-Identifier: Apache-2.0
// Cross-thread trace of a warmed FFM caller, with optional reflection.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.util.ArrayList;
import java.util.List;

public class FfmHeldTraceProbe {
    static final int SLEEP_MS = 3000;
    static volatile boolean inC;
    static volatile boolean returned;
    static volatile String failure;
    static MethodHandle sleepMh;
    static boolean linux;
    static boolean reflective;
    static boolean expectWait;

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

    static void workerBody(int ms) throws Throwable {
        sleepInC(ms);
    }

    static final class Worker extends Thread {
        Worker() {
            super("worker");
        }

        @Override
        public void run() {
            try {
                if (reflective) {
                    FfmHeldTraceProbe.class.getDeclaredMethod("workerBody", int.class).invoke(null, SLEEP_MS);
                } else {
                    workerBody(SLEEP_MS);
                }
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
        reflective = args.length != 0 && args[0].equals("reflective");
        expectWait = args.length > 1 && args[1].equals("wait");
        for (int i = 0; i < 2500; i++) workerBody(0);
        inC = false;
        returned = false;
        failure = null;
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
            if (e.getClassName().startsWith("FfmHeldTraceProbe")) {
                own.add(e.getClassName() + "." + e.getMethodName());
            }
        }
        System.err.println("time trace " + ms + " ms");
        System.out.println("trace returned while the worker was in C: " + stillInC);
        System.out.println("worker frames: " + String.join(" ", own));
        System.out.println("worker state in C: " + (stillInC ? state : "(returned)"));
        w.join();
        System.out.println("worker returned: " + returned);
        if (failure == null && expectWait && stillInC) {
            failure = "did-not-wait";
        }
        if (failure == null && !expectWait && !stillInC) {
            failure = "waited";
        }
        List<String> expected = expectWait
                ? List.of("FfmHeldTraceProbe$Worker.run")
                : List.of("FfmHeldTraceProbe.sleepInC", "FfmHeldTraceProbe.workerBody",
                        "FfmHeldTraceProbe$Worker.run");
        if (failure == null && !own.equals(expected)) {
            failure = "frames";
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
