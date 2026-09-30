/**
 * Discriminating probe for the IR relocation map contract.
 *
 * The predecessor, {@code IrRelocProbe}, had the right *shape* but allocated
 * with {@code new int[6]}, and array allocation has no arm in the IR builder's
 * opcode match — so it produced zero IR bodies and both lanes of every A/B
 * agreed for a reason that had nothing to do with the contract
 * (jit-ir-relocation-map-contract.md). This one allocates an
 * OBJECT, which the optimizing tier does lower.
 *
 * Four properties are needed at once, or the probe measures nothing:
 *
 *   1. IR-eligible — no arrays anywhere in the hot methods (so no {@code
 *      newarray}/{@code anewarray}/{@code aaload}), no statics (the builder has
 *      no {@code getstatic}/{@code putstatic} arm), no athrow, no invokedynamic,
 *      no checkcast/instanceof, no try/catch.
 *   2. the allocation must ESCAPE, or escape analysis scalar-replaces it and
 *      nothing reaches the heap. {@code mine} is passed to a call, which is what
 *      makes it escape and the {@code Op::New} survive to real codegen.
 *   3. a live reference held ACROSS a call and read after it, so the frame has
 *      something to publish and something that must be rewritten.
 *   4. depth, so many such frames are live at once and band-scan cost — which
 *      scales with live frame count — is visible.
 *
 * The recursion is deliberately routed through {@code down} rather than being
 * direct. {@code ir_lower}'s self-recursive call route bypasses
 * {@code emit_call_return_check}, so it neither publishes nor claims coverage;
 * a directly self-recursive {@code step} would therefore exercise none of the
 * contract even once it compiled.
 *
 * Verify discrimination BEFORE trusting any timing: with the coverage claim OFF
 * the run must report NON-ZERO {@code coverage_fallbacks}, and with it ON they
 * must drop. If both lanes agree, the probe is wrong, not the VM — that is the
 * error this file exists to stop repeating.
 */
public final class IrEscapeProbe {
    /** One int field only: reference fields would take the compact-layout bail. */
    static final class Cell {
        int v;
    }

    /** Gives {@code mine} somewhere to escape to, without needing a static. */
    static int consume(Cell c) {
        return c.v & 1;
    }

    /** Trampoline: keeps {@code step}'s recursive edge a CROSS call. */
    static int down(Cell held, int depth) {
        return step(held, depth - 1);
    }

    static int step(Cell held, int depth) {
        Cell mine = new Cell();
        mine.v = depth;
        if (depth <= 0) {
            return held.v & 1;
        }
        int r = down(held, depth);
        // `held` is live across the call above and read after it; `mine` is a
        // second live reference that escapes. A correct map publishes both and
        // a relocation rewrites both.
        return r + consume(mine) + (held.v & 1);
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 300000;
        Cell held = new Cell();
        held.v = 3;
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += step(held, depth);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("IrEscapeProbe depth=" + depth + " iters=" + iters
                + ": " + ms + " ms  [" + acc + "]");
    }
}
