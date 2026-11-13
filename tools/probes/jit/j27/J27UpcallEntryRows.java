// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Guards the single execute door's cached metadata and compiled lookup.
// java --enable-native-access=ALL-UNNAMED J27UpcallEntryRows
import java.lang.foreign.*;
import java.lang.invoke.*;
import static java.lang.foreign.ValueLayout.*;

public class J27UpcallEntryRows {
    static final Linker L = Linker.nativeLinker();
    static long counter;
    static boolean locked = true;
    static String location;

    static long wide(long a, long b) { return a * 31 + b; }
    static double floating(double a, double b) { return a * 0.5 + b; }
    static synchronized void sync(int x) {
        locked &= Thread.holdsLock(J27UpcallEntryRows.class);
        counter += x;
    }
    static int handles(int x) {
        try {
            if ((x & 1) == 0) throw new IllegalArgumentException("entry-" + x);
            return x + 3;
        } catch (IllegalArgumentException e) {
            if (x == 6000) {
                StackTraceElement f = e.getStackTrace()[0];
                location = f.getClassName() + "/" + f.getMethodName() + "/" + f.getFileName()
                    + "/" + (f.getLineNumber() > 0);
            }
            return -x;
        }
    }
    interface Op { int apply(int x); }
    static class Plus implements Op { public int apply(int x) { return x + 1; } }
    static class Minus implements Op { public int apply(int x) { return x - 1; } }
    static Op op = new Plus();
    static int polymorphic(int x) { return op.apply(x); }

    static MethodHandle call(Arena a, String name, MethodType type, FunctionDescriptor fd)
            throws ReflectiveOperationException {
        MethodHandle target = MethodHandles.lookup().findStatic(J27UpcallEntryRows.class, name, type);
        return L.downcallHandle(L.upcallStub(target, fd, a), fd);
    }
    static void fact(String name, Object value) { System.out.println("fact " + name + " " + value); }

    public static void main(String[] args) throws Throwable {
        try (Arena a = Arena.ofConfined()) {
            MethodHandle wide = call(a, "wide", MethodType.methodType(long.class, long.class, long.class),
                FunctionDescriptor.of(JAVA_LONG, JAVA_LONG, JAVA_LONG));
            MethodHandle floating = call(a, "floating", MethodType.methodType(double.class, double.class, double.class),
                FunctionDescriptor.of(JAVA_DOUBLE, JAVA_DOUBLE, JAVA_DOUBLE));
            MethodHandle sync = call(a, "sync", MethodType.methodType(void.class, int.class),
                FunctionDescriptor.ofVoid(JAVA_INT));
            MethodHandle handles = call(a, "handles", MethodType.methodType(int.class, int.class),
                FunctionDescriptor.of(JAVA_INT, JAVA_INT));
            MethodHandle poly = call(a, "polymorphic", MethodType.methodType(int.class, int.class),
                FunctionDescriptor.of(JAVA_INT, JAVA_INT));
            long longs = 0, ints = 0, polys = 0;
            double doubles = 0;
            for (int i = 0; i <= 6000; i++) {
                longs += (long) wide.invokeExact((long) i, 0x1_0000_0001L);
                doubles += (double) floating.invokeExact((double) i, 0.25);
                sync.invokeExact(i);
                ints += (int) handles.invokeExact(i);
                if (i == 4000) { op = new Minus(); System.gc(); }
                polys += (int) poly.invokeExact(i);
            }
            fact("wide", longs);
            fact("wide-extremes", (long) wide.invokeExact(0L, Long.MIN_VALUE) + "/" + (long) wide.invokeExact(0L, 0L));
            fact("double-specials", Double.isNaN((double) floating.invokeExact(Double.NaN, 1.0)) + "/"
                + Double.doubleToRawLongBits((double) floating.invokeExact(-0.0, -0.0)));
            fact("double", doubles);
            fact("void", counter);
            fact("monitor", locked + "/" + Thread.holdsLock(J27UpcallEntryRows.class));
            fact("handlers", ints);
            fact("source", location);
            fact("polymorphic", polys);
        }
    }
}
