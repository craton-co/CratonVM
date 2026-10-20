// Interpreter round i6 wave 122, lane L2 (page
// `i100-L4-proposal-method-handle-warm-call-directions`, item 2;
// `CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID`, default off).
//
// Warm compiled `invokeExact` sites whose descriptors name an application
// class and have basic types `Invokers$Holder` does not pregenerate (so the
// linker is a spun hidden `LambdaForm$MH`). Each row prints the sum of a warm
// loop, then the cold edges the linker must keep: a wrong exact type
// (`WrongMethodTypeException` and its message), a throwing target (the
// exception type and the first user frames of its trace), and a null first
// argument the target dereferences. Every row must print the same with the
// switch on and off, as on HotSpot.
//
// Run (HotSpot):  java L2W122LinkerByIdUserSite.java (25.0.3: default and
//   -Xint print the block below)
// Run (CratonVM): cratonvm [--jdk-only|--compatible] L2W122LinkerByIdUserSite
//   with and without CRATONVM_JIT_MH_COMPILED_LINK_LINKER_BY_ID=1, also with
//   CRATONVM_JIT_MH_COMPILED_LINK_USER_SITE=1 CRATONVM_MH_LINKER_BASIC_MEMBER=1;
//   JIT and --nojit. CRATONVM_DBG_JITC=1 shows the compiled sites.
//
// HOTSPOT_EXPECTED_BEGIN
// mixed 2054981050000
// wide 3004970750000
// bound 1200000
// wmte java.lang.invoke.WrongMethodTypeException: handle's method type (Box,long,double,float,int,Box)long but found (Box,long,double,float,int)long
// throw java.lang.IllegalStateException boom 5
// throw-frame L2W122LinkerByIdUserSite.mixed
// throw-frame L2W122LinkerByIdUserSite.callMixed
// npe java.lang.NullPointerException
// done
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;

public class L2W122LinkerByIdUserSite {
    static final class Box {
        final long v;

        Box(long v) {
            this.v = v;
        }

        long get(int k) {
            return v + k;
        }
    }

    static long mixed(Box b, long j, double d, float f, int i, Box c) {
        if (i == -5) {
            throw new IllegalStateException("boom 5");
        }
        return b.v + j + (long) d + (long) f + i + c.v;
    }

    static long wide(Box a, long b, long c, double d, double e, Box g, int h) {
        return a.v + b + c + (long) d + (long) e + g.v + h;
    }

    static final MethodHandle MIXED;
    static final MethodHandle WIDE;
    static final MethodHandle GET;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MIXED = l.findStatic(L2W122LinkerByIdUserSite.class, "mixed",
                    MethodType.methodType(long.class, Box.class, long.class, double.class,
                            float.class, int.class, Box.class));
            WIDE = l.findStatic(L2W122LinkerByIdUserSite.class, "wide",
                    MethodType.methodType(long.class, Box.class, long.class, long.class,
                            double.class, double.class, Box.class, int.class));
            GET = l.findVirtual(Box.class, "get", MethodType.methodType(long.class, int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long callMixed(Box b, long j, int i, Box c) throws Throwable {
        return (long) MIXED.invokeExact(b, j, 2.0, 3.0f, i, c);
    }

    static long callWide(Box a, long b, int h) throws Throwable {
        return (long) WIDE.invokeExact(a, b, 1L, 2.0, 3.0, a, h);
    }

    static long callBound(MethodHandle bound, int k) throws Throwable {
        return (long) bound.invokeExact(k);
    }

    public static void main(String[] args) throws Throwable {
        Box one = new Box(1);
        Box ten = new Box(10);
        long acc = 0;
        for (int i = 0; i < 100_000; i++) {
            acc += callMixed(one, i * 410L, i, ten);
        }
        System.out.println("mixed " + acc);
        acc = 0;
        for (int i = 0; i < 100_000; i++) {
            acc += callWide(one, i * 600L, i);
        }
        System.out.println("wide " + acc);
        MethodHandle bound = GET.bindTo(new Box(2));
        acc = 0;
        for (int i = 0; i < 100_000; i++) {
            acc += callBound(bound, 10);
        }
        System.out.println("bound " + acc);
        try {
            long r = (long) MIXED.invokeExact(one, 1L, 2.0, 3.0f, 4);
            System.out.println("wmte none " + r);
        } catch (WrongMethodTypeException e) {
            System.out.println("wmte " + e);
        }
        try {
            callMixed(one, 1L, -5, ten);
            System.out.println("throw none");
        } catch (IllegalStateException e) {
            System.out.println("throw " + e.getClass().getName() + " " + e.getMessage());
            int shown = 0;
            for (StackTraceElement s : e.getStackTrace()) {
                if (s.getClassName().equals("L2W122LinkerByIdUserSite") && shown < 2) {
                    System.out.println("throw-frame " + s.getClassName() + "." + s.getMethodName());
                    shown++;
                }
            }
        }
        try {
            callMixed(null, 1L, 1, ten);
            System.out.println("npe none");
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName());
        }
        System.out.println("done");
    }
}
