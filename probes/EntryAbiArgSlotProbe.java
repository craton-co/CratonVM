// Entry-ABI argument-slot probe for the JIT.
//
// A compiled method receives its incoming arguments in the platform's integer
// argument registers (RCX/RDX/R8/R9 on Win64; RDI/RSI/RDX/RCX/R8/R9 on SysV),
// with everything past them on the caller's stack. The single-pass (C1) backend
// loads both halves; the optimizing (C2/IR) backend's prologue reads registers
// ONLY, so `lower()` must refuse any graph with more incoming slots than the
// register file. When that refusal was scoped to `needs_context` methods, every
// LEAF method with more slots was lowered anyway and its stack-passed
// parameters were silently dropped — the local kept whatever the frame slot
// held, i.e. usually null.
//
// Each callee below is `aload <n>; areturn`: it returns one argument unchanged,
// so whatever comes back names the slot the JIT actually handed it. The
// RECEIVER is an incoming slot too, so an instance method with N parameters
// occupies N+1 — which is why `i3` (4 slots) was fine and `i4` (5 slots) was
// not, while the static `s4` (4 slots) was fine and `s5` (5 slots) was not.
//
// Measured on 2026-08-01, Win64, before the fix (300_000 iterations each):
//
//     i1 ok    i2 ok    i3 ok    i4 SIGSEGV   i5 SIGSEGV   i6 bad=299_368
//     s3 ok    s4 ok    s5 SIGSEGV            s6 bad=299_447
//
// Run one shape per process so a single run compiles a single method and the
// answer is unambiguous:
//
//   cratonvm --java-home <jdk> -cp <out> EntryAbiArgSlotProbe i6 300000
//   cratonvm --java-home <jdk> -cp <out> EntryAbiArgSlotProbe all 300000
//
// `all` exercises every shape in one process; prefer it only as a smoke test,
// since a crash in one shape hides the rest.
public class EntryAbiArgSlotProbe {

    static final class Box {
        final String id;
        Box(String id) { this.id = id; }
        @Override public String toString() { return id; }
    }

    static final class R {
        Object i1(Object a)                                                        { return a; }
        Object i2(Object a, Object b)                                              { return b; }
        Object i3(Object a, Object b, Object c)                                    { return c; }
        Object i4(Object a, Object b, Object c, Object d)                          { return d; }
        Object i5(Object a, Object b, Object c, Object d, Object e)                { return e; }
        Object i6(Object a, Object b, Object c, Object d, Object e, Object f)      { return f; }
    }

    static Object s3(Object a, Object b, Object c)                                 { return c; }
    static Object s4(Object a, Object b, Object c, Object d)                       { return d; }
    static Object s5(Object a, Object b, Object c, Object d, Object e)             { return e; }
    static Object s6(Object a, Object b, Object c, Object d, Object e, Object f)   { return f; }

    // The shape Spring Boot's property binding actually runs: an interface
    // default method that returns its last argument. `BindHandler.onSuccess` is
    // `aload 4; areturn` over five slots, so a dropped argument makes the binder
    // discard every bound value and `BindResult.isBound()` answer false for
    // every property.
    interface Handler {
        default Object onSuccess(Object name, Object target, Object context, Object result) {
            return result;
        }
    }

    static final class PlainHandler implements Handler {
    }

    static long run(String which, int iters) {
        R r = new R();
        Handler h = new PlainHandler();
        long bad = 0;
        String first = null;

        for (int n = 0; n < iters; n++) {
            Box a = new Box("a"), b = new Box("b"), c = new Box("c");
            Box d = new Box("d"), e = new Box("e"), f = new Box("f");
            Object got;
            Object want;
            switch (which) {
                case "i1":      got = r.i1(a);                          want = a; break;
                case "i2":      got = r.i2(a, b);                       want = b; break;
                case "i3":      got = r.i3(a, b, c);                    want = c; break;
                case "i4":      got = r.i4(a, b, c, d);                 want = d; break;
                case "i5":      got = r.i5(a, b, c, d, e);              want = e; break;
                case "i6":      got = r.i6(a, b, c, d, e, f);           want = f; break;
                case "s3":      got = s3(a, b, c);                      want = c; break;
                case "s4":      got = s4(a, b, c, d);                   want = d; break;
                case "s5":      got = s5(a, b, c, d, e);                want = e; break;
                case "s6":      got = s6(a, b, c, d, e, f);             want = f; break;
                case "onSuccess": got = h.onSuccess(a, b, c, d);        want = d; break;
                default: throw new IllegalArgumentException("unknown shape: " + which);
            }
            if (got != want) {
                bad++;
                if (first == null) {
                    String g;
                    try {
                        g = String.valueOf(got);
                    } catch (Throwable t) {
                        g = "<unprintable " + t + ">";
                    }
                    first = "n=" + n + " want=" + want + " got=" + g;
                }
            }
        }
        System.out.println(which + ": bad=" + bad + "/" + iters
                + (first == null ? "" : "  first: " + first));
        return bad;
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 300_000;

        String[] shapes = which.equals("all")
                ? new String[] { "i1", "i2", "i3", "i4", "i5", "i6",
                                 "s3", "s4", "s5", "s6", "onSuccess" }
                : new String[] { which };

        long bad = 0;
        for (String shape : shapes) {
            bad += run(shape, iters);
        }
        System.out.println(bad == 0
                ? "PASS EntryAbiArgSlotProbe"
                : "FAIL EntryAbiArgSlotProbe (" + bad + " wrong results)");
        System.exit(bad == 0 ? 0 : 1);
    }
}
