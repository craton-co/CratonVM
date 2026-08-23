/**
 * The compiled-caller -> interpreted-callee transition, per invoke kind.
 *
 * `XferProbe` measured it for `invokestatic`. Real application and reactive code
 * is overwhelmingly virtual/interface, so this splits the three kinds so a fix
 * scoped to one of them can be shown to move the right one.
 *
 * Usage: XferProbe2 <iters> <arm>   arm = static | virtual | iface | iface2 | special
 * Deny the callee with e.g. CRATONVM_JIT_DENY=.calleeVirtual (the deny filter
 * is a SUBSTRING of `Class.method`, and the virtual/interface callees live on
 * `XferProbe2$Impl`, not on `XferProbe2` — a `XferProbe2.calleeVirtual` filter
 * matches nothing and silently measures the undenied arm).
 *
 * `iface` and `iface2` differ ONLY in the SAM's NAME, and that is the point.
 * `apply` is on `jit::helpers::site_name_is_special_cased`, the name-only list
 * `invoke_or_native`'s special cases are deferred to, so the two arms take
 * different routes through the dispatch helper for byte-identical bytecode.
 * `iface2`'s `step` is an ordinary interface method with nothing special about
 * its name — and it turns out to measure nothing, because the compiler
 * devirtualizes and INLINES it outright: the arm reads ~23 ns/op even with the
 * whole of `XferProbe2$Impl` denied, and the dispatch helper is never entered
 * (no `[DISP_CENSUS]` line at all). `apply` is not inlined, which is why
 * `iface` is the arm that actually prices an interface transition. Keep both:
 * the pair is the evidence that the name, not the shape, decides.
 *
 * `special` is a `super.` call, i.e. `invokespecial` — the third kind, whose
 * dispatch must NOT re-target onto the receiver's runtime class.
 */
public class XferProbe2 {
    static long sink;

    interface Op { int apply(int x); }
    interface Op2 { int step(int x); }

    static class Impl implements Op, Op2 {
        public int apply(int x) { return calleeIface(x); }
        public int step(int x) { return calleeIface(x); }
        int calleeVirtual(int x) { return x + 1; }
    }

    static class Base { int calleeSpecial(int x) { return x + 1; } }
    static class Sub extends Base {
        @Override int calleeSpecial(int x) { return super.calleeSpecial(x) + 0; }
    }

    static int calleeStatic(int x) { return x + 1; }
    static int calleeIface(int x) { return x + 1; }

    static void loopStatic(int n) {
        for (int i = 0; i < n; i++) sink += calleeStatic(i);
    }
    static void loopVirtual(int n, Impl o) {
        for (int i = 0; i < n; i++) sink += o.calleeVirtual(i);
    }
    static void loopIface(int n, Op o) {
        for (int i = 0; i < n; i++) sink += o.apply(i);
    }
    static void loopIface2(int n, Op2 o) {
        for (int i = 0; i < n; i++) sink += o.step(i);
    }
    // `Sub.calleeSpecial` compiles to `invokespecial Base.calleeSpecial`, and
    // that is the site being priced: deny `Base.calleeSpecial` and the compiled
    // caller's invokespecial lands on an interpreted callee.
    static void loopSpecial(int n, Sub o) {
        for (int i = 0; i < n; i++) sink += o.calleeSpecial(i);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        String arm = args.length > 1 ? args[1] : "static";
        Impl o = new Impl();
        Sub sub = new Sub();
        switch (arm) {
            case "virtual": loopVirtual(50000, o); break;
            case "iface":   loopIface(50000, o);   break;
            case "iface2":  loopIface2(50000, o);  break;
            case "special": loopSpecial(50000, sub); break;
            default:        loopStatic(50000);     break;
        }
        long t0 = System.nanoTime();
        switch (arm) {
            case "virtual": loopVirtual(n, o); break;
            case "iface":   loopIface(n, o);   break;
            case "iface2":  loopIface2(n, o);  break;
            case "special": loopSpecial(n, sub); break;
            default:        loopStatic(n);     break;
        }
        long d = System.nanoTime() - t0;
        System.out.printf("xfer[%s] %8.1f ns/op sink=%d%n", arm, (double) d / n, sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
