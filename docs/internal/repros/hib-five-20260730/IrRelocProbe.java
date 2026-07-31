/**
 * Discriminating probe for the IR relocation map contract.
 *
 * Needs all four properties at once, or it measures nothing (see
 * docs/known-issues/jit-ir-relocation-map-contract-remaining.md — BinTreesClassic
 * has deep stacks and heavy allocation yet never has an IR frame live at a
 * collection, so both lanes of an A/B agree and the null result is meaningless):
 *
 *   1. IR-eligible      — no athrow, no invokedynamic, few invokes/fields, small.
 *   2. holds a live REF across a call, so the frame has something to publish.
 *   3. allocates, so young collections actually happen while it is on the stack.
 *   4. recurses, so many such frames are live at once and band-scan cost (which
 *      scales with live frame count) is visible.
 *
 * `step` is the method that must be IR-compiled: `held` is live across the
 * recursive call and used after it, and `mine` is a second live reference.
 *
 * Verify discrimination BEFORE trusting any timing: with the coverage claim OFF
 * the run must report non-zero `coverage_fallbacks`, and with it ON they must
 * drop. If both lanes agree, the probe is wrong, not the VM.
 */
public final class IrRelocProbe {
    static Object sink;

    /** Kept tiny and branch-simple so the IR tier admits it. */
    static int step(Object held, int depth) {
        Object mine = new int[6];
        if (depth <= 0) {
            sink = mine;
            return held.hashCode() & 1;
        }
        int r = step(held, depth - 1);
        // Both references are live ACROSS the call above and read after it, so
        // a correct map must publish both and a relocation must rewrite both.
        return r + (mine.hashCode() & 1) + (held.hashCode() & 1);
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 40000;
        Object held = new int[8];
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += step(held, depth);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("IrRelocProbe depth=" + depth + " iters=" + iters
                + ": " + ms + " ms  [" + acc + "]");
    }
}
