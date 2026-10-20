// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 9b (lane ffm): where a plain downcall's fixed cost goes.
// Each row is a compiled loop (call it with CRATONVM_DBG_JITC=1 to see
// costLoop* compiled) over one handle shape; stderr lines
// "[r16-ffm-cost2] <row> median=<n> ns/call" (5 reps of 20 000 calls).
// Rows, each adding one thing to the one before:
//   mh-java       a MethodHandle invokeExact of a Java static (long)long: the door alone
//   pid           getpid / GetCurrentProcessId: a downcall with no argument
//   llabs         one long argument and a long return
//   llabs-crit    the same through Linker.Option.critical(false)
//   llabs-errno   the same with captureCallState("errno"/"GetLastError")
//   strlen        one native segment argument (its session acquired per call)
//   strlen-crit   the same, critical
// stdout carries only correctness lines (HotSpot prints the same).
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class R16FfmDowncallCost {
    static final int N = 20_000;
    static boolean bad;

    static long javaAbs(long v) {
        return v < 0 ? -v : v;
    }

    static long costLoopLong(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) h.invokeExact((long) -i);
            if (v != i) {
                bad = true;
            }
            sum += v;
        }
        return sum;
    }

    static long costLoopErrno(MethodHandle h, MemorySegment state, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) h.invokeExact(state, (long) -i);
            if (v != i) {
                bad = true;
            }
            sum += v;
        }
        return sum;
    }

    static long costLoopPid(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) h.invokeExact();
        }
        return sum;
    }

    static long costLoopSeg(MethodHandle h, MemorySegment s, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            long len = (long) h.invokeExact(s);
            if (len != 5) {
                bad = true;
            }
            sum += len;
        }
        return sum;
    }

    interface Body {
        void run(int n) throws Throwable;
    }

    static void row(String name, Body body) throws Throwable {
        for (int w = 0; w < 10; w++) {
            body.run(1000);
        }
        long[] ns = new long[5];
        for (int r = 0; r < ns.length; r++) {
            long t0 = System.nanoTime();
            body.run(N);
            ns[r] = (System.nanoTime() - t0) / N;
        }
        long[] s = ns.clone();
        Arrays.sort(s);
        System.err.println("[r16-ffm-cost2] " + name + " median=" + s[2] + " ns/call");
        System.out.println(name + ": " + (bad ? "WRONG" : "ok"));
    }

    public static void main(String[] args) throws Throwable {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        Arena arena = Arena.ofConfined();
        FunctionDescriptor jj = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG);
        MemorySegment labsAddr = std.find("llabs").orElseThrow();
        MethodHandle mhJava = MethodHandles.lookup().findStatic(R16FfmDowncallCost.class, "javaAbs",
                MethodType.methodType(long.class, long.class));
        MethodHandle pid = windows
                ? linker.downcallHandle(SymbolLookup.libraryLookup("kernel32", arena)
                        .find("GetCurrentProcessId").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT))
                : linker.downcallHandle(std.find("getpid").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT));
        MethodHandle labs = linker.downcallHandle(labsAddr, jj);
        MethodHandle labsCrit = linker.downcallHandle(labsAddr, jj, Linker.Option.critical(false));
        MethodHandle labsErrno = linker.downcallHandle(labsAddr, jj,
                Linker.Option.captureCallState(windows ? "GetLastError" : "errno"));
        MemorySegment state = arena.allocate(Linker.Option.captureStateLayout());
        FunctionDescriptor js = FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.ADDRESS);
        MemorySegment strlenAddr = std.find("strlen").orElseThrow();
        MethodHandle strlen = linker.downcallHandle(strlenAddr, js);
        MethodHandle strlenCrit = linker.downcallHandle(strlenAddr, js, Linker.Option.critical(false));
        MemorySegment five = arena.allocateFrom("hello");

        row("mh-java", n -> costLoopLong(mhJava, n));
        row("pid", n -> costLoopPid(pid, n));
        row("llabs", n -> costLoopLong(labs, n));
        row("llabs-crit", n -> costLoopLong(labsCrit, n));
        row("llabs-errno", n -> costLoopErrno(labsErrno, state, n));
        row("strlen", n -> costLoopSeg(strlen, five, n));
        row("strlen-crit", n -> costLoopSeg(strlenCrit, five, n));
        System.out.println(bad ? "FAIL" : "PASS");
    }
}
