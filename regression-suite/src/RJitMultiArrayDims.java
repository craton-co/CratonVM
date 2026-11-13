import java.util.ArrayList;
import java.util.List;

/**
 * Regression: a {@code multianewarray} of ANY arity must compile, and must give
 * the interpreter's answer when it does.
 *
 * <h2>The defect</h2>
 *
 * The x64 scanner refused every {@code multianewarray} whose {@code dimensions}
 * operand was not exactly 2:
 *
 * <pre>
 *   let ndims = code[pc + 3];
 *   if ndims != 2 { return None; }
 * </pre>
 *
 * A {@code jit_scan} {@code None} is PERMANENT and WHOLE-METHOD — both
 * {@code try_compile_inner} and the OSR door call {@code mark_jit_bail_listed}
 * on it — so one {@code new byte[3][4][4]} anywhere in a method made that whole
 * method run interpreted for the life of the process, through every door,
 * however hot it became. {@code RomulusEngine.skinny_128_384_plus_enc} opens
 * with two of them and is a 40-round permutation afterwards: 62.6% of
 * {@code RomulusTest}'s execution samples and 138x HotSpot's wall for that
 * test, against 4.3x for the lightweight-crypto battery around it.
 *
 * <p>The cause was the helper ABI, not the allocator. Windows x64 gives a
 * helper four integer argument registers and {@code jit_multianewarray_2d}
 * spent all four on {@code (vm_ptr, site, dim1, dim2)}, so a third dimension
 * had nowhere to go. The lowering now writes the dimension counts into frame
 * scratch words and passes their ADDRESS ({@code jit_multianewarray_n}, helper
 * ABI v14), which costs one register whatever the arity is.
 *
 * <h2>Why a two-tier fixture</h2>
 *
 * Same design, and same reason, as {@code RJitMultiArrayClass}: the property is
 * tier AGREEMENT, so every shape is read on the first iteration (interpreted),
 * at the iteration its answer changes if it ever does, and on the last
 * (compiled). Reading each shape once measures the interpreter alone — which
 * was always right here — and would go green on a VM that compiled nothing.
 *
 * <h2>What the E-rows are about</h2>
 *
 * Negative dimensions are checked OUTERMOST FIRST, and all of them are checked
 * before anything is allocated. MEASURED on HotSpot 25.0.3+9:
 * {@code new byte[-1][4][-2]} reports {@code -1}, and {@code new byte[2][0][-1]}
 * reports {@code -1} even though a zero outer length means no inner array would
 * ever be allocated. This VM's interpreter used to check them in POP order,
 * i.e. innermost first, and answered {@code -2} and "no throw" respectively;
 * its JIT helper already answered outermost-first, so the two tiers disagreed
 * with each other as well as with HotSpot. Both were corrected with this
 * lowering, which is why the messages and not just the exception type are
 * asserted here.
 *
 * <h2>Run it BOTH ways</h2>
 *
 * Normally and with {@code --nojit}. The arity rows are the ones that move: on
 * a pre-fix binary they are green under {@code --nojit} and green without it
 * too — the method simply never compiles — so this vector's value is the
 * {@code moved=} evidence and the companion measurement in
 * {@code docs/jdk-only/}, not a red-to-green flip on the answers alone. The
 * E-rows DO flip: they were red on both tiers before.
 */
public final class RJitMultiArrayDims {

    private static final int ITERS = 3000;

    private static final List<String> DIVERGENCES = new ArrayList<>();

    private static void ck(String name, String evidence) {
        System.out.println("CK RJitMultiArrayDims " + name + " " + evidence);
    }

    private static void diverge(String message) {
        DIVERGENCES.add(message);
        System.out.println("FAILED RJitMultiArrayDims " + message);
    }

    // --- allocation sites -------------------------------------------------
    //
    // One method per shape so each gets its own compiled artifact; dimensions
    // arrive as arguments so nothing can be folded into a shape the real defect
    // never saw.

    private static byte[][][] d3byte(int a, int b, int c) { return new byte[a][b][c]; }
    private static String[][][] d3ref(int a, int b, int c) { return new String[a][b][c]; }
    private static int[][] d2int(int a, int b) { return new int[a][b]; }
    private static long[][][][] d4long(int a, int b, int c, int d) { return new long[a][b][c][d]; }
    private static int[][][][][] d5int(int a, int b, int c, int d, int e) {
        return new int[a][b][c][d][e];
    }
    // Three brackets, two dimensions allocated: JVMS allows `dimensions` to be
    // less than the descriptor's bracket count, and the allocator must stop at
    // `dimensions` levels rather than at the type's.
    private static int[][][] d3partial(int a, int b) { return new int[a][b][]; }

    /**
     * The RomulusEngine shape proper: the allocation is not the method, it is
     * the first few bytecodes of a long one that must now compile as a whole.
     * Its answer is a checksum of every cell, so a wrong level length or a
     * mis-stamped component class shows up as a number rather than as a class
     * name.
     */
    private static int d3thenWork(int a, int b, int c) {
        int[][][] cells = new int[a][b][c];
        for (int i = 0; i < a; i++) {
            for (int j = 0; j < b; j++) {
                for (int k = 0; k < c; k++) {
                    cells[i][j][k] = i * 100 + j * 10 + k;
                }
            }
        }
        int acc = 0;
        for (int i = 0; i < a; i++) {
            for (int j = 0; j < b; j++) {
                for (int k = 0; k < c; k++) {
                    acc = acc * 31 + cells[i][j][k];
                }
            }
        }
        return acc;
    }

    private static String nameOf(Object o) { return o == null ? "null" : o.getClass().getName(); }

    /**
     * Length and runtime class at every level down the leftmost spine, so a
     * wrong level length AND a mis-stamped component class are both visible.
     *
     * It stops at the deepest ARRAY level rather than at the deepest object:
     * `Array.get` on an `int[]` boxes, and an `java.lang.Integer` at the end of
     * every primitive row would say nothing about the array.
     */
    private static String spine(Object o) {
        StringBuilder sb = new StringBuilder();
        Object cur = o;
        while (true) {
            if (cur == null) {
                sb.append("null");
                break;
            }
            Class<?> c = cur.getClass();
            if (!c.isArray()) {
                sb.append(c.getName());
                break;
            }
            int n = java.lang.reflect.Array.getLength(cur);
            sb.append(n).append(':').append(c.getName());
            if (!c.getComponentType().isArray()) {
                break;
            }
            sb.append(" / ");
            if (n == 0) {
                sb.append("(empty)");
                break;
            }
            cur = java.lang.reflect.Array.get(cur, 0);
        }
        return sb.toString();
    }

    /**
     * Allocate and report the NegativeArraySizeException message, which names
     * WHICH dimension was rejected. Anything else — including success — is
     * reported verbatim so a wrong answer is legible rather than merely absent.
     */
    private static String neg(int kind, int a, int b, int c, int d) {
        try {
            Object o;
            switch (kind) {
                case 2: o = d2int(a, b); break;
                case 3: o = d3byte(a, b, c); break;
                case 4: o = d4long(a, b, c, d); break;
                default: return "BAD-KIND";
            }
            return "no-throw-" + nameOf(o);
        } catch (NegativeArraySizeException e) {
            return "NASE:" + e.getMessage();
        } catch (Throwable t) {
            return "wrong-" + t.getClass().getName();
        }
    }

    // --- vector table -----------------------------------------------------

    private static final String[] NAMES = {
        "s00-byte[3][4][4]",
        "s01-byte[3][4][4].leaf",
        "s02-String[2][3][2]",
        "s03-String[2][3][2].l1",
        "s04-String[2][3][2].l2",
        "s05-String[2][3][2]-instanceof",
        "s06-int[4][5]",
        "s07-long[2][3][4][5]",
        "s08-int[2][2][2][2][2]",
        "s09-int[3][2][]",
        "s10-int[3][2][].leaf-null",
        "s11-3d-then-work",
        "s12-byte[0][4][4]",
        "s13-byte[2][0][4]",
        "s14-byte[2][3][0]",
        "s15-byte[3][4][4]-zeroed",
        "e00-neg-outer",
        "e01-neg-middle",
        "e02-neg-inner",
        "e03-neg-outer-and-inner",
        "e04-neg-middle-and-inner",
        "e05-neg-all",
        "e06-neg-2d-both",
        "e07-neg-4d-third",
        "e08-zero-outer-neg-inner",
        "e09-neg-outer-zero-inner",
        "e10-3d-zero-outer-neg-mid",
        "e11-3d-zero-mid-neg-inner",
    };

    /** MEASURED on HotSpot 25.0.3+9 (Temurin), not predicted. */
    private static final String[] EXPECTED = {
        "3:[[[B / 4:[[B / 4:[B",
        "[B",
        "2:[[[Ljava.lang.String; / 3:[[Ljava.lang.String; / 2:[Ljava.lang.String;",
        "[[Ljava.lang.String;",
        "[Ljava.lang.String;",
        "true",
        "4:[[I / 5:[I",
        "2:[[[[J / 3:[[[J / 4:[[J / 5:[J",
        "2:[[[[[I / 2:[[[[I / 2:[[[I / 2:[[I / 2:[I",
        "3:[[[I / 2:[[I / null",
        "true",
        "-1729318132",
        "0:[[[B / (empty)",
        "2:[[[B / 0:[[B / (empty)",
        "2:[[[B / 3:[[B / 0:[B",
        "0",
        "NASE:-1",
        "NASE:-5",
        "NASE:-7",
        "NASE:-1",
        "NASE:-5",
        "NASE:-1",
        "NASE:-1",
        "NASE:-9",
        "NASE:-1",
        "NASE:-1",
        "NASE:-1",
        "NASE:-1",
    };

    private static String observe(int id) {
        switch (id) {
            case 0:  return spine(d3byte(3, 4, 4));
            case 1:  return nameOf(d3byte(3, 4, 4)[0][0]);
            case 2:  return spine(d3ref(2, 3, 2));
            case 3:  return nameOf(d3ref(2, 3, 2)[0]);
            case 4:  return nameOf(d3ref(2, 3, 2)[0][0]);
            case 5:  return String.valueOf((Object) d3ref(2, 3, 2) instanceof String[][][]);
            case 6:  return spine(d2int(4, 5));
            case 7:  return spine(d4long(2, 3, 4, 5));
            case 8:  return spine(d5int(2, 2, 2, 2, 2));
            case 9:  return spine(d3partial(3, 2));
            case 10: return String.valueOf(d3partial(3, 2)[0][0] == null);
            case 11: return Integer.toString(d3thenWork(2, 3, 4));
            case 12: return spine(d3byte(0, 4, 4));
            case 13: return spine(d3byte(2, 0, 4));
            case 14: return spine(d3byte(2, 3, 0));
            case 15: return Integer.toString(d3byte(3, 4, 4)[2][3][3]);
            case 16: return neg(3, -1, 4, 4, 0);
            case 17: return neg(3, 2, -5, 4, 0);
            case 18: return neg(3, 2, 3, -7, 0);
            case 19: return neg(3, -1, 4, -2, 0);
            case 20: return neg(3, 2, -5, -7, 0);
            case 21: return neg(3, -1, -2, -3, 0);
            case 22: return neg(2, -1, -2, 0, 0);
            case 23: return neg(4, 2, 2, -9, 2);
            case 24: return neg(2, 0, -1, 0, 0);
            case 25: return neg(2, -1, 0, 0, 0);
            case 26: return neg(3, 0, -1, 4, 0);
            case 27: return neg(3, 2, 0, -1, 0);
            default: throw new AssertionError("no such shape " + id);
        }
    }

    public static void main(String[] args) {
        int checks = 0;

        for (int id = 0; id < NAMES.length; id++) {
            String cold = null;
            String hot = null;
            String moved = null;
            int movedAt = -1;

            for (int i = 0; i < ITERS; i++) {
                String a = observe(id);
                if (i == 0) {
                    cold = a;
                } else if (movedAt < 0 && !a.equals(cold)) {
                    movedAt = i;
                    moved = a;
                }
                hot = a;
            }

            String name = NAMES[id];
            String want = EXPECTED[id];

            // Evidence first, verdicts after, so a FAILED line always follows
            // the values it is about.
            ck(name, "cold=[" + cold + "] hot=[" + hot + "] moved=" + movedAt + " iters=" + ITERS);

            checks++;
            if (!want.equals(cold)) {
                diverge(name + " COLD: want=[" + want + "] got=[" + cold + "]");
            }
            checks++;
            if (!want.equals(hot)) {
                diverge(name + " HOT: want=[" + want + "] got=[" + hot + "]");
            }
            checks++;
            if (movedAt >= 0) {
                diverge(name + " TIER-SPLIT at i=" + movedAt
                    + ": cold=[" + cold + "] became=[" + moved + "] final=[" + hot + "]");
            }
        }

        // SEPARATE lines, and in this order — `harness_check_count` does
        // `sub(/^.*checks=/, ""); print`, so `checks=84 fails=0` would publish
        // the "count" `84 fails=0`.
        System.out.println("CK RJitMultiArrayDims fails=" + DIVERGENCES.size());
        System.out.println("CK RJitMultiArrayDims checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RJitMultiArrayDims (" + checks + " checks)");
    }
}
