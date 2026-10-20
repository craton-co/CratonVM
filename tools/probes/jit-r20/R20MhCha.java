import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

// JIT round 20 (MHIFACE19-2, CRATONVM_JIT_MH_INTERFACE_CHA_SPLICE): an interface constant handle
// site compiled while Fn has ONE loaded implementor splices it by CHA; a second implementor loaded
// afterwards must take the miss edge (the interface call), and a non-implementor still meets the
// handle's ICCE. Answer lines must match HotSpot 25 byte for byte.
public class R20MhCha {
    interface Fn { int apply(int x); }
    static final class First implements Fn { public int apply(int x) { return x + 7; } }
    // Not loaded until phase 2 (no reference before `late()`).
    static final class Late implements Fn { public int apply(int x) { return 3 * x - 1; } }
    static class Base implements Fn { public int apply(int x) { return x ^ 0x55; } }
    static final class Sub extends Base { public int apply(int x) { return x - 100; } }
    static final MethodHandle APPLY;
    static {
        try {
            APPLY = MethodHandles.lookup().findVirtual(Fn.class, "apply",
                    MethodType.methodType(int.class, int.class));
        } catch (ReflectiveOperationException e) { throw new ExceptionInInitializerError(e); }
    }
    static long loop(Fn f, int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) s += (int) APPLY.invokeExact(f, i);
        return s;
    }
    static long mixed(Fn[] fs, int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) s += (int) APPLY.invokeExact(fs[i % fs.length], i);
        return s;
    }
    static Fn late() { return new Late(); }
    static Fn sub() { return new Sub(); }
    public static void main(String[] a) throws Throwable {
        Fn first = new First();
        long s = 0;
        for (int c = 0; c < 3000; c++) s = loop(first, 1000);
        System.out.println("first " + s);
        Fn late = late();
        System.out.println("late " + loop(late, 1000));
        System.out.println("first-again " + loop(first, 1000));
        long m = 0;
        for (int c = 0; c < 500; c++) m = mixed(new Fn[] { first, late }, 1001);
        System.out.println("mixed " + m);
        Fn sub = sub();
        System.out.println("sub " + loop(sub, 1000));
        System.out.println("base " + loop(new Base(), 1000));
        long t = 0;
        for (int c = 0; c < 500; c++) t = mixed(new Fn[] { first, late, sub }, 999);
        System.out.println("tri " + t);
    }
}
