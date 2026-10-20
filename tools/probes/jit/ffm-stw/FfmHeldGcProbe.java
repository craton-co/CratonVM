// SPDX-License-Identifier: Apache-2.0
// A warmed compiled caller holds its sole local array across C and collections.
// Run with JIT threshold 1 and DBG_FFM to audit the pinned-deposit GC licence.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.util.ArrayList;
import java.util.List;

public class FfmHeldGcProbe {
    static final int SLEEP_MS = 12000;
    static volatile boolean ready;
    static volatile boolean inC;
    static volatile boolean returned;
    static volatile String failure;
    static MethodHandle sleepMh;
    static boolean linux;
    static boolean reflective;

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
        byte[] held = new byte[257];
        held[0] = 37;
        held[256] = 91;
        sleepInC(ms);
        if (held[0] != 37 || held[256] != 91) throw new AssertionError("held array moved incorrectly");
    }

    static final class Worker extends Thread {
        Worker() {
            super("worker");
        }

        @Override
        public void run() {
            try {
                // Compilation/profiling on the actual owner is part of this
                // gate: warming only main can leave a fresh worker interpreted.
                for (int i = 0; i < 2500; i++) workerBody(0);
                inC = false;
                returned = false;
                ready = true;
                if (reflective) {
                    FfmHeldGcProbe.class.getDeclaredMethod("workerBody", int.class).invoke(null, SLEEP_MS);
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
        for (int i = 0; i < 2500; i++) workerBody(0);
        inC = false;
        returned = false;
        failure = null;
        Worker w = new Worker();
        w.start();
        while (!ready || !inC) {
            if (failure != null) throw new AssertionError(failure);
            Thread.sleep(1);
        }
        Thread.sleep(200);
        Object[] keep = new Object[8192];
        // Keep a live 8 MiB object set while churning 64 MiB through the young
        // generation. The worker array remains pinned, while these survivors
        // must move; this makes the relocation assertion meaningful in both
        // runtime modes.
        for (int i = 0; i < 65536; i++) keep[i & 8191] = new byte[1024];
        for (int i = 0; i < 3; i++) System.gc();
        boolean collectedInC = !returned;
        long t0 = System.nanoTime();
        StackTraceElement[] st = w.getStackTrace();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        boolean stillInC = !returned;
        Thread.State state = w.getState();
        List<String> own = new ArrayList<>();
        for (StackTraceElement e : st) {
            if (e.getClassName().startsWith("FfmHeldGcProbe")) {
                own.add(e.getClassName() + "." + e.getMethodName());
            }
        }
        System.err.println("time trace " + ms + " ms");
        System.out.println("gc returned while the worker was in C: " + collectedInC);
        System.out.println("trace returned while the worker was in C: " + stillInC);
        System.out.println("worker frames: " + String.join(" ", own));
        System.out.println("worker state in C: " + (stillInC ? state : "(returned)"));
        w.join();
        System.out.println("worker returned: " + returned);
        if (failure == null && (!stillInC || !collectedInC)) {
            failure = "waited";
        }
        if (failure == null && !own.equals(List.of("FfmHeldGcProbe.sleepInC",
                "FfmHeldGcProbe.workerBody", "FfmHeldGcProbe$Worker.run"))) {
            failure = "frames";
        }
        System.out.println(failure == null ? "PASS" : "FAIL " + failure);
    }
}
