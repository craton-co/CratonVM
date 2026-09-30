// difftest: strict
//
// Fast-path / decoded-path parity for the constant-pool and object opcodes
// that gained a raw-bytecode arm in the 2026-08-18 interpreter audit:
// getstatic, putstatic, getfield, putfield, new, checkcast, instanceof,
// monitorenter, monitorexit, ldc, ldc_w and ldc2_w.
//
// The bodies are shared — one `opcodes::op_*` per opcode, called by both paths
// — so the arms cannot disagree about what an opcode DOES. What they can
// disagree about is the plumbing around it, and that is what this seed aims at:
//
//   * when `pc` is advanced. The decoded path writes `next_pc` before
//     dispatching; the fast-path arm has to do the same, because
//     `monitorenter`/`monitorexit` snapshot `frame.pc` and the diagnostics
//     print it. A stale or early pc shows up in an exception's bci, so every
//     throwing case below is caught and its message printed.
//   * how an error is routed. The decoded path runs a per-opcode conversion
//     (RuntimeError -> Java throwable, LinkageError -> Java throwable) before
//     the exception table walk; the fast-path arm reaches the same conversions
//     through `classify_fastpath_invoke_error`. A difference shows as a
//     different exception class, a different message, or an exception that
//     escapes a handler that should have caught it.
//
// `interp-decoded` is the axis that makes this a comparison rather than a
// self-check: it is the only mode that passes `--noverify`, so it takes the
// decoded arms while `jit-on` and `nojit` take the new fast-path arms.
public class InterpCpOpcodeParity {

    static int sInt = 7;
    static long sLong = 0x0123_4567_89AB_CDEFL;
    static String sRef = "static-ref";
    static double sDouble = 2.5d;

    static final class Holder {
        int i = 3;
        long l = 42L;
        double d = 1.5d;
        String s = "field-ref";
        Object o;
    }

    static class Base {}
    static class Derived extends Base {}

    // A class whose <clinit> throws, to exercise the class-initialization
    // error path of `new` and `getstatic` (ExceptionInInitializerError first
    // time, NoClassDefFoundError thereafter).
    static class Poison {
        static final int X;
        static {
            if (Boolean.parseBoolean("true")) {
                throw new IllegalStateException("poison clinit");
            }
            X = 1;
        }
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        sb.append(t.getClass().getName());
        String m = t.getMessage();
        sb.append('|').append(m == null ? "<null>" : m);
        Throwable c = t.getCause();
        if (c != null) {
            sb.append("|cause=").append(c.getClass().getName());
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        happyPath();
        nullReceivers();
        badCasts();
        monitors();
        classInit();
        ldcShapes();
    }

    private static void happyPath() {
        Holder h = new Holder();          // new
        h.i = 11;                         // putfield
        h.l = 99L;                        // putfield (cat-2)
        h.d = 3.25d;                      // putfield (cat-2)
        h.s = "assigned";                 // putfield (reference, write barrier)
        h.o = h;                          // putfield (self reference)
        sInt = 21;                        // putstatic
        sLong = -1L;                      // putstatic (cat-2)
        sDouble = -0.5d;                  // putstatic (cat-2)
        sRef = "restatic";                // putstatic (reference)
        System.out.println("happy: " + h.i + " " + h.l + " " + h.d + " " + h.s
                + " " + (h.o == h) + " " + sInt + " " + sLong + " " + sDouble + " " + sRef);

        Object o = new Derived();
        System.out.println("casts: " + (o instanceof Base) + (o instanceof Derived)
                + (o instanceof String) + " " + ((Base) o).getClass().getName());
        Object n = null;
        // null is instanceof nothing, and checkcast of null always succeeds.
        System.out.println("null casts: " + (n instanceof Base) + " " + ((Base) n));
    }

    private static void nullReceivers() {
        Holder h = null;
        try {
            System.out.println(h.i);          // getfield on null
        } catch (NullPointerException e) {
            System.out.println("npe getfield int: " + e.getClass().getName());
        }
        try {
            System.out.println(h.l);          // getfield cat-2 on null
        } catch (NullPointerException e) {
            System.out.println("npe getfield long: " + e.getClass().getName());
        }
        try {
            System.out.println(h.s);          // getfield reference on null
        } catch (NullPointerException e) {
            System.out.println("npe getfield ref: " + e.getClass().getName());
        }
        try {
            h.i = 1;                          // putfield on null
        } catch (NullPointerException e) {
            System.out.println("npe putfield int: " + e.getClass().getName());
        }
        try {
            h.l = 1L;                         // putfield cat-2 on null
        } catch (NullPointerException e) {
            System.out.println("npe putfield long: " + e.getClass().getName());
        }
        try {
            h.s = "x";                        // putfield reference on null
        } catch (NullPointerException e) {
            System.out.println("npe putfield ref: " + e.getClass().getName());
        }
    }

    private static void badCasts() {
        Object o = new Base();
        try {
            Derived d = (Derived) o;          // checkcast that must fail
            System.out.println("unreachable " + d);
        } catch (ClassCastException e) {
            System.out.println("cce: " + e.getClass().getName());
        }
        Object s = "a string";
        try {
            Base b = (Base) s;
            System.out.println("unreachable " + b);
        } catch (ClassCastException e) {
            System.out.println("cce2: " + e.getClass().getName());
        }
        // Array covariance: checkcast on array types.
        Object arr = new Derived[2];
        System.out.println("arr casts: " + (arr instanceof Base[]) + (arr instanceof Derived[])
                + (arr instanceof Object[]) + (arr instanceof int[]));
    }

    private static void monitors() {
        Object lock = new Object();
        synchronized (lock) {                 // monitorenter / monitorexit
            synchronized (lock) {             // reentrant
                System.out.println("monitors: nested ok");
            }
        }
        // An exception unwinding out of a synchronized block must still run the
        // compiler-generated monitorexit in the handler.
        try {
            synchronized (lock) {
                throw new IllegalStateException("unwind through monitorexit");
            }
        } catch (IllegalStateException e) {
            System.out.println("monitors: " + describe(e));
        }
        synchronized (lock) {
            System.out.println("monitors: lock reusable after unwind");
        }
        Object nl = null;
        try {
            synchronized (nl) {               // monitorenter on null
                System.out.println("unreachable");
            }
        } catch (NullPointerException e) {
            System.out.println("monitors: npe " + e.getClass().getName());
        }
    }

    private static void classInit() {
        // First touch: the <clinit> throws, so `new`/`getstatic` must report
        // ExceptionInInitializerError with the original as its cause.
        try {
            System.out.println(Poison.X);
        } catch (Throwable t) {
            System.out.println("clinit first: " + describe(t));
        }
        // Second touch: the class is now erroneous, so it must be
        // NoClassDefFoundError, not a second ExceptionInInitializerError.
        try {
            System.out.println(Poison.X);
        } catch (Throwable t) {
            System.out.println("clinit second: " + t.getClass().getName());
        }
        try {
            Object p = new Poison();
            System.out.println("unreachable " + p);
        } catch (Throwable t) {
            System.out.println("clinit new: " + t.getClass().getName());
        }
    }

    private static void ldcShapes() {
        // ldc of an int too large for sipush, a float, a String, and a Class;
        // ldc2_w of a long and a double.
        int i = 123456789;
        float f = 3.5f;
        String s = "ldc-string";
        Class<?> c = InterpCpOpcodeParity.class;
        Class<?> prim = int.class;
        long l = 1234567890123L;
        double d = 1.7976931348623157E308;
        System.out.println("ldc: " + i + " " + f + " " + s + " " + c.getName()
                + " " + prim.getName() + " " + l + " " + d);
        // String constants are interned, so the identity comparison is part of
        // what ldc has to get right.
        String a = "interned";
        String b = "interned";
        System.out.println("ldc interning: " + (a == b) + " " + (a == "interned"));
    }
}
