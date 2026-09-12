/**
 * Decomposes CratonVM's per-call floor.
 *
 * Every rung runs the SAME total work two ways:
 *
 *   OSR    — one call whose loop runs `total` times, so the loop can only
 *            reach compiled code through on-stack replacement.
 *   INVOKE — `total/chunk` calls whose loop runs `chunk` times each, so the
 *            method is compiled by invocation count and every call after the
 *            threshold runs compiled from its entry.
 *
 * If a rung is much faster under INVOKE than under OSR, the problem is OSR
 * code quality, not the rung's body. If both are equal and a rung with a call
 * costs far more than the call-free rung, the problem is the call.
 *
 * Usage: CallFloorProbe [totalIterations] [chunk]
 */
public final class CallFloorProbe {

    // ---- bodies -----------------------------------------------------------

    static int staticSink;

    /** Pure ALU: the baseline every other rung is measured against. */
    static long arith(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += i ^ (acc >>> 7); }
        return acc;
    }

    static int addOne(int i) { return i + 1; }

    /** invokestatic to a tiny leaf, otherwise identical to `arith`. */
    static long staticCall(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += addOne(i) ^ (acc >>> 7); }
        return acc;
    }

    static final class Leaf {
        int addOne(int i) { return i + 1; }
    }
    interface Adder { int addOne(int i); }
    static final class IfaceLeaf implements Adder {
        public int addOne(int i) { return i + 1; }
    }

    static final Leaf LEAF = new Leaf();
    static final Adder IFACE = new IfaceLeaf();

    /** invokevirtual on a final class. */
    static long virtualCall(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += LEAF.addOne(i) ^ (acc >>> 7); }
        return acc;
    }

    /** invokeinterface, monomorphic. */
    static long ifaceCall(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += IFACE.addOne(i) ^ (acc >>> 7); }
        return acc;
    }

    /** A JDK call the JIT has no special knowledge of. */
    static final String[] NAMES = { "ISO-8859-1", "ISO-8859-2", "ISO-8859-3" };
    static long jdkCall(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) { acc += NAMES[i % 3].length() ^ (acc >>> 7); }
        return acc;
    }

    // ---- harness ----------------------------------------------------------

    interface Body { long run(int n); String name(); }

    static final Body[] BODIES = {
        new Body() { public long run(int n) { return arith(n); }       public String name() { return "arith (no call)"; } },
        new Body() { public long run(int n) { return staticCall(n); }  public String name() { return "+ invokestatic leaf"; } },
        new Body() { public long run(int n) { return virtualCall(n); } public String name() { return "+ invokevirtual leaf"; } },
        new Body() { public long run(int n) { return ifaceCall(n); }   public String name() { return "+ invokeinterface leaf"; } },
        new Body() { public long run(int n) { return jdkCall(n); }     public String name() { return "+ String.length()"; } },
    };

    static volatile long sink;

    /** One call, `total` iterations — the loop can only be OSR-compiled. */
    static double osr(Body b, int total) {
        long s = System.nanoTime();
        sink += b.run(total);
        return (System.nanoTime() - s) / (double) total;
    }

    /** `total/chunk` calls of `chunk` iterations — compiled by invocation count. */
    static double invoke(Body b, int total, int chunk) {
        int rounds = total / chunk;
        long s = System.nanoTime();
        for (int r = 0; r < rounds; r++) { sink += b.run(chunk); }
        return (System.nanoTime() - s) / (double) (rounds * (long) chunk);
    }

    public static void main(String[] args) {
        int total = args.length > 0 ? Integer.parseInt(args[0]) : 20_000_000;
        int chunk = args.length > 1 ? Integer.parseInt(args[1]) : 2_000;

        // Warm every body both ways before timing anything.
        for (Body b : BODIES) { osr(b, Math.min(total, 200_000)); invoke(b, Math.min(total, 2_000_000), chunk); }

        System.out.printf("%-26s %12s %12s %10s%n", "body", "OSR ns/op", "INVOKE ns/op", "OSR/INVOKE");
        for (Body b : BODIES) {
            double o = osr(b, total);
            double i = invoke(b, total, chunk);
            System.out.printf("%-26s %12.2f %12.2f %9.2fx%n", b.name(), o, i, o / i);
        }
        System.out.println("sink=" + sink);
    }
}
