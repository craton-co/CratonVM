// SPDX-License-Identifier: Apache-2.0
// Redefine a class while a compiled caller is blocked in an FFM downcall.
// The no-roots Redefinition pause must claim the in-native thread, finish the
// swap before C returns, and let the caller observe the armed post-call loop
// exit. Build a jar with Premain-Class: R20FfmRedefInC$Agent and
// Can-Redefine-Classes: true, then run with -javaagent and
// CRATONVM_JIT_THRESHOLD=1.
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;

public class R20FfmRedefInC {
    static volatile Instrumentation instrumentation;
    static MethodHandle sleep;
    static volatile boolean windows;

    public static class Agent {
        public static void premain(String args, Instrumentation value) {
            instrumentation = value;
        }
    }

    public static class Tgt {
        static volatile boolean inC;
        static volatile boolean returnedFromC;
        static volatile boolean stop;
        static volatile String seen;

        public static String tag() { return "old-body"; }

        public static void spin() {
            while (!stop) {
                try {
                    inC = true;
                    if (windows) {
                        sleep.invokeExact(3000);
                    } else {
                        int rc = (int) sleep.invokeExact(MemorySegment.NULL, 0L, 3000);
                        if (rc != 0) throw new IllegalStateException("poll rc=" + rc);
                    }
                    inC = false;
                    returnedFromC = true;
                    seen = tag();
                } catch (Throwable t) {
                    inC = false;
                    throw new RuntimeException(t);
                }
            }
        }
    }

    /** Same class shape, with a changed constant pool and method body. */
    public static class Dgt {
        static volatile boolean inC;
        static volatile boolean returnedFromC;
        static volatile boolean stop;
        static volatile String seen;

        public static String tag() { return "edited-body"; }

        public static void spin() {
            while (!stop) {
                try {
                    inC = true;
                    if (windows) {
                        sleep.invokeExact(3000);
                    } else {
                        int rc = (int) sleep.invokeExact(MemorySegment.NULL, 0L, 3000);
                        if (rc != 0) throw new IllegalStateException("poll rc=" + rc);
                    }
                    inC = false;
                    returnedFromC = true;
                    seen = tag();
                } catch (Throwable t) {
                    inC = false;
                    throw new RuntimeException(t);
                }
            }
        }
    }

    static byte[] classBytes(String name) throws Exception {
        try (InputStream in = R20FfmRedefInC.class.getResourceAsStream("R20FfmRedefInC$" + name + ".class")) {
            if (in == null) throw new IllegalStateException("missing class bytes: " + name);
            return in.readAllBytes();
        }
    }

    static byte[] renamedDonor() throws Exception {
        byte[] bytes = classBytes("Dgt");
        byte[] from = "R20FfmRedefInC$Dgt".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        byte[] to = "R20FfmRedefInC$Tgt".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        for (int i = 0; i + from.length <= bytes.length; i++) {
            boolean match = true;
            for (int j = 0; j < from.length && match; j++) match = bytes[i + j] == from[j];
            if (match) System.arraycopy(to, 0, bytes, i, to.length);
        }
        return bytes;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation inst = instrumentation;
        if (inst == null || !inst.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        windows = System.getProperty("os.name").toLowerCase().contains("win");
        Linker linker = Linker.nativeLinker();
        if (windows) {
            SymbolLookup k32 = SymbolLookup.libraryLookup("kernel32.dll", java.lang.foreign.Arena.global());
            sleep = linker.downcallHandle(k32.find("Sleep").orElseThrow(), FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        } else {
            sleep = linker.downcallHandle(linker.defaultLookup().find("poll").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
        }
        Tgt.tag();
        Thread worker = new Thread(Tgt::spin, "ffm-redefinition-worker");
        worker.setDaemon(true);
        worker.start();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (!Tgt.inC && System.nanoTime() < deadline) Thread.sleep(1);
        if (!Tgt.inC) throw new IllegalStateException("worker never entered C");
        Thread.sleep(150);
        long start = System.nanoTime();
        inst.redefineClasses(new ClassDefinition(Tgt.class, renamedDonor()));
        long redefineMs = (System.nanoTime() - start) / 1_000_000L;
        // This Java marker runs after the downcall returns. A disabled pause
        // can wait until the native sleep ends, then release this thread only
        // after redefineClasses returns, so elapsed time adjudicates the wait.
        boolean postCallMarkerIncomplete = !Tgt.returnedFromC;
        String fresh = Tgt.tag();
        Tgt.stop = true;
        worker.join(10_000);
        System.out.println("worker post-call marker incomplete at redefine return: "
                + postCallMarkerIncomplete);
        System.out.println("redefine ms: " + redefineMs);
        System.out.println("fresh tag: " + fresh);
        System.out.println("worker stopped: " + !worker.isAlive());
        System.out.println(postCallMarkerIncomplete && redefineMs < 2000
                && "edited-body".equals(fresh) && !worker.isAlive()
                ? "PASS" : "FAIL redefine waited or stale body");
    }
}
