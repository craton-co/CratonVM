// j27 wave 7, lane T1L2 (book jit-r23-w1-L2-proposals.md, L2W4B-1): lambda
// forwarders that load their implementation handle as class data (an `ldc`
// of a CONSTANT_Dynamic, `InnerClassLambdaMetafactory.useImplMethodHandle`),
// built by calling LambdaMetafactory.metafactory directly. The opt-in
// `CRATONVM_JIT_IR_LDC_CONDY_SLOT=1 CRATONVM_JIT_MH_CONSTANT_CONDY=1` pair binds
// the forwarder's `invokeExact` as a plain call of the handle's member.
//
// Rows:
//  * special-final: a `findSpecial` handle (special caller = the declaring
//    class Base) of a FINAL method, called through a subclass receiver
//    (REF_invokeSpecial keeps useImplMethodHandle on; the bind admits it
//    because virtual selection cannot pick another method). j27 wave 9: the
//    special caller was P, which HotSpot rejects with WrongMethodTypeException.
//  * iface-static: a `findStatic` handle of an interface's static method.
//  * special-null: the special-final lambda called with a null receiver
//    after warm-up (the bound call's null guard must re-run invokeExact and
//    raise the handle's own exception class).
//
// Expected HotSpot 25 output (worked out from the arithmetic below; N = 2^18):
//   fact special-final 2752512      (sum over i<N of 7 + (i & 7) = 7N + 28 N/8)
//   fact iface-static 268173312     (256 blocks of sum 2k, k<1024 = 1047552)
//   fact special-null java.lang.NullPointerException
//   fact special-after-null 10
// Timing lines (`TIME <row> <ns/op>`) only with -Dj27.time=true.
//
// Engagement on CratonVM: CRATONVM_DBG=jit-mh-bind prints one
// `[mh-bind] compile ... condy cp#<n> -> <member> kind <k>` line per bound site
// (or `refused ...`). Whether a row reaches the condy route at all depends on
// the JDK's useImplMethodHandle choice; the facts are the same either way.
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class J27T1L2CondyForwarderRows {
    static final int N = 1 << 18;

    interface IntOf {
        int of(P p);
    }

    interface IntOp {
        int op(int x);
    }

    interface Ops {
        static int twice(int x) {
            return 2 * x;
        }
    }

    static class Base {
        int bump;

        final int fin() {
            return 7 + bump;
        }

        // The special caller is Base itself: the handle's type is (Base)int, the
        // type the JDK's class-data forwarder uses for its invokeExact. (A
        // special caller P gives (P)int and HotSpot throws
        // WrongMethodTypeException at the first call.) fin is not private, so
        // the metafactory keeps REF_invokeSpecial and useImplMethodHandle.
        static IntOf specialFinal() throws Throwable {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodType type = MethodType.methodType(int.class);
            MethodHandle impl = lookup.findSpecial(Base.class, "fin", type, Base.class);
            MethodType sam = MethodType.methodType(int.class, P.class);
            CallSite cs = LambdaMetafactory.metafactory(
                    lookup, "of", MethodType.methodType(IntOf.class), sam, impl, sam);
            return (IntOf) cs.getTarget().invoke();
        }
    }

    static class P extends Base {
    }

    static IntOp ifaceStatic() throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType type = MethodType.methodType(int.class, int.class);
        MethodHandle impl = lookup.findStatic(Ops.class, "twice", type);
        CallSite cs = LambdaMetafactory.metafactory(
                lookup, "op", MethodType.methodType(IntOp.class), type, impl, type);
        return (IntOp) cs.getTarget().invoke();
    }

    static long runSpecial(IntOf f, P p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            p.bump = i & 7;
            sum += f.of(p);
        }
        return sum;
    }

    static long runStatic(IntOp f) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += f.op(i & 1023);
        }
        return sum;
    }

    static String callNull(IntOf f) {
        try {
            return "no-exception " + f.of(null);
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Throwable {
        boolean time = Boolean.getBoolean("j27.time");
        IntOf special = Base.specialFinal();
        IntOp stat = ifaceStatic();
        P p = new P();
        long a = 0;
        long b = 0;
        int reps = time ? 5 : 3;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            a = runSpecial(special, p);
            long t1 = System.nanoTime();
            b = runStatic(stat);
            long t2 = System.nanoTime();
            if (time) {
                System.out.println("TIME special-final " + (t1 - t0) / N);
                System.out.println("TIME iface-static " + (t2 - t1) / N);
            }
        }
        System.out.println("fact special-final " + a);
        System.out.println("fact iface-static " + b);
        System.out.println("fact special-null " + callNull(special));
        p.bump = 3;
        System.out.println("fact special-after-null " + special.of(p));
    }
}
