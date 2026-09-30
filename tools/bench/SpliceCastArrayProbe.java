/**
 * SpliceCastArrayProbe -- SpliceCastProbe with the container taken out.
 *
 * Identical accessors (`asBox` = `checkcast`, `kindOf` = `instanceof`) and an
 * identical loop, reading an `Object[]` instead of a `List<Object>`. The point
 * is attribution: SpliceCastProbe's optimizing body is ~3x SLOWER than its
 * single-pass body, and the type checks are only one of the things in it. If
 * the gap follows the container rather than the casts, it is not this lane's.
 *
 * MEASURED 2026-09-09: the gap does NOT follow the casts. Optimizing ~193 ms
 * against single-pass ~226 ms here, so SpliceCastProbe's 3x belongs to the
 * container. With the type-check splice on, this probe also shows a second mode
 * at ~133 ms -- the spliced body -- in 8 of 34 interleaved rounds, which the
 * off arm never reaches.
 *
 * Usage: SpliceCastArrayProbe [reps]     default 4,000,000
 */
public class SpliceCastArrayProbe {
    static final class Box {
        final int v;
        Box(int v) { this.v = v; }
        int value() { return v; }
    }

    static Box asBox(Object o) { return (Box) o; }

    static int kindOf(Object o) { return o instanceof Box ? 1 : 0; }

    static int step(Object[] xs, int acc, int i) {
        Object o = xs[i & 15];
        return acc * 31 + asBox(o).value() + kindOf(o);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        Object[] xs = new Object[16];
        for (int i = 0; i < 16; i++) xs[i] = new Box(i * 7 + 1);
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(xs, warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(xs, acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicecastarray (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
