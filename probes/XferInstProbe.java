/**
 * Prices the compiled-caller -> INTERPRETED-callee transition for the three
 * INSTANCE invoke kinds, exactly as `XferProbe` does for `invokestatic`.
 *
 * `XferProbe` could only ever price kind 3, so the fix it drove
 * (`try_jit_static_bytecode_callee`) had no control for the other three and
 * the page it belongs to stayed open on them. Each arm here is the same shape:
 * a hot loop that gets compiled, calling a trivial one-line callee that
 * `CRATONVM_JIT_DENY` keeps interpreted.
 *
 *   virtual   `Impl.add`      reached through a concrete class receiver
 *   iface     `Adder.add`     reached through an interface-typed receiver
 *   special   `Base.add`      reached as a `super.add` call
 *   megamorph `Adder.add`     four receiver classes at one site, so the memo
 *                             has to key on the receiver rather than the site
 *
 * The megamorphic arm is the one that would catch a memo keyed by call site
 * alone: all four receivers answer a DIFFERENT constant, and the printed sums
 * are checked against the arithmetic below, so serving the first receiver's
 * body to the second is a wrong number and not merely a slow one.
 *
 *   XferInstProbe [iterations]
 *
 * Arms, on ONE binary:
 *   (base)                                     both compiled
 *   CRATONVM_JIT_DENY=XferInstProbe$Impl.add,... callee interpreted
 *   --nojit                                     both interpreted
 */
public class XferInstProbe {

    interface Adder {
        int add(int x);
    }

    static class Base implements Adder {
        public int add(int x) { return x + 1; }
    }

    static class Impl extends Base {
        public int add(int x) { return x + 2; }
        int viaSuper(int x) { return super.add(x); }
    }

    static class A2 implements Adder { public int add(int x) { return x + 3; } }
    static class A3 implements Adder { public int add(int x) { return x + 4; } }

    static long sink;

    static void loopVirtual(Impl r, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += r.add(i);
        sink += s;
    }

    static void loopIface(Adder r, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += r.add(i);
        sink += s;
    }

    static void loopSuper(Impl r, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += r.viaSuper(i);
        sink += s;
    }

    static void loopMega(Adder[] rs, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += rs[i & 3].add(i);
        sink += s;
    }

    static long timed(String name, Runnable warm, Runnable hot, int n) {
        warm.run();
        long before = sink;
        long t0 = System.nanoTime();
        hot.run();
        long d = System.nanoTime() - t0;
        long produced = sink - before;
        System.out.printf("%-9s %8.1f ns/op sum=%d%n", name, (double) d / n, produced);
        return produced;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        // The megamorphic arm's expected sum is exact only over whole cycles
        // of its four receivers.
        n -= n % 4;
        Impl impl = new Impl();
        Adder[] rs = { new Base(), new Impl(), new A2(), new A3() };

        long sumN = (long) n * (n - 1) / 2;

        long vv = timed("virtual", () -> loopVirtual(impl, 50000),
                        () -> loopVirtual(impl, n), n);
        long ii = timed("iface", () -> loopIface(impl, 50000),
                        () -> loopIface(impl, n), n);
        long ss = timed("special", () -> loopSuper(impl, 50000),
                        () -> loopSuper(impl, n), n);
        long mm = timed("megamorph", () -> loopMega(rs, 50000),
                        () -> loopMega(rs, n), n);

        // The answers, not just the timings. `virtual` and `iface` both reach
        // `Impl.add` (+2); `special` reaches `Base.add` (+1) through the
        // super-call; `megamorph` cycles +1,+2,+3,+4 over the four receivers,
        // which averages +2.5 and is exact because n is a multiple of 4.
        boolean ok = vv == sumN + 2L * n
                  && ii == sumN + 2L * n
                  && ss == sumN + 1L * n
                  && mm == sumN + (1L + 2L + 3L + 4L) * (n / 4);
        System.out.println("answers " + (ok ? "OK" : "WRONG")
                + " virtual=" + vv + " iface=" + ii
                + " special=" + ss + " mega=" + mm);
        System.out.flush();
        Runtime.getRuntime().halt(ok ? 0 : 1);
    }
}
