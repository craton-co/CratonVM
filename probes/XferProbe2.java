/**
 * The compiled-caller -> interpreted-callee transition, per invoke kind.
 *
 * `XferProbe` measured it for `invokestatic`. Real application and reactive code
 * is overwhelmingly virtual/interface, so this splits the three kinds so a fix
 * scoped to one of them can be shown to move the right one.
 *
 * Usage: XferProbe2 <iters> <arm>
 *   arm = static | virtual | iface | iface2 | special | special1
 *
 * Deny the callee with the class the body actually lives on, e.g.
 * `CRATONVM_JIT_DENY=XferProbe2$Impl.calleeVirtual` for `virtual` and
 * `CRATONVM_JIT_DENY=XferProbe2$Impl.compute` for `iface2`. Denying
 * `XferProbe2.calleeIface` denies a STATIC method and re-measures the
 * `invokestatic` kind, which is why that spelling is not the one to use.
 *
 * `iface` and `iface2` differ ONLY in the SAM's name, and that pair is now a
 * REGRESSION GATE rather than a workaround. `apply` is on
 * `site_name_is_special_cased`'s deliberately over-broad list (it is one of the
 * names `invoke_or_native`'s opening cascade can claim, via the
 * `ToIntFunction.apply` SAM bridges), and deferring to that list made the memo
 * refuse every `apply` site: `out_virtual_bc=0 out_virtual_bc_refused=1048575`
 * on this arm against `1048575 / 0` on `virtual`. Since `apply` is the SAM of
 * `java.util.function.Function` -- every reactive operator that is a CLASS
 * rather than a lambda -- that was a name-wide refusal over exactly the
 * population the fix exists for, so `virtual_site_name_is_special_cased`
 * narrowed it to the rescue's own triple. BOTH arms should now read
 * `out_virtual_bc_refused=0`; `iface` going back to a refusal means the
 * narrowing was lost.
 */
public class XferProbe2 {
    static long sink;

    interface Op { int apply(int x); }
    interface Op2 { int compute(int x); }

    /**
     * A `tableswitch` whose every arm returns the same value: 64 arms is far
     * past the inline planner's cost budget (it planned `DirectBind cost=4
     * budget_left=750` for the one-line version and spliced it, which made
     * `CRATONVM_JIT_DENY` on the callee a no-op), while the switch itself is
     * O(1) so the arm still measures a CALL and not a body. `sink` is
     * bit-identical to every other arm, which is the check that it is.
     */
    static class Base {
        int calleeSpecial(int x) {
            switch (x & 63) {
                case 0: return x + 1;
                case 1: return x + 1;
                case 2: return x + 1;
                case 3: return x + 1;
                case 4: return x + 1;
                case 5: return x + 1;
                case 6: return x + 1;
                case 7: return x + 1;
                case 8: return x + 1;
                case 9: return x + 1;
                case 10: return x + 1;
                case 11: return x + 1;
                case 12: return x + 1;
                case 13: return x + 1;
                case 14: return x + 1;
                case 15: return x + 1;
                case 16: return x + 1;
                case 17: return x + 1;
                case 18: return x + 1;
                case 19: return x + 1;
                case 20: return x + 1;
                case 21: return x + 1;
                case 22: return x + 1;
                case 23: return x + 1;
                case 24: return x + 1;
                case 25: return x + 1;
                case 26: return x + 1;
                case 27: return x + 1;
                case 28: return x + 1;
                case 29: return x + 1;
                case 30: return x + 1;
                case 31: return x + 1;
                case 32: return x + 1;
                case 33: return x + 1;
                case 34: return x + 1;
                case 35: return x + 1;
                case 36: return x + 1;
                case 37: return x + 1;
                case 38: return x + 1;
                case 39: return x + 1;
                case 40: return x + 1;
                case 41: return x + 1;
                case 42: return x + 1;
                case 43: return x + 1;
                case 44: return x + 1;
                case 45: return x + 1;
                case 46: return x + 1;
                case 47: return x + 1;
                case 48: return x + 1;
                case 49: return x + 1;
                case 50: return x + 1;
                case 51: return x + 1;
                case 52: return x + 1;
                case 53: return x + 1;
                case 54: return x + 1;
                case 55: return x + 1;
                case 56: return x + 1;
                case 57: return x + 1;
                case 58: return x + 1;
                case 59: return x + 1;
                case 60: return x + 1;
                case 61: return x + 1;
                case 62: return x + 1;
                case 63: return x + 1;
                default: return x + 1;
            }
        }
    }

    /** `super.calleeSpecial` is an unambiguous `invokespecial`; deny
     *  `XferProbe2$Base.calleeSpecial` to leave the callee interpreted. */
    static class Sub extends Base {
        @Override int calleeSpecial(int x) { return super.calleeSpecial(x); }
    }

    /**
     * The ONE-LINE super-callee, kept as the lever's own witness.
     *
     * `CRATONVM_JIT_DENY=XferProbe2$Base1.calleeSpecial1` on this arm is the
     * test of whether the deny lever means what its doc says. Before
     * 2026-08-23 the planner did not consult it, so `CRATONVM_DBG=jitc`
     * printed `inline-planned XferProbe2$Base1.calleeSpecial1 @pc=2` and the
     * arm read the SAME ns/op denied and undenied -- the tell that the lever
     * was not engaged, and the reason the `special` arm above needed a callee
     * the inliner refuses on size.
     */
    static class Base1 { int calleeSpecial1(int x) { return x + 1; } }
    static class Sub1 extends Base1 {
        @Override int calleeSpecial1(int x) { return super.calleeSpecial1(x); }
    }

    static class Impl implements Op, Op2 {
        public int apply(int x) { return calleeIface(x); }
        public int compute(int x) { return x + 1; }
        int calleeVirtual(int x) { return x + 1; }
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
        for (int i = 0; i < n; i++) sink += o.compute(i);
    }
    static void loopSpecial(int n, Sub s) {
        for (int i = 0; i < n; i++) sink += s.calleeSpecial(i);
    }
    static void loopSpecial1(int n, Sub1 s) {
        for (int i = 0; i < n; i++) sink += s.calleeSpecial1(i);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        String arm = args.length > 1 ? args[1] : "static";
        Impl o = new Impl();
        Sub s = new Sub();
        Sub1 s1 = new Sub1();
        switch (arm) {
            case "virtual": loopVirtual(50000, o); break;
            case "iface":   loopIface(50000, o);   break;
            case "iface2":  loopIface2(50000, o);  break;
            case "special": loopSpecial(50000, s);  break;
            case "special1": loopSpecial1(50000, s1); break;
            default:        loopStatic(50000);     break;
        }
        long t0 = System.nanoTime();
        switch (arm) {
            case "virtual": loopVirtual(n, o); break;
            case "iface":   loopIface(n, o);   break;
            case "iface2":  loopIface2(n, o);  break;
            case "special": loopSpecial(n, s);  break;
            case "special1": loopSpecial1(n, s1); break;
            default:        loopStatic(n);     break;
        }
        long d = System.nanoTime() - t0;
        System.out.printf("xfer[%s] %8.1f ns/op sink=%d%n", arm, (double) d / n, sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
