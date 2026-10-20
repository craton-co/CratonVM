// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 12 (lane ffmdoor): correctness of the JIT's bound downcall
// row (CRATONVM_JIT_FFM_DOWNCALL_BIND, FFC16-1) against HotSpot. Every row is a
// compiled loop in a method called from main (CRATONVM_DBG_JITC=1 shows loop*
// compiled; CRATONVM_DBG_FFM=1 shows "[ffm-bind] site ... bound=" per fill).
// Rows: a static final handle; one site alternating two handles (the bind's
// churn); a loop allocating enough to collect (the bind re-validates per
// epoch); an asType copy at its own type (refused: the door converts); a
// wrong-type invokeExact (refused: the door's WrongMethodTypeException); a
// void and a no-argument handle; narrow returns. stdout: facts only.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;

public class R16FfmdoorBind {
    static final int N = 20_000;
    static final Linker LINKER = Linker.nativeLinker();
    static final SymbolLookup STD = LINKER.defaultLookup();
    static final FunctionDescriptor JJ =
            FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_LONG);
    static final MethodHandle LLABS = LINKER.downcallHandle(STD.find("llabs").orElseThrow(), JJ);
    static volatile Object sink;

    static int plainIncrement(int value) { return value + 1; }

    static long loopConst(int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) LLABS.invokeExact((long) -i);
        }
        return sum;
    }

    static long loopAlternate(MethodHandle a, MethodHandle b, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            MethodHandle h = (i & 1) == 0 ? a : b;
            sum += (long) h.invokeExact((long) -i);
        }
        return sum;
    }

    static long loopGc(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sink = new byte[4096];
            sum += (long) h.invokeExact((long) -i * 3);
        }
        return sum;
    }

    static long loopIntToLong(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (long) h.invokeExact(-i);
        }
        return sum;
    }

    static int loopWrongType(MethodHandle h, int n) throws Throwable {
        int wrong = 0;
        for (int i = 0; i < n; i++) {
            try {
                sum0 += (int) h.invokeExact(-i);
            } catch (WrongMethodTypeException e) {
                wrong++;
            }
        }
        return wrong;
    }

    static long sum0;

    static long loopVoid(MethodHandle h, MemorySegment unused, int n) throws Throwable {
        long calls = 0;
        for (int i = 0; i < n; i++) {
            h.invokeExact((long) i);
            calls++;
        }
        return calls;
    }

    static long loopNoArg(MethodHandle h, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) h.invokeExact() == 0 ? 0 : 1;
        }
        return sum;
    }

    static long loopNarrow(MethodHandle toupper, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) toupper.invokeExact('a' + (i % 26));
        }
        return sum;
    }

    static long loopShort(MethodHandle abs16, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (short) abs16.invokeExact(-(i & 0x3fff));
        }
        return sum;
    }

    static long loopByte(MethodHandle upper8, int n) throws Throwable {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (byte) upper8.invokeExact('a' + (i % 26));
        }
        return sum;
    }

    interface Body {
        Object run(int n) throws Throwable;
    }

    static void row(String name, Body body) {
        Object last = null;
        try {
            for (int w = 0; w < 10; w++) {
                last = body.run(N);
            }
        } catch (Throwable t) {
            System.out.println(name + ": threw " + t.getClass().getName());
            return;
        }
        System.out.println(name + ": " + last);
    }

    public static void main(String[] args) throws Throwable {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        Arena arena = Arena.ofConfined();
        MemorySegment labsAddr = STD.find("llabs").orElseThrow();
        MethodHandle labs = LINKER.downcallHandle(labsAddr, JJ);
        MethodHandle labsCrit = LINKER.downcallHandle(labsAddr, JJ, Linker.Option.critical(false));
        MethodHandle abs = LINKER.downcallHandle(STD.find("abs").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        MethodHandle absToLong = abs.asType(MethodType.methodType(long.class, int.class));
        MethodHandle toupper = LINKER.downcallHandle(STD.find("toupper").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        // abs on a C int with a Java short return layout (the int argument
        // keeps the call ABI-clean: a short ARGUMENT to an int callee leaves
        // the register's upper bits unspecified).
        MethodHandle abs16 = LINKER.downcallHandle(STD.find("abs").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_SHORT, ValueLayout.JAVA_INT));
        MethodHandle upper8 = LINKER.downcallHandle(STD.find("toupper").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_INT));
        // srand(unsigned) as a void (long) call: a void, one-argument handle.
        MethodHandle srand = LINKER.downcallHandle(STD.find("srand").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        MethodHandle srandLong = LINKER.downcallHandle(STD.find("srand").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.JAVA_LONG));
        MethodHandle pid = windows
                ? LINKER.downcallHandle(SymbolLookup.libraryLookup("kernel32", arena)
                        .find("GetCurrentProcessId").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT))
                : LINKER.downcallHandle(STD.find("getpid").orElseThrow(), FunctionDescriptor.of(ValueLayout.JAVA_INT));

        row("const", n -> loopConst(n));
        row("alternate", n -> loopAlternate(labs, labsCrit, n));
        row("alternate-swap", n -> loopAlternate(labsCrit, LLABS, n));
        row("gc", n -> loopGc(labs, n));
        row("astype-copy", n -> loopIntToLong(absToLong, n));
        row("wrong-type", n -> loopWrongType(labs, n));
        row("void", n -> loopVoid(srandLong, null, n));
        row("no-arg", n -> loopNoArg(pid, n));
        row("narrow-int", n -> loopNarrow(toupper, n));
        row("narrow-short", n -> loopShort(abs16, n));
        row("narrow-byte", n -> loopByte(upper8, n));
        System.out.println("srand-int type: " + srand.type());
        System.out.println("sum0: " + sum0);
        MethodHandle plain = MethodHandles.lookup().findStatic(
                R16FfmdoorBind.class, "plainIncrement",
                MethodType.methodType(int.class, int.class));
        MethodHandle plainAdapt = plain.asType(MethodType.methodType(long.class, short.class));
        System.out.println("real-astype: " + (long) plainAdapt.invokeExact((short) 3));
    }
}
