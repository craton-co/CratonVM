/**
 * The compiled-caller -> interpreted-callee transition, per invoke kind.
 *
 * `XferProbe` measured it for `invokestatic`. Real application and reactive code
 * is overwhelmingly virtual/interface, so this splits the three kinds so a fix
 * scoped to one of them can be shown to move the right one.
 *
 * Usage: XferProbe2 <iters> <arm>      arm = static | virtual | iface
 * Deny the callee with e.g. CRATONVM_JIT_DENY=XferProbe2.calleeVirtual
 */
public class XferProbe2 {
    static long sink;

    interface Op { int apply(int x); }

    static class Impl implements Op {
        public int apply(int x) { return calleeIface(x); }
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

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        String arm = args.length > 1 ? args[1] : "static";
        Impl o = new Impl();
        switch (arm) {
            case "virtual": loopVirtual(50000, o); break;
            case "iface":   loopIface(50000, o);   break;
            default:        loopStatic(50000);     break;
        }
        long t0 = System.nanoTime();
        switch (arm) {
            case "virtual": loopVirtual(n, o); break;
            case "iface":   loopIface(n, o);   break;
            default:        loopStatic(n);     break;
        }
        long d = System.nanoTime() - t0;
        System.out.printf("xfer[%s] %8.1f ns/op sink=%d%n", arm, (double) d / n, sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
