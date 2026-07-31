/**
 * Differential verifier for the OSR dead-local entry refusal
 * (`CompiledMethod::can_osr_enter` / `osr_enter` rejecting a non-zero
 * `osr_dead_mask`).
 *
 * Every shape here is built so the graph-colouring allocator has an
 * incentive to COALESCE two locals onto one callee-saved register: a
 * "setup" local whose live range ends before the hot loop, and a loop
 * local whose range begins at the loop head. At the loop's OSR entry pc
 * the setup local is dead but register-resident, which is exactly the
 * `osr_dead_mask` bit that used to refuse the entry.
 *
 * Each shape reads back everything that could have been clobbered by a
 * mis-seeded OSR entry -- the loop's own result, the values live ACROSS
 * the loop, and (where applicable) the dead local's own re-definition
 * afterwards. The FNV-1a accumulator over all of them is the verdict:
 * it must be byte-identical on HotSpot, on CratonVM with the JIT, and on
 * CratonVM `--nojit`.
 *
 * Run with an iteration count high enough to trigger OSR (default 400k
 * per loop, well past the back-edge threshold).
 */
public final class OsrDeadLocalProbe {

    private static long acc = -3750763034362895579L; // FNV-1a 64 offset basis

    /** -Dosr.probe.verbose=1 prints every contribution so a mismatching
     *  accumulator can be narrowed to the shape that produced it. */
    private static final boolean VERBOSE = System.getProperty("osr.probe.verbose") != null;
    private static int round = 0;

    private static void mix(String tag, long v) {
        if (VERBOSE) {
            System.out.println("  [r" + round + "] " + tag + " = " + v);
        }
        for (int i = 0; i < tag.length(); i++) {
            acc = (acc ^ tag.charAt(i)) * 1099511628211L;
        }
        acc = (acc ^ v) * 1099511628211L;
    }

    private static int N = 400_000;

    // ------------------------------------------------------------------
    // 1. The canonical shape: a reference parameter (`args`-like) that is
    //    dead at the loop head. This is `entry_pc=4 mask=0x1` from the
    //    known-issue doc.
    // ------------------------------------------------------------------
    private static long deadRefParam(String[] setup) {
        int seed = setup.length + setup[0].length();
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += i ^ seed;
        }
        return total;
    }

    // ------------------------------------------------------------------
    // 2. Two disjoint int ranges either side of the loop, plus a value
    //    that must survive ACROSS the loop. If the trampoline seeds the
    //    dead local over the live one, `carried` comes back wrong.
    // ------------------------------------------------------------------
    private static long disjointIntRanges(int base) {
        int scratchA = base * 3;
        int scratchB = scratchA + 7;
        int scratchC = scratchB ^ 0x5a5a;
        long carried = scratchA + scratchB + scratchC;
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += (i & 0xffff) + carried;
        }
        int after = (int) (total & 0x7fff);
        int afterB = after * 31;
        int afterC = afterB - 11;
        return total + carried + after + afterB + afterC;
    }

    // ------------------------------------------------------------------
    // 3. The historical H2 `Select.queryFlat` shape: a category-2 (long)
    //    parameter live across the loop, plus a later local (`row`) that
    //    is dead at the loop head and re-defined inside it. Commit
    //    3415d052b cited exactly this -- local 7 dead, sharing state with
    //    live local 2 (`long limitRows`).
    // ------------------------------------------------------------------
    private static long category2ParamWithDeadRow(Object marker, long limitRows, boolean flag) {
        int pre = marker.hashCode() & 0xff;
        long sum = 0;
        long row = -1;
        for (int i = 0; i < N; i++) {
            row = i + limitRows;
            if (flag && (i & 1023) == 0) {
                sum += row;
            } else {
                sum += 1;
            }
        }
        return sum + row + limitRows + pre;
    }

    // ------------------------------------------------------------------
    // 4. Reference locals: a dead object reference sharing a register
    //    with a live one. A mis-seed here is a corrupted heap pointer,
    //    which shows up as a wrong length or an NPE rather than a wrong
    //    number.
    // ------------------------------------------------------------------
    private static long deadRefSharesWithLiveRef(String tag) {
        String scratch = tag + "-scratch";
        int scratchLen = scratch.length();
        String live = tag + "-live";
        long total = scratchLen;
        for (int i = 0; i < N; i++) {
            total += (i & 7) + live.length();
        }
        String rebound = live + total;
        return total + rebound.length() + live.charAt(0);
    }

    // ------------------------------------------------------------------
    // 5. double/float locals -- the XMM half of the same coalescing.
    //    (`DualPivotQuicksort` was the case that forced XMM coverage.)
    // ------------------------------------------------------------------
    private static long deadDoubleLocals(double seed) {
        double pivotA = seed * 1.5;
        double pivotB = pivotA + 0.25;
        double acc0 = pivotA + pivotB;
        double sum = 0.0;
        for (int i = 0; i < N; i++) {
            sum += (i % 17) * 0.5 + acc0;
        }
        double tailA = sum / 3.0;
        double tailB = tailA - 1.0;
        return Double.doubleToLongBits(sum) ^ Double.doubleToLongBits(tailA)
                ^ Double.doubleToLongBits(tailB) ^ Double.doubleToLongBits(acc0);
    }

    // ------------------------------------------------------------------
    // 6. Nested loops: the inner loop's OSR entry sees the outer loop's
    //    counter live and the setup locals dead.
    // ------------------------------------------------------------------
    private static long nestedLoops(int outerCount) {
        int setupA = outerCount * 5;
        int setupB = setupA ^ 0x1234;
        long total = setupB & 1;
        for (int o = 0; o < outerCount; o++) {
            long inner = 0;
            for (int i = 0; i < N / outerCount; i++) {
                inner += i + o;
            }
            total += inner;
        }
        return total;
    }

    // ------------------------------------------------------------------
    // 7. An array parameter dead at the loop head while the loop walks a
    //    DIFFERENT array. Reading the dead one afterwards proves the
    //    interpreter frame was not corrupted by the OSR round trip.
    // ------------------------------------------------------------------
    private static long deadArrayParam(int[] unusedInLoop, int[] walked) {
        int head = unusedInLoop[0];
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += walked[i % walked.length];
        }
        return total + head + unusedInLoop[unusedInLoop.length - 1] + walked.length;
    }

    // ------------------------------------------------------------------
    // 8. A loop followed by a string concat -- the exact RBC.7 shape, so
    //    the indy-concat bridge and the dead-mask entry are exercised
    //    together rather than one masking the other.
    // ------------------------------------------------------------------
    private static long loopThenConcat(String label) {
        int labelLen = label.length();
        long total = 0;
        for (int i = 0; i < N; i++) {
            total += i & 31;
        }
        String out = label + ":" + total + ":" + labelLen;
        return out.length() * 1_000_003L + out.hashCode();
    }

    public static void main(String[] args) throws Exception {
        if (args.length > 0) {
            N = Integer.parseInt(args[0]);
        }
        int[] unused = {11, 22, 33};
        int[] walked = {1, 2, 3, 4, 5, 6, 7};

        // Two rounds: the first drives the loops past the OSR threshold,
        // the second re-enters an already-published artifact.
        for (round = 0; round < 2; round++) {
            mix("deadRefParam", deadRefParam(new String[] {"alpha", "beta"}));
            mix("disjointIntRanges", disjointIntRanges(13));
            mix("category2ParamWithDeadRow",
                    category2ParamWithDeadRow("marker", 1_000_000_007L, true));
            mix("category2ParamWithDeadRowFalse",
                    category2ParamWithDeadRow("marker", -5L, false));
            mix("deadRefSharesWithLiveRef", deadRefSharesWithLiveRef("tag"));
            mix("deadDoubleLocals", deadDoubleLocals(3.25));
            mix("nestedLoops", nestedLoops(8));
            mix("deadArrayParam", deadArrayParam(unused, walked));
            mix("loopThenConcat", loopThenConcat("phase"));
        }

        System.out.println("OsrDeadLocalProbe acc=" + acc);
        System.out.println("sample deadRefParam=" + deadRefParam(new String[] {"alpha", "beta"}));
        System.out.println("sample cat2=" + category2ParamWithDeadRow("marker", 1_000_000_007L, true));
        System.out.println("sample nested=" + nestedLoops(8));
        System.out.println("sample concat=" + loopThenConcat("phase"));
    }
}
