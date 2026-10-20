// JIT round 21 wave 4, lane bce: range analysis, bounds-check elimination, null-check elimination
// and loop predication, as Java sees them.
//
// Every row method is warmed with valid inputs until both tiers have compiled it, then called with
// the hazard input. A predicate or an elided check may only DEOPT; what the program observes must
// be HotSpot's: the same return value, or the same exception class and message, thrown at the same
// iteration (`last=` is the index the loop body reached, written before the access). Each hazard
// call runs three times (first trap, after the deopt, after a possible recompile) and must answer
// the same each time; the row is then re-warmed and called once more with a valid input (`after`).
//
// Correctness rows (`row <name> <case> <outcome>`):
//   upcount      for (i = 0; i < n; i++) s += a[i]      zero-trip with null / short arrays, n > len
//   downcount    for (i = start; i >= 0; i--)            zero-trip (start -1, null a), start >= len
//   inclusive    for (i = lo; i <= hi; i++)              hi = MAX_VALUE: the counter wraps to MIN and
//                                                        the wrapped index is the one that throws
//   wrapstep     i = start; while (i >= start) i += k    the counter wraps past MAX, exits cleanly
//   stride2      i += 2 against a.length, a[i + 1]       odd length: Index len out of bounds
//   stride2ok    i += 2 against a.length - 1             odd length: valid
//   copy         for (i < a.length) a[i] = b[i]          b shorter / b null / zero-trip with null b
//   rowswap      m[0][j] while m[0] is replaced          the replaced row's length decides
//   reassign     a = y inside the loop                    the new array's length decides
//   reverse      for (i = a.length - off; i >= 0; i--)   off 0: the first access is out of bounds
//   masked       a[(h + k) & (a.length - 1)]             empty a: the mask is -1, the index is h
//   minusone     a[i - 1] from i = 0                     Index -1
//   guardneg     if (i < a.length) a[i]                  a negative i passes the signed test
//   longiv       for (long i ..) a[(int) i]              truncation and n > len
//   stencil      c[i] = a[i-1] + a[i] + a[i+1]           n past the array
//   bignull      read / write of a field at offset > 4096 on a null receiver: an implicit null check
//                there would fault outside the null page, so it must be an explicit one (NPE text)
// Timing rows (`time <row> <N> ns/op`, per element, median of 9 reps after a warm-up):
//   copy-unknown-len  a[i] = b[i] under i < a.length, b.length unknown (HotSpot: predicate + deopt)
//   reverse           for (i = a.length - 1; i >= 0; i--) s += a[i]
//   stride2           for (i = 0; i < n; i += 2) s += a[i]
//   sum2d             for i, for j < m[i].length: s += m[i][j]   (64 x 64)
//   stencil           c[i] = a[i-1] + a[i] + a[i+1] under i < n - 1, n unknown
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21BceRows.java):
//   P=tools/probes/out
//   cratonvm -Xmx1g -cp $P R21BceRows                 # --jdk-only (launcher default)
//   cratonvm --compatible -Xmx1g -cp $P R21BceRows    # both modes: the same row lines
//   java -Xmx1g -cp $P R21BceRows                     # HotSpot 25 reference
//   optional arguments: `facts` prints the row lines only; a number sets the timed elements per rep.
// Arms (row lines must not change in any of them):
//   def   ||
//   nopred |CRATONVM_JIT_IR_RANGE_PREDICATE=0 CRATONVM_JIT_IR_MONOTONE_PREDICATE=0
//           CRATONVM_JIT_IR_OFFSET_PREDICATE=0 CRATONVM_JIT_IR_MASK_PREDICATE=0|
//   noelim |CRATONVM_JIT_IR_CHECK_ELIM=0|
//   x64    |CRATONVM_JIT_INCLUSIVE_BCE=1 CRATONVM_JIT_RANGE_BCE=1|   (the opt-in single-pass reasons)
//   constoff |CRATONVM_JIT_IR_CONST_OFFSET_PREDICATE=1|   (BCE21-2; decider: time stencil, row stencil)
//   x64len   |CRATONVM_JIT_INLINE_LENGTH_BCE=1|   (BCE21-1; decider: time copy-unknown-len with the
//             optimizing tier withheld; rows copy / stride2 / reassign)
//   x64down  |CRATONVM_JIT_DOWN_COUNT_BCE=1|   (BCE21-3, incl. javac's `iflt` zero-compare test;
//             rows downcount / reverse / longiv; decider: time reverse with the optimizing tier withheld)
// HotSpot expectation: every `row` line identical to the HotSpot run, `stable` on every hazard
// case. Time rows (HotSpot 25, x64, order of magnitude): copy-unknown-len ~0.1-0.3 ns (vectorized),
// reverse ~0.3, stride2 ~0.5, sum2d ~0.3, stencil ~0.3-0.6 ns per element.

import java.util.Arrays;

public class R21BceRows {
    static int last;
    static long sink;

    /** A field past the first 4 KiB of the object. */
    static final class Big {
        long l0, l1, l2, l3, l4, l5, l6, l7, l8, l9, l10, l11, l12, l13, l14, l15;
        long l16, l17, l18, l19, l20, l21, l22, l23, l24, l25, l26, l27, l28, l29, l30, l31;
        long l32, l33, l34, l35, l36, l37, l38, l39, l40, l41, l42, l43, l44, l45, l46, l47;
        long l48, l49, l50, l51, l52, l53, l54, l55, l56, l57, l58, l59, l60, l61, l62, l63;
        long l64, l65, l66, l67, l68, l69, l70, l71, l72, l73, l74, l75, l76, l77, l78, l79;
        long l80, l81, l82, l83, l84, l85, l86, l87, l88, l89, l90, l91, l92, l93, l94, l95;
        long l96, l97, l98, l99, l100, l101, l102, l103, l104, l105, l106, l107, l108, l109, l110, l111;
        long l112, l113, l114, l115, l116, l117, l118, l119, l120, l121, l122, l123, l124, l125, l126, l127;
        long l128, l129, l130, l131, l132, l133, l134, l135, l136, l137, l138, l139, l140, l141, l142, l143;
        long l144, l145, l146, l147, l148, l149, l150, l151, l152, l153, l154, l155, l156, l157, l158, l159;
        long l160, l161, l162, l163, l164, l165, l166, l167, l168, l169, l170, l171, l172, l173, l174, l175;
        long l176, l177, l178, l179, l180, l181, l182, l183, l184, l185, l186, l187, l188, l189, l190, l191;
        long l192, l193, l194, l195, l196, l197, l198, l199, l200, l201, l202, l203, l204, l205, l206, l207;
        long l208, l209, l210, l211, l212, l213, l214, l215, l216, l217, l218, l219, l220, l221, l222, l223;
        long l224, l225, l226, l227, l228, l229, l230, l231, l232, l233, l234, l235, l236, l237, l238, l239;
        long l240, l241, l242, l243, l244, l245, l246, l247, l248, l249, l250, l251, l252, l253, l254, l255;
        long l256, l257, l258, l259, l260, l261, l262, l263, l264, l265, l266, l267, l268, l269, l270, l271;
        long l272, l273, l274, l275, l276, l277, l278, l279, l280, l281, l282, l283, l284, l285, l286, l287;
        long l288, l289, l290, l291, l292, l293, l294, l295, l296, l297, l298, l299, l300, l301, l302, l303;
        long l304, l305, l306, l307, l308, l309, l310, l311, l312, l313, l314, l315, l316, l317, l318, l319;
        long l320, l321, l322, l323, l324, l325, l326, l327, l328, l329, l330, l331, l332, l333, l334, l335;
        long l336, l337, l338, l339, l340, l341, l342, l343, l344, l345, l346, l347, l348, l349, l350, l351;
        long l352, l353, l354, l355, l356, l357, l358, l359, l360, l361, l362, l363, l364, l365, l366, l367;
        long l368, l369, l370, l371, l372, l373, l374, l375, l376, l377, l378, l379, l380, l381, l382, l383;
        long l384, l385, l386, l387, l388, l389, l390, l391, l392, l393, l394, l395, l396, l397, l398, l399;
        long l400, l401, l402, l403, l404, l405, l406, l407, l408, l409, l410, l411, l412, l413, l414, l415;
        long l416, l417, l418, l419, l420, l421, l422, l423, l424, l425, l426, l427, l428, l429, l430, l431;
        long l432, l433, l434, l435, l436, l437, l438, l439, l440, l441, l442, l443, l444, l445, l446, l447;
        long l448, l449, l450, l451, l452, l453, l454, l455, l456, l457, l458, l459, l460, l461, l462, l463;
        long l464, l465, l466, l467, l468, l469, l470, l471, l472, l473, l474, l475, l476, l477, l478, l479;
        long l480, l481, l482, l483, l484, l485, l486, l487, l488, l489, l490, l491, l492, l493, l494, l495;
        long l496, l497, l498, l499, l500, l501, l502, l503, l504, l505, l506, l507, l508, l509, l510, l511;
        long l512, l513, l514, l515, l516, l517, l518, l519, l520, l521, l522, l523, l524, l525, l526, l527;
        long l528, l529, l530, l531, l532, l533, l534, l535, l536, l537, l538, l539, l540, l541, l542, l543;
        long l544, l545, l546, l547, l548, l549, l550, l551, l552, l553, l554, l555, l556, l557, l558, l559;
        long l560, l561, l562, l563, l564, l565, l566, l567, l568, l569, l570, l571, l572, l573, l574, l575;
        long l576, l577, l578, l579, l580, l581, l582, l583, l584, l585, l586, l587, l588, l589, l590, l591;
        long l592, l593, l594, l595, l596, l597, l598, l599, l600, l601, l602, l603, l604, l605, l606, l607;
        long l608, l609, l610, l611, l612, l613, l614, l615, l616, l617, l618, l619, l620, l621, l622, l623;
        long l624, l625, l626, l627, l628, l629, l630, l631, l632, l633, l634, l635, l636, l637, l638, l639;
        long tail;
    }

    // ---------------------------------------------------------------- row methods

    static int upcount(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            last = i;
            s += a[i];
        }
        return s;
    }

    static int downcount(int[] a, int start) {
        int s = 0;
        for (int i = start; i >= 0; i--) {
            last = i;
            s += a[i];
        }
        return s;
    }

    static int inclusive(int[] a, int lo, int hi) {
        int s = 0;
        int cnt = 0;
        for (int i = lo; i <= hi; i++) {
            last = i;
            s += a[i - lo];
            if (++cnt > 6) break;
        }
        return s;
    }

    static int wrapstep(int[] a, int start, int k) {
        int s = 0;
        int i = start;
        while (i >= start) {
            last = i - start;
            s += a[i - start];
            i += k;
        }
        return s;
    }

    static int stride2(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i += 2) {
            last = i;
            s += a[i + 1];
        }
        return s;
    }

    static int stride2ok(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length - 1; i += 2) {
            s += a[i] + a[i + 1];
        }
        return s;
    }

    static int copy(int[] a, int[] b) {
        for (int i = 0; i < a.length; i++) {
            a[i] = b[i];
        }
        return a.length;
    }

    static int rowswap(int[][] m, int[] shorter) {
        int s = 0;
        for (int j = 0; j < 8; j++) {
            last = j;
            s += m[0][j];
            if (j == 3) m[0] = shorter;
        }
        return s;
    }

    static int reassign(int[] x, int[] y, int n) {
        int[] a = x;
        int s = 0;
        for (int i = 0; i < n; i++) {
            last = i;
            s += a[i];
            if (i == 2) a = y;
        }
        return s;
    }

    static int reverse(int[] a, int off) {
        int s = 0;
        for (int i = a.length - off; i >= 0; i--) {
            last = i;
            s += a[i];
        }
        return s;
    }

    static int masked(int[] a, int h) {
        int s = 0;
        for (int k = 0; k < 4; k++) {
            last = k;
            s += a[(h + k) & (a.length - 1)];
        }
        return s;
    }

    static int minusone(int[] a, int from, int n) {
        int s = 0;
        for (int i = from; i < n; i++) {
            last = i;
            s += a[i - 1];
        }
        return s;
    }

    static int guardneg(int[] a, int i) {
        if (i < a.length) {
            return a[i];
        }
        return -1;
    }

    static int longiv(int[] a, long from, long n) {
        int s = 0;
        for (long i = from; i < n; i++) {
            last = (int) i;
            s += a[(int) i];
        }
        return s;
    }

    static int stencil(int[] a, int[] c, int n) {
        for (int i = 1; i < n - 1; i++) {
            last = i;
            c[i] = a[i - 1] + a[i] + a[i + 1];
        }
        return n;
    }

    static int bignull(Big b, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            last = i;
            s += (int) b.l639;
        }
        return s;
    }

    static int bignullStore(Big b, int n) {
        for (int i = 0; i < n; i++) {
            last = i;
            b.l639 = i;
        }
        return n;
    }

    // ---------------------------------------------------------------- harness

    interface Case {
        int run();
    }

    static int[] filled(int n) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) a[i] = i * 3 + 1;
        return a;
    }

    static String outcome(Case c) {
        last = Integer.MIN_VALUE + 7;
        try {
            int r = c.run();
            return "ret=" + r + " last=" + lastText();
        } catch (Throwable t) {
            return t.getClass().getSimpleName() + ": " + t.getMessage() + " last=" + lastText();
        }
    }

    static String lastText() {
        return last == Integer.MIN_VALUE + 7 ? "none" : Integer.toString(last);
    }

    static final int WARM = 20000;

    static void warm(Case valid) {
        long s = 0;
        for (int k = 0; k < WARM; k++) s += valid.run();
        sink += s;
    }

    /** Warm with `valid`, call each hazard three times, re-warm, call `valid` once more. */
    static void row(String name, Case valid, String[] labels, Case[] hazards) {
        warm(valid);
        for (int h = 0; h < hazards.length; h++) {
            String first = outcome(hazards[h]);
            String second = outcome(hazards[h]);
            String third = outcome(hazards[h]);
            boolean stable = first.equals(second) && second.equals(third);
            System.out.println("row " + name + " " + labels[h] + " " + first
                    + (stable ? " stable" : " UNSTABLE[" + second + " | " + third + "]"));
        }
        warm(valid);
        System.out.println("row " + name + " after " + outcome(valid));
    }

    static void rows() {
        final int[] a16 = filled(16);
        final int[] a2 = filled(2);
        final int[] a3 = filled(3);
        final int[] a4 = filled(4);
        final int[] a6 = filled(6);
        final int[] a7 = filled(7);
        final int[] a8 = filled(8);
        final int[] a10 = filled(10);
        final int max = Integer.MAX_VALUE;

        row("upcount", () -> upcount(a16, 16),
                new String[] {"null-zero-trip", "short-zero-trip", "negative-bound", "null-one",
                    "n-past-len"},
                new Case[] {() -> upcount(null, 0), () -> upcount(a2, 0), () -> upcount(a3, -4),
                    () -> upcount(null, 1), () -> upcount(a2, 5)});

        row("downcount", () -> downcount(a16, 15),
                new String[] {"null-zero-trip", "start-past-len", "start-in-range"},
                new Case[] {() -> downcount(null, -1), () -> downcount(a3, 5),
                    () -> downcount(a3, 2)});

        row("inclusive", () -> inclusive(a16, 3, 9),
                new String[] {"null-zero-trip", "hi-max-wraps", "hi-max-minus-one"},
                new Case[] {() -> inclusive(null, 5, 4), () -> inclusive(a4, max - 3, max),
                    () -> inclusive(a8, max - 4, max - 1)});

        row("wrapstep", () -> wrapstep(a16, max - 15, 3),
                new String[] {"wraps-clean", "wraps-past-len"},
                new Case[] {() -> wrapstep(a6, max - 5, 2), () -> wrapstep(a4, max - 5, 2)});

        row("stride2", () -> stride2(a16),
                new String[] {"odd-len", "empty"},
                new Case[] {() -> stride2(a7), () -> stride2(new int[0])});

        row("stride2ok", () -> stride2ok(a16),
                new String[] {"odd-len", "len-one", "null"},
                new Case[] {() -> stride2ok(a7), () -> stride2ok(new int[1]),
                    () -> stride2ok(null)});

        final int[] dst5 = new int[5];
        row("copy", () -> copy(new int[16], a16),
                new String[] {"b-shorter", "b-null", "zero-trip-b-null", "a-null"},
                new Case[] {
                    () -> {
                        Arrays.fill(dst5, 0);
                        try {
                            return copy(dst5, a3);
                        } finally {
                            System.out.println("row copy b-shorter-state " + Arrays.toString(dst5));
                        }
                    },
                    () -> copy(new int[2], null), () -> copy(new int[0], null),
                    () -> copy(null, a3)});

        final int[] a4b = filled(4);
        row("rowswap", () -> rowswap(new int[][] {filled(8)}, filled(8)),
                new String[] {"shorter-row", "null-row"},
                new Case[] {() -> rowswap(new int[][] {filled(8)}, a4b),
                    () -> rowswap(new int[][] {filled(8)}, null)});

        row("reassign", () -> reassign(a16, a16, 16),
                new String[] {"shorter", "null"},
                new Case[] {() -> reassign(a8, a3, 8), () -> reassign(a8, null, 8)});

        row("reverse", () -> reverse(a16, 1),
                new String[] {"from-len", "empty-from-len", "null", "empty-zero-trip"},
                new Case[] {() -> reverse(filled(5), 0), () -> reverse(new int[0], 0),
                    () -> reverse(null, 1), () -> reverse(new int[0], 1)});

        row("masked", () -> masked(a16, 13),
                new String[] {"empty", "null", "len8"},
                new Case[] {() -> masked(new int[0], 5), () -> masked(null, 1),
                    () -> masked(a8, 6)});

        row("minusone", () -> minusone(a4, 1, 4),
                new String[] {"from-zero", "zero-trip-null"},
                new Case[] {() -> minusone(a4, 0, 2), () -> minusone(null, 0, 0)});

        row("guardneg", () -> guardneg(a16, 5),
                new String[] {"negative", "min", "at-len"},
                new Case[] {() -> guardneg(a4, -3), () -> guardneg(a4, Integer.MIN_VALUE),
                    () -> guardneg(a4, 4)});

        row("longiv", () -> longiv(a16, 0, 16),
                new String[] {"n-past-len", "truncated", "zero-trip-null"},
                new Case[] {() -> longiv(a4, 0, 6), () -> longiv(a4, (1L << 32) + 1, (1L << 32) + 3),
                    () -> longiv(null, 5, 3)});

        final int[] c10 = new int[10];
        row("stencil", () -> stencil(a16, new int[16], 16),
                new String[] {"n-past-len", "zero-trip-null"},
                new Case[] {() -> stencil(a10, c10, 12), () -> stencil(null, null, 2)});

        final Big big = new Big();
        row("bignull", () -> bignull(big, 4),
                new String[] {"zero-trip", "read", "store"},
                new Case[] {() -> bignull(null, 0), () -> bignull(null, 3),
                    () -> bignullStore(null, 2)});
        warm(() -> bignullStore(big, 4));
        System.out.println("row bignull store-after " + outcome(() -> bignullStore(null, 1)));
    }

    // ---------------------------------------------------------------- timing

    static long median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        return c[c.length / 2];
    }

    static long copyLoop(int[] a, int[] b) {
        for (int i = 0; i < a.length; i++) a[i] = b[i];
        return a[a.length - 1];
    }

    static long reverseLoop(int[] a) {
        long s = 0;
        for (int i = a.length - 1; i >= 0; i--) s += a[i];
        return s;
    }

    static long stride2Loop(int[] a, int n) {
        long s = 0;
        for (int i = 0; i < n; i += 2) s += a[i];
        return s;
    }

    static long sum2d(int[][] m) {
        long s = 0;
        for (int i = 0; i < m.length; i++) {
            for (int j = 0; j < m[i].length; j++) s += m[i][j];
        }
        return s;
    }

    static long stencilLoop(int[] a, int[] c, int n) {
        for (int i = 1; i < n - 1; i++) c[i] = a[i - 1] + a[i] + a[i + 1];
        return c[1];
    }

    interface Timed {
        long run();
    }

    static void time(String name, long elementsPerCall, int calls, Timed t) {
        for (int k = 0; k < 3 * calls; k++) sink += t.run();
        long[] reps = new long[9];
        for (int r = 0; r < reps.length; r++) {
            long t0 = System.nanoTime();
            long s = 0;
            for (int k = 0; k < calls; k++) s += t.run();
            reps[r] = System.nanoTime() - t0;
            sink += s;
        }
        double per = (double) median(reps) / ((double) calls * elementsPerCall);
        System.out.println("time " + name + " " + String.format("%.3f", per) + " ns/op");
    }

    static void times(int elements) {
        final int n = Math.max(64, elements);
        final int[] a = filled(n);
        final int[] b = filled(n);
        final int[] c = new int[n];
        final int calls = Math.max(1, 20_000_000 / n);
        time("copy-unknown-len", n, calls, () -> copyLoop(c, b));
        time("reverse", n, calls, () -> reverseLoop(a));
        time("stride2", n / 2, calls, () -> stride2Loop(a, n));
        final int[][] m = new int[64][];
        for (int i = 0; i < 64; i++) m[i] = filled(64);
        time("sum2d", 64 * 64, Math.max(1, 20_000_000 / 4096), () -> sum2d(m));
        time("stencil", n - 2, calls, () -> stencilLoop(a, c, n));
    }

    public static void main(String[] args) {
        boolean factsOnly = false;
        int elements = 4096;
        for (String s : args) {
            if (s.equals("facts")) factsOnly = true;
            else elements = Integer.parseInt(s);
        }
        rows();
        if (!factsOnly) times(elements);
        System.out.println("done sink=" + (sink != 42 ? "ok" : "?"));
    }
}
