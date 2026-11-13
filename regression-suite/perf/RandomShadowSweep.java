import java.util.*;
import java.util.stream.*;

/** L3 — `java.util.Random`, 32 owning registrations and no differential coverage.
 *
 *  After `Scanner` (43), this is the largest shadowed class in `java.util` that
 *  no probe had ever asked.
 *
 *  IT IS EXACTLY SPECIFIED, which is what makes a shadow here testable at all.
 *  `java.util.Random`'s javadoc pins the algorithm: a 48-bit linear congruential
 *  generator with the multiplier, addend and mask written out, `next(int)`
 *  defined on it, and `nextInt(bound)`, `nextDouble`, `nextGaussian` and the
 *  rest defined on `next`. A SEEDED `Random` therefore produces one legal
 *  sequence, and any conformant JVM produces exactly that one. So every row here
 *  prints VALUES, not shapes -- a differing digit is a defect, not a coin flip.
 *
 *  `nextGaussian()` is included for the same reason: `java.util.Random` keeps
 *  the legacy polar method with its cached second value, specified in the
 *  javadoc, rather than inheriting `RandomGenerator`'s ziggurat.
 *
 *  Nothing here uses an unseeded `Random` for its VALUE. The two unseeded rows
 *  ask only for a range property, which is the one thing that is specified.
 */
public class RandomShadowSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static Random r(long seed) { return new Random(seed); }

    static String seq(int n, java.util.function.Supplier<Object> f) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) sb.append(f.get()).append(',');
        return sb.toString();
    }

    public static void main(String[] args) {
        // ---- the raw sequence, which every other method is defined on
        tv("nextInt x8 seed42", () -> { Random x = r(42); return seq(8, x::nextInt); });
        tv("nextInt x8 seed0", () -> { Random x = r(0); return seq(8, x::nextInt); });
        tv("nextInt x8 seed-1", () -> { Random x = r(-1); return seq(8, x::nextInt); });
        tv("nextInt x4 seedMAX", () -> { Random x = r(Long.MAX_VALUE); return seq(4, x::nextInt); });
        tv("nextInt x4 seedMIN", () -> { Random x = r(Long.MIN_VALUE); return seq(4, x::nextInt); });
        tv("nextLong x6", () -> { Random x = r(42); return seq(6, x::nextLong); });
        tv("nextBoolean x12", () -> { Random x = r(42); return seq(12, x::nextBoolean); });
        tv("nextDouble x6", () -> { Random x = r(42); return seq(6, x::nextDouble); });
        tv("nextFloat x6", () -> { Random x = r(42); return seq(6, x::nextFloat); });
        tv("nextGaussian x6", () -> { Random x = r(42); return seq(6, x::nextGaussian); });

        // ---- bounded nextInt: the specified rejection loop, and its edges
        tv("nextInt(10) x10", () -> { Random x = r(42); return seq(10, () -> x.nextInt(10)); });
        tv("nextInt(1) x4", () -> { Random x = r(42); return seq(4, () -> x.nextInt(1)); });
        tv("nextInt(2) x10", () -> { Random x = r(42); return seq(10, () -> x.nextInt(2)); });
        tv("nextInt(pow2 1024) x6", () -> { Random x = r(42); return seq(6, () -> x.nextInt(1024)); });
        tv("nextInt(non-pow2 1000) x6",
                () -> { Random x = r(42); return seq(6, () -> x.nextInt(1000)); });
        tv("nextInt(MAX) x4", () -> { Random x = r(42); return seq(4, () -> x.nextInt(Integer.MAX_VALUE)); });
        tv("nextInt(0)", () -> r(42).nextInt(0));
        tv("nextInt(-5)", () -> r(42).nextInt(-5));

        // ---- the JDK 17+ RandomGenerator surface on Random
        tv("nextInt(origin,bound) x6",
                () -> { Random x = r(42); return seq(6, () -> x.nextInt(10, 20)); });
        tv("nextInt(o,b) empty", () -> r(42).nextInt(5, 5));
        tv("nextInt(o,b) inverted", () -> r(42).nextInt(9, 3));
        tv("nextLong(bound) x4", () -> { Random x = r(42); return seq(4, () -> x.nextLong(100L)); });
        tv("nextLong(o,b) x4", () -> { Random x = r(42); return seq(4, () -> x.nextLong(10L, 20L)); });
        tv("nextLong(0)", () -> r(42).nextLong(0L));
        tv("nextDouble(bound) x4", () -> { Random x = r(42); return seq(4, () -> x.nextDouble(5.0)); });
        tv("nextDouble(o,b) x4", () -> { Random x = r(42); return seq(4, () -> x.nextDouble(1.0, 2.0)); });
        tv("nextFloat(bound) x4", () -> { Random x = r(42); return seq(4, () -> x.nextFloat(5.0f)); });
        tv("nextGaussian(m,s) x4",
                () -> { Random x = r(42); return seq(4, () -> x.nextGaussian(10.0, 2.0)); });
        tv("nextExponential x4", () -> { Random x = r(42); return seq(4, x::nextExponential); });
        tv("nextDouble(bad bound)", () -> r(42).nextDouble(-1.0));
        tv("nextDouble(NaN bound)", () -> r(42).nextDouble(Double.NaN));

        // ---- nextBytes, which writes through an array
        tv("nextBytes 8", () -> {
            byte[] b = new byte[8];
            r(42).nextBytes(b);
            return Arrays.toString(b);
        });
        tv("nextBytes 3 (partial word)", () -> {
            byte[] b = new byte[3];
            r(42).nextBytes(b);
            return Arrays.toString(b);
        });
        tv("nextBytes 0", () -> {
            byte[] b = new byte[0];
            r(42).nextBytes(b);
            return "len=" + b.length;
        });
        tv("nextBytes null", () -> { r(42).nextBytes(null); return "no-throw"; });

        // ---- setSeed must RESET the sequence, and clear the gaussian cache
        tv("setSeed resets", () -> {
            Random x = r(42);
            int a = x.nextInt();
            x.setSeed(42);
            return a + " then " + x.nextInt();
        });
        tv("setSeed clears gaussian cache", () -> {
            Random x = r(42);
            x.nextGaussian();
            x.setSeed(42);
            return String.valueOf(x.nextGaussian());
        });
        tv("two instances same seed agree", () -> {
            return r(7).nextLong() == r(7).nextLong();
        });

        // ---- the streams, specified to use the same algorithm
        tv("ints(5)", () -> { Random x = r(42); return x.ints(5).boxed().toList().toString(); });
        tv("ints(5, 0, 10)", () -> r(42).ints(5, 0, 10).boxed().toList().toString());
        tv("longs(4)", () -> r(42).longs(4).boxed().toList().toString());
        tv("doubles(4)", () -> r(42).doubles(4).boxed().toList().toString());
        tv("doubles(3, 1.0, 2.0)", () -> r(42).doubles(3, 1.0, 2.0).boxed().toList().toString());
        tv("ints() limited", () -> r(42).ints().limit(4).boxed().toList().toString());
        tv("ints(-1)", () -> r(42).ints(-1).count());
        tv("ints stream is sized", () -> r(42).ints(5).spliterator().hasCharacteristics(
                Spliterator.SIZED));

        // ---- identity and the RandomGenerator wiring
        p("class", r(42).getClass().getName());
        p("is RandomGenerator", r(42) instanceof java.util.random.RandomGenerator);
        tv("isDeprecated", () -> r(42).isDeprecated());

        // ---- unseeded: only the RANGE is specified, never the value
        tv("unseeded nextInt(10) in range", () -> {
            Random x = new Random();
            for (int i = 0; i < 50; i++) { int v = x.nextInt(10); if (v < 0 || v > 9) return "OUT " + v; }
            return "in range";
        });
        tv("unseeded nextDouble in range", () -> {
            Random x = new Random();
            for (int i = 0; i < 50; i++) { double v = x.nextDouble(); if (v < 0.0 || v >= 1.0) return "OUT " + v; }
            return "in range";
        });

        System.out.println("DONE RandomShadowSweep");
    }
}
