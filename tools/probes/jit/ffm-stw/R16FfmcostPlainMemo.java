// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 11 (lane ffmcost): correctness of the plain-downcall memo
// (CRATONVM_FFM_DOWNCALL_PLAIN_MEMO) against HotSpot. Every row runs warm in a
// method called from main (compiled; CRATONVM_DBG_JITC=1 shows warm* compiled,
// CRATONVM_DBG_FFM=1 shows "[ffm-plain] memo"/"[ffm-plain] hit" per handle).
// stdout: facts only. stderr: one timing line, "[r16-ffmcost] llabs <n> ns".
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;

public class R16FfmcostPlainMemo {
    static final int N = 20_000;

    static long warmLlabs(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) h.invokeExact((long) -i);
        }
        return sum;
    }

    static long warmAbs(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) h.invokeExact(-i);
        }
        return sum;
    }

    static double warmSqrt(MethodHandle h, int n) throws Throwable {
        double sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (double) h.invokeExact((double) i);
        }
        return sum;
    }

    static double warmSqrtf(MethodHandle h, int n) throws Throwable {
        double sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (float) h.invokeExact((float) (i & 1023));
        }
        return sum;
    }

    static double warmLdexp(MethodHandle h, int n) throws Throwable {
        double sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (double) h.invokeExact(1.5, i & 15);
        }
        return sum;
    }

    static double warmAtan2(MethodHandle h, int n) throws Throwable {
        double sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (double) h.invokeExact((double) (i & 7) - 3.0, 2.0);
        }
        return sum;
    }

    static long warmUpperChar(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (char) h.invokeExact('a' + (i % 26));
        }
        return sum;
    }

    static long warmUpperByte(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (byte) h.invokeExact(0xC8 + (i & 3));
        }
        return sum;
    }

    static long warmUpperShort(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (short) h.invokeExact((short) ('a' + (i % 26)));
        }
        return sum;
    }

    static long warmRand(MethodHandle srand, MethodHandle rand, int n) throws Throwable {
        srand.invokeExact(42);
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) rand.invokeExact();
        }
        return sum;
    }

    static long warmPid(MethodHandle h, int n) throws Throwable {
        int first = (int) h.invokeExact();
        long same = 0;
        for (int i = 0; i < n; i++) {
            if ((int) h.invokeExact() == first) {
                same++;
            }
        }
        return first > 0 ? same : -1;
    }

    static int compare(MemorySegment a, MemorySegment b) {
        return Integer.compare(a.get(ValueLayout.JAVA_INT, 0), b.get(ValueLayout.JAVA_INT, 0));
    }

    public static void main(String[] args) throws Throwable {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        Arena arena = Arena.ofConfined();
        ValueLayout.OfLong J = ValueLayout.JAVA_LONG;
        ValueLayout.OfInt I = ValueLayout.JAVA_INT;
        ValueLayout.OfDouble D = ValueLayout.JAVA_DOUBLE;
        ValueLayout.OfFloat F = ValueLayout.JAVA_FLOAT;

        MethodHandle llabs = linker.downcallHandle(std.find("llabs").orElseThrow(), FunctionDescriptor.of(J, J));
        MethodHandle abs = linker.downcallHandle(std.find("abs").orElseThrow(), FunctionDescriptor.of(I, I));
        MethodHandle absCrit = linker.downcallHandle(std.find("abs").orElseThrow(), FunctionDescriptor.of(I, I),
                Linker.Option.critical(false));
        MethodHandle sqrt = linker.downcallHandle(std.find("sqrt").orElseThrow(), FunctionDescriptor.of(D, D));
        MethodHandle sqrtf = linker.downcallHandle(std.find("sqrtf").orElseThrow(), FunctionDescriptor.of(F, F));
        MethodHandle ldexp = linker.downcallHandle(std.find("ldexp").orElseThrow(), FunctionDescriptor.of(D, D, I));
        MethodHandle atan2 = linker.downcallHandle(std.find("atan2").orElseThrow(), FunctionDescriptor.of(D, D, D));
        MemorySegment toupperAddr = std.find("toupper").orElseThrow();
        MethodHandle upperChar = linker.downcallHandle(toupperAddr, FunctionDescriptor.of(ValueLayout.JAVA_CHAR, I));
        MethodHandle upperByte = linker.downcallHandle(toupperAddr, FunctionDescriptor.of(ValueLayout.JAVA_BYTE, I));
        MethodHandle upperShort = linker.downcallHandle(toupperAddr,
                FunctionDescriptor.of(ValueLayout.JAVA_SHORT, ValueLayout.JAVA_SHORT));
        MethodHandle srand = linker.downcallHandle(std.find("srand").orElseThrow(), FunctionDescriptor.ofVoid(I));
        MethodHandle rand = linker.downcallHandle(std.find("rand").orElseThrow(), FunctionDescriptor.of(I));
        MethodHandle pid = windows
                ? linker.downcallHandle(SymbolLookup.libraryLookup("kernel32", arena)
                        .find("GetCurrentProcessId").orElseThrow(), FunctionDescriptor.of(I))
                : linker.downcallHandle(std.find("getpid").orElseThrow(), FunctionDescriptor.of(I));

        for (int w = 0; w < 10; w++) {
            warmLlabs(llabs, 1000);
        }
        long t0 = System.nanoTime();
        long s = warmLlabs(llabs, N);
        long per = (System.nanoTime() - t0) / N;
        System.err.println("[r16-ffmcost] llabs " + per + " ns");
        System.out.println("llabs " + s + " one=" + (long) llabs.invokeExact(Long.MIN_VALUE + 1));
        warmAbs(abs, 1000);
        System.out.println("abs " + warmAbs(abs, N) + " crit " + warmAbs(absCrit, N));
        warmSqrt(sqrt, 1000);
        System.out.println("sqrt " + warmSqrt(sqrt, N));
        warmSqrtf(sqrtf, 1000);
        System.out.println("sqrtf " + warmSqrtf(sqrtf, N));
        warmLdexp(ldexp, 1000);
        System.out.println("ldexp " + warmLdexp(ldexp, N));
        warmAtan2(atan2, 1000);
        System.out.println("atan2 " + warmAtan2(atan2, N));
        warmUpperChar(upperChar, 1000);
        System.out.println("toupper-char " + warmUpperChar(upperChar, N) + " "
                + (char) upperChar.invokeExact((int) 'q'));
        warmUpperByte(upperByte, 1000);
        System.out.println("toupper-byte " + warmUpperByte(upperByte, N));
        warmUpperShort(upperShort, 1000);
        System.out.println("toupper-short " + warmUpperShort(upperShort, N));
        warmRand(srand, rand, 1000);
        long r1 = warmRand(srand, rand, 100);
        long r2 = warmRand(srand, rand, 100);
        System.out.println("rand-repeatable " + (r1 == r2));
        System.out.println("pid-stable " + (warmPid(pid, N) == N));

        // Not-exact entry points: the boxing adapters reach the same handle.
        try {
            System.out.println("invoke " + abs.invoke(Integer.valueOf(-4)) + " " + llabs.invoke(-9L));
        } catch (Throwable e) {
            System.out.println("invoke threw " + e.getClass().getName());
        }
        try {
            System.out.println("invokeWithArguments " + atan2.invokeWithArguments(0.0, -1.0));
        } catch (Throwable e) {
            System.out.println("invokeWithArguments threw " + e.getClass().getName());
        }
        try {
            MethodHandle widened = abs.asType(MethodType.methodType(long.class, int.class));
            System.out.println("asType " + (long) widened.invokeExact(-6));
        } catch (Throwable e) {
            System.out.println("asType threw " + e.getClass().getName());
        }
        try {
            int bad = (int) abs.invokeExact(5L);
            System.out.println("wrong-type no exception " + bad);
        } catch (WrongMethodTypeException e) {
            System.out.println("wrong-type " + e.getClass().getName());
        }

        // Segment handles stay on the general path: scope checks unchanged.
        MethodHandle strlen = linker.downcallHandle(std.find("strlen").orElseThrow(),
                FunctionDescriptor.of(J, ValueLayout.ADDRESS));
        Arena closed = Arena.ofConfined();
        MemorySegment hello = closed.allocateFrom("hello");
        System.out.println("strlen " + (long) strlen.invokeExact(hello));
        closed.close();
        try {
            long n = (long) strlen.invokeExact(hello);
            System.out.println("closed no exception " + n);
        } catch (IllegalStateException e) {
            System.out.println("closed " + e.getClass().getName() + ": " + e.getMessage());
        }

        // An upcall from C through a downcall: the active-context guard.
        MethodHandle cmp = MethodHandles.lookup().findStatic(R16FfmcostPlainMemo.class, "compare",
                MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        AddressLayout P = ValueLayout.ADDRESS.withTargetLayout(I);
        MemorySegment stub = linker.upcallStub(cmp, FunctionDescriptor.of(I, P, P), arena);
        MethodHandle qsort = linker.downcallHandle(std.find("qsort").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, J, J, ValueLayout.ADDRESS));
        MemorySegment arr = arena.allocateFrom(I, 5, 3, 9, 1, 7);
        for (int k = 0; k < 3; k++) {
            qsort.invokeExact(arr, 5L, 4L, stub);
        }
        System.out.println("qsort " + java.util.Arrays.toString(arr.toArray(I)));
        System.out.println("done");
    }
}
