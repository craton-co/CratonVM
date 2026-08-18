/**
 * Does RBC.6's exemption of `new` (0xbb) and `athrow` (0xbf) preserve handler
 * locals?
 *
 * The sibling of `Rbc6FieldProbe.java`, for the two opcodes admitted on
 * 2026-08-17. Every method here has the shape RBC.6 exists to protect:
 *
 *   - a protected range containing an ALLOCATION or an explicit THROW,
 *   - a local written INSIDE the try that is NOT a parameter slot,
 *   - a handler that READS that local.
 *
 * Unlike the field probe, these two opcodes did NOT already publish a precise
 * exceptional frame — `emit_post_alloc_oom_check` branched to the shared
 * sentinel-only stub and the `athrow` lowering ran the epilogue directly. Both
 * grew a reason-9 exit in the same change that admitted them, so this probe is
 * the acceptance test for that codegen, not a bookkeeping check.
 *
 * The failure it looks for is a SILENT WRONG ANSWER: a handler reading its
 * local back as 0/null. Compare against HotSpot and against CratonVM --nojit;
 * a fault is not the signature.
 *
 * `throwNewInTry` is the netty shape verbatim —
 * `AdaptivePoolingAllocator$Magazine.allocate` carries
 * `new`/`dup`/`invokespecial`/`athrow` at pc 338-345 inside the range [319,383)
 * and is the method whose refusal opened this work.
 */
public final class Rbc6AllocThrowProbe {

    static final class Box {
        final int v;
        Box(int v) { this.v = v; }
    }

    static final class Marked extends RuntimeException {
        final int mark;
        Marked(int mark) { super(null, null, false, false); this.mark = mark; }
    }

    /**
     * `new` (0xbb) inside a protected range, reached on the common path, with an
     * explicit `throw new` on the rare one. This is the netty sequence.
     */
    static int throwNewInTry(int n, boolean fail) {
        int scratch = 0;
        Object held = null;
        try {
            scratch = n * 7 + 3;          // non-parameter local, written in the try
            held = new Box(n);            // 0xbb on the common path
            if (fail) {
                throw new Marked(n + 1);  // 0xbb / dup / invokespecial / 0xbf
            }
            return ((Box) held).v + scratch;
        } catch (Marked e) {
            // Reads BOTH a local written before the allocation and one written
            // by it. A params-only frame gives 0 and null here.
            return scratch * 1000 + e.mark + (held == null ? 0 : 7);
        }
    }

    /** An allocation whose result is the only thing the handler can report. */
    static int allocThenThrow(int n, boolean fail) {
        Box b = null;
        try {
            b = new Box(n * 13 + 1);
            if (fail) {
                throw new Marked(2);
            }
            return b.v;
        } catch (Marked e) {
            return b == null ? -1 : b.v;
        }
    }

    /** A rethrow: `athrow` of a local the handler of an OUTER try must see. */
    static int rethrowInTry(int n, boolean fail) {
        int outer = 0;
        try {
            outer = n + 100;
            try {
                if (fail) {
                    throw new Marked(n);
                }
                return outer;
            } catch (Marked e) {
                int inner = outer + 5;    // non-parameter local in the inner try
                throw new Marked(inner);  // 0xbf on an existing reference
            }
        } catch (Marked e) {
            return outer * 1000 + e.mark;
        }
    }

    /** A `finally`, i.e. a catch-all whose bci match has nothing else to go on. */
    static int allocInTryWithFinally(int n, boolean fail) {
        int tally = 0;
        try {
            tally = n * 3;
            Box b = new Box(tally);
            if (fail) {
                throw new Marked(b.v);
            }
            return b.v;
        } catch (Marked e) {
            return -e.mark;
        } finally {
            // The `finally` runs on both paths; a dropped one shows as drift.
            tally = tally + 1;
            if (tally == Integer.MIN_VALUE) {
                System.out.println("unreachable");
            }
        }
    }

    /** Two locals, so a partial reconstruction shows up as a mismatch. */
    static int twoLocalsAcrossAlloc(int n, boolean fail) {
        int a = 0;
        int b = 0;
        try {
            a = n + 100;
            Box held = new Box(n);
            b = n + 200 + held.v - n;
            if (fail) {
                throw new Marked(0);
            }
            return a + b;
        } catch (Marked e) {
            return a * 1000 + b;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        long acc = 0;

        for (int i = 0; i < iterations; i++) {
            // Mostly the non-throwing path so each method gets hot and compiled,
            // then the throwing path on a fraction of iterations so the handler
            // runs with the compiled frame live.
            boolean fail = (i % 8 == 0);
            acc += throwNewInTry(i, fail);
            acc += allocThenThrow(i, fail);
            acc += rethrowInTry(i, fail);
            acc += allocInTryWithFinally(i, fail);
            acc += twoLocalsAcrossAlloc(i, fail);
        }

        // Spot values on the throwing path, printed so a wrong local is visible
        // as a number rather than only as a checksum drift.
        System.out.println("acc=" + acc);
        System.out.println("throwNewInTry(5,true)=" + throwNewInTry(5, true) + " expect=38013");
        System.out.println("allocThenThrow(5,true)=" + allocThenThrow(5, true) + " expect=66");
        System.out.println("rethrowInTry(5,true)=" + rethrowInTry(5, true) + " expect=105110");
        System.out.println("allocInTryWithFinally(5,true)=" + allocInTryWithFinally(5, true)
                + " expect=-15");
        System.out.println("twoLocalsAcrossAlloc(5,true)=" + twoLocalsAcrossAlloc(5, true)
                + " expect=105205");
    }
}
