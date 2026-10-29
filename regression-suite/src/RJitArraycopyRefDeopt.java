import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * Regression: every {@code System.arraycopy} the x64 primitive-copy intrinsic
 * BAILS on must resume in the interpreter with the operand stack it actually
 * had.
 *
 * <h2>The defect</h2>
 *
 * The intrinsic inlines a primitive fast path and branches to an uncommon-trap
 * deopt stub whenever any guard is unsatisfied — null, non-array, <b>reference
 * array</b>, mismatched element kinds, or an out-of-bounds position. A
 * reference array is therefore <i>always</i> a bail, by design, and the
 * interpreter re-runs the call. That makes the bail's frame snapshot
 * load-bearing for ordinary correct code, not only for error paths.
 *
 * Before the fix the snapshot was wrong. It names each of the five operands by
 * its frame home, captured BEFORE the intrinsic pins them into scratch homes —
 * and those scratch homes were allocated from {@code next_spill_offset}, which
 * the five {@code pop_stack} calls had just rewound back over the operands' own
 * slots. The store of {@code srcPos} landed on {@code dst}'s home. The resumed
 * frame then carried, in {@code dst}'s stack slot, the integer {@code srcPos}
 * tagged as a reference: not a plausible heap pointer, degraded to null by
 * {@code CompactValue::to_value}, and {@code System.arraycopy} threw
 * {@code NullPointerException} on a perfectly valid copy.
 *
 * The tell is that the bogus payload EQUALS srcPos — {@code arraycopy(a,1,b,2,3)}
 * resumed with {@code Object(1)}, and changing srcPos to 2 gave {@code Object(2)}.
 * The emitter had already noticed the aliasing and worked around it for its own
 * reads, by loading all five operands into distinct GPRs before storing any; the
 * deopt snapshot is a second consumer that workaround never reached.
 *
 * <h2>Why {@link #witness()} is shaped the way it is — do not "tidy" it</h2>
 *
 * Whether the scratch home actually lands on {@code dst}'s slot depends on the
 * method's spill layout, so this is NOT reproducible from the shape alone. A
 * first draft of this vector expressed the same idea with a counter instead of
 * the throw, the error cases inline as lambdas, and the copies spread over a
 * few more locals — and it passed on the broken binary. It asserted the right
 * things about a frame layout the defect does not occur in.
 *
 * {@link #witness()} is therefore kept byte-for-byte in the shape reduced from
 * the original {@code RMethodSiteCache.mixedRefAndPrimitive} failure: the
 * primitive warm-up loop allocating its destination inside the loop, the throw
 * on mismatch, then the reference copy — same locals, same order. The error
 * cases live in a separate method precisely so they cannot perturb it.
 *
 * If this vector is ever edited, re-verify it goes RED on a binary built before
 * the fix. A green here is only evidence if it can be made red.
 *
 * <h2>Run it BOTH ways</h2>
 *
 * Normally and with {@code --nojit}. Red without the flag and green with it puts
 * the divergence in the compiled tier, which is where this one lived.
 */
public final class RJitArraycopyRefDeopt {

    private static int checks;
    private static final List<String> DIVERGENCES = new ArrayList<>();

    private static void eq(String name, String got, String want) {
        checks++;
        if (!want.equals(got)) {
            String m = name + ": want=[" + want + "] got=[" + got + "]";
            DIVERGENCES.add(m);
            System.out.println("FAILED RJitArraycopyRefDeopt " + m);
        }
    }

    /** Names the throwable kind, or the result, without depending on a message. */
    private static String kindOf(Runnable r) {
        try {
            r.run();
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    /**
     * The layout-faithful witness. See the class comment before changing ANY of
     * it — the shape is the test.
     */
    static String witness(int srcPos, int dstPos) {
        int[] src = new int[16];
        for (int k = 0; k < 16; k++) {
            src[k] = 1000 + k;
        }
        for (int n = 0; n < 20_000; n++) {
            int[] dst = new int[16];
            System.arraycopy(src, 3, dst, 5, 7);
            if (dst[5] != 1003) {
                throw new AssertionError("int arraycopy at n=" + n);
            }
        }
        String[] ssrc = {"a", "b", "c", "d", "e"};
        String[] sdst = new String[5];
        System.arraycopy(ssrc, srcPos, sdst, dstPos, 3);
        return Arrays.toString(sdst);
    }

    /** The other bails, each re-running from the same reconstructed frame. */
    static void otherBails() {
        final String[] nsrc = null;
        final String[] ndst = null;
        final String[] five = {"a", "b", "c", "d", "e"};
        final String[] small = new String[2];
        final Object[] ints = new Integer[] {1, 2, 3};
        final String[] strs = new String[3];

        eq("s10-null-src", kindOf(() -> System.arraycopy(nsrc, 0, small, 0, 1)),
                "java.lang.NullPointerException");
        eq("s11-null-dst", kindOf(() -> System.arraycopy(five, 0, ndst, 0, 1)),
                "java.lang.NullPointerException");
        eq("s12-src-oob", kindOf(() -> System.arraycopy(five, 4, small, 0, 3)),
                "java.lang.ArrayIndexOutOfBoundsException");
        eq("s13-dst-oob", kindOf(() -> System.arraycopy(five, 0, small, 1, 3)),
                "java.lang.ArrayIndexOutOfBoundsException");
        eq("s14-negative-length", kindOf(() -> System.arraycopy(five, 0, small, 0, -1)),
                "java.lang.ArrayIndexOutOfBoundsException");
        eq("s15-incompatible-element", kindOf(() -> System.arraycopy(ints, 0, strs, 0, 3)),
                "java.lang.ArrayStoreException");
        eq("s16-not-an-array", kindOf(() -> System.arraycopy("nope", 0, small, 0, 1)),
                "java.lang.ArrayStoreException");

        // A reference copy that must SUCCEED, so a blanket bail-to-exception
        // cannot pass this method.
        String[] ok = new String[5];
        System.arraycopy(five, 1, ok, 2, 3);
        eq("s17-ref-copy-succeeds", Arrays.toString(ok), "[null, null, b, c, d]");

        // Same array, overlapping forward — memmove semantics through the bail.
        String[] ov = {"a", "b", "c", "d", "e"};
        System.arraycopy(ov, 0, ov, 1, 4);
        eq("s18-ref-overlap", Arrays.toString(ov), "[a, a, b, c, d]");
    }

    public static void main(String[] args) {
        // srcPos is the value that leaked into dst's slot, so drive several:
        // the pre-fix payload tracked this number exactly, and a single value
        // could in principle collide with a valid pointer-shaped small int.
        eq("s00-srcPos1", witness(1, 2), "[null, null, b, c, d]");
        eq("s01-srcPos2", witness(2, 1), "[null, c, d, e, null]");
        eq("s02-srcPos0", witness(0, 0), "[a, b, c, null, null]");
        // Twice, so the second call is unambiguously running the installed
        // artifact rather than racing its publication.
        eq("s03-srcPos1-again", witness(1, 2), "[null, null, b, c, d]");

        otherBails();

        System.out.println("CK RJitArraycopyRefDeopt fails=" + DIVERGENCES.size());
        System.out.println("CK RJitArraycopyRefDeopt checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RJitArraycopyRefDeopt (" + checks + " checks)");
    }
}
