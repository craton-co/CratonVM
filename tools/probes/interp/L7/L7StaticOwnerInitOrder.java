// Interpreter round i1 wave 7, lane L7 -- entering a compiled body from
// `execute` no longer checks (or runs) the static-field owners' initializers.
//
// `f` and `g` reference a static of a class only on a branch the warm-up never
// takes, so they compile (first-call door, background tier or OSR) long before
// that class is initialized. JVMS 5.5 initializes it at the first getstatic
// that EXECUTES: `Holder.<clinit>` must print only when `f(true)` runs, and
// `Bad.<clinit>`'s failure must leave `g` as an ExceptionInInitializerError
// even though `g` holds a `catch (Throwable)` -- its `try` does not cover the
// getstatic. The second touch of `Bad` is a NoClassDefFoundError.
//
// HotSpot 25 prints exactly:
//   f warm 0
//   Holder.<clinit>
//   f(true) 42
//   g warm 20000
//   g(true) java.lang.ExceptionInInitializerError cause java.lang.RuntimeException boom
//   g(true) again java.lang.NoClassDefFoundError
//
// Compare `--compatible` with and without `--nojit`, and with
// CRATONVM_BG_COMPILE=0 (the eager first-call door): identical stdout.
public class L7StaticOwnerInitOrder {
    static class Holder {
        static final int X;

        static {
            System.out.println("Holder.<clinit>");
            X = Integer.parseInt("42");
        }
    }

    static class Bad {
        static int Y = boom();

        static int boom() {
            throw new RuntimeException("boom");
        }
    }

    static int f(boolean b) {
        if (b) {
            return Holder.X;
        }
        return 0;
    }

    static int g(boolean b) {
        int r = 0;
        try {
            r = 1;
        } catch (Throwable t) {
            return -1;
        }
        if (b) {
            r += Bad.Y;
        }
        return r;
    }

    public static void main(String[] a) {
        long sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += f(false);
        }
        System.out.println("f warm " + sum);
        System.out.println("f(true) " + f(true));
        sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += g(false);
        }
        System.out.println("g warm " + sum);
        try {
            System.out.println("g(true) returned " + g(true));
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println("g(true) " + t.getClass().getName() + " cause "
                    + (c == null ? "none" : c.getClass().getName() + " " + c.getMessage()));
        }
        try {
            System.out.println("g(true) again returned " + g(true));
        } catch (Throwable t) {
            System.out.println("g(true) again " + t.getClass().getName());
        }
    }
}
