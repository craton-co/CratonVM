/**
 * SpliceCastProbe -- the callee shape the optimizing tier refuses as
 * `checkcast/instanceof`.
 *
 * `unwrap` and `tagOf` are the typed-read-out-of-an-untyped-container shape
 * `c2-splice-getstatic-and-the-calls-it-left-behind-20260909.md` §5 names as
 * the next refusal to take. Every generic collection read compiles to one:
 * `aload_0; checkcast #k; getfield #f; ireturn` is what `List<Box>.get(i).v`
 * is after erasure, and the splice scanner refused the whole callee at its
 * `0xc0`.
 *
 * Same harness contract as SpliceStaticProbe: deterministic, checksummed, and
 * warmed 3,000,000 iterations rather than the 200,000 that merely clears the
 * tier threshold -- clearing it only ENQUEUES the compile, and a window that
 * mixes the single-pass and optimizing bodies has no median worth quoting.
 * Pin the arm with `CRATONVM_C2_ACCEPT=never` / `=always`.
 *
 * Usage: SpliceCastProbe [reps]     default 4,000,000
 */
public class SpliceCastProbe {
    static class Box {
        final int v;
        Box(int v) { this.v = v; }
    }

    static class Other {
        final int v;
        Other(int v) { this.v = v; }
    }

    // The table is Object[], so every read out of it needs a cast -- which is
    // what erasure leaves behind and what this probe is about.
    static final Object[] TABLE = new Object[64];
    static {
        for (int i = 0; i < TABLE.length; i++) {
            TABLE[i] = (i & 7) == 3 ? new Other(i) : new Box(i * 31);
        }
    }

    // `aload_0; checkcast #Box; getfield #v; ireturn` -- straight line, one
    // trailing return, no allocation. Everything the splicer wants except the
    // `0xc0`.
    static int unwrap(Object o) {
        return ((Box) o).v;
    }

    // `aload_0; instanceof #Box; ireturn` -- the 0xc1 half.
    static int tagOf(Object o) {
        return o instanceof Box ? 1 : 0;
    }

    static int step(int acc, int i) {
        Object o = TABLE[i & 63];
        int t = tagOf(o);
        return t == 1 ? acc + unwrap(o) : acc ^ i;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicecast (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
