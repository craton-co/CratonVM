import java.util.ArrayList;
import java.util.List;

/**
 * TypecheckAnswerMemoProbe — differential probe for the JIT `checkcast` /
 * `instanceof` positive-answer memo (`JIT_TYPECHECK_ANSWER_CACHE` in
 * `vm/src/jit/helpers.rs`, hashmap-half-gap-20260730).
 *
 * The memo answers a type check from `(vm, call-site class-name pointer,
 * receiver class id, lenient)` alone, so the shapes that could break are the
 * ones where that key does NOT determine the answer:
 *
 *   1. A POLYMORPHIC site — one `checkcast`/`instanceof` reached by receivers
 *      of several classes. A memo that ignored the receiver class, or that
 *      cached a `true` and replayed it for a different receiver, answers wrong.
 *   2. An ARRAY receiver — a reference array's header stores its COMPONENT
 *      class id, so `String[]` and `String` present the same receiver class id.
 *      The memo must refuse to cache these; if it did not, `String[] instanceof
 *      CharSequence` would start answering true after `"x" instanceof
 *      CharSequence` warmed the site.
 *   3. ALTERNATION between sites, which must not let one site's answer leak
 *      into another's.
 *   4. NEGATIVE answers, which are never cached and so must keep re-deriving.
 *   5. A failing `checkcast`, which must still throw ClassCastException after
 *      the same site has succeeded thousands of times.
 *
 * Every loop runs far past the OSR/tier-up thresholds so the checks execute
 * compiled, not interpreted. The probe prints one PASS/FAIL line per shape and
 * a final verdict; exit code is non-zero if anything failed.
 */
public class TypecheckAnswerMemoProbe {

    interface Shape {}

    interface Named {}

    static class Circle implements Shape, Named {}

    static class Square implements Shape {}

    static class Unrelated {}

    private static int failures = 0;

    static void check(String what, boolean ok) {
        System.out.println((ok ? "PASS " : "FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    /** One `instanceof Shape` site reached by four different receiver classes. */
    static long polymorphicInstanceof(Object[] receivers, int reps) {
        long shapes = 0;
        for (int r = 0; r < reps; r++) {
            for (Object o : receivers) {
                if (o instanceof Shape) {
                    shapes++;
                }
            }
        }
        return shapes;
    }

    /** One `instanceof CharSequence` site reached by a String and a String[]. */
    static long arrayVersusComponent(Object str, Object arr, int reps) {
        long hits = 0;
        for (int r = 0; r < reps; r++) {
            if (str instanceof CharSequence) {
                hits++;
            }
            if (arr instanceof CharSequence) {
                hits += 1000;
            }
        }
        return hits;
    }

    /** Two distinct `checkcast` sites alternating in one loop. */
    static long alternatingCheckcastSites(Object a, Object b, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            Circle c = (Circle) a;
            Square s = (Square) b;
            n += (c != null ? 1 : 0) + (s != null ? 2 : 0);
        }
        return n;
    }

    /** Reference-array casts, both the legal and the illegal direction. */
    static long arrayCasts(Object strArr, int reps) {
        long n = 0;
        for (int r = 0; r < reps; r++) {
            Object[] objs = (Object[]) strArr;
            n += objs.length;
            if (strArr instanceof String[]) {
                n += 2;
            }
            if (strArr instanceof Integer[]) {
                n += 1000;
            }
        }
        return n;
    }

    /** A site that succeeds many times, then must still throw for a bad value. */
    static boolean checkcastStillThrowsAfterWarmup(int reps) {
        Object good = new Circle();
        for (int r = 0; r < reps; r++) {
            Circle c = (Circle) good;
            if (c == null) {
                return false;
            }
        }
        try {
            Object bad = new Unrelated();
            Circle c = (Circle) bad;
            return c == null; // unreachable
        } catch (ClassCastException expected) {
            return true;
        }
    }

    /** `null` is castable to anything and instanceof-nothing, always. */
    static long nullBehaviour(int reps) {
        long n = 0;
        Object nul = null;
        for (int r = 0; r < reps; r++) {
            Circle c = (Circle) nul;
            if (c == null) {
                n++;
            }
            if (nul instanceof Shape) {
                n += 1000;
            }
        }
        return n;
    }

    static void run(int reps) {
        Object[] receivers = {new Circle(), new Square(), new Unrelated(), "a string"};
        // 2 of the 4 receivers are Shapes.
        check(
                "polymorphic instanceof site counts exactly the Shapes",
                polymorphicInstanceof(receivers, reps) == 2L * reps);

        String[] strArr = {"a", "b", "c"};
        // Only the String hits; the String[] must never be a CharSequence.
        check(
                "String[] is not a CharSequence even after String warmed the site",
                arrayVersusComponent("hello", strArr, reps) == (long) reps);

        check(
                "two checkcast sites alternating stay independent",
                alternatingCheckcastSites(new Circle(), new Square(), reps) == 3L * reps);

        // 3 (length) + 2 (String[]) per rep, and never the Integer[] arm.
        check("String[] casts to Object[] and is a String[], never an Integer[]",
                arrayCasts(strArr, reps) == 5L * reps);

        check(
                "a warmed-up checkcast site still throws CCE for a bad receiver",
                checkcastStillThrowsAfterWarmup(reps));

        check("null casts to anything and is an instance of nothing",
                nullBehaviour(reps) == (long) reps);

        // Interface widening through a genuinely polymorphic site, with the
        // negative arm (never cached) interleaved.
        List<Object> mixed = new ArrayList<>();
        mixed.add(new Circle());
        mixed.add(new Square());
        long named = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < mixed.size(); i++) {
                if (mixed.get(i) instanceof Named) {
                    named++;
                }
            }
        }
        check("only Circle implements Named at a shared interface site", named == (long) reps);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        run(reps);
        System.out.println(failures == 0 ? "TYPECHECK-MEMO PROBE: PASS" : "TYPECHECK-MEMO PROBE: FAIL");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
