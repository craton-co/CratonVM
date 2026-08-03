import java.util.ArrayList;
import java.util.List;

/**
 * cov-01 — is a reference materialised by `getstatic`, `ldc &lt;String&gt;` or
 * `ldc &lt;Class&gt;` published as a GC root by the OPTIMIZING (C2/IR) tier?
 *
 * <p>The lane brief's third verification item: "a test that a GC at a safepoint
 * after the {@code ldc}/{@code getstatic} does not lose or fail to rewrite the
 * loaded reference." Those are two different failures and this probe can only
 * reach one of them today — see the note at the bottom.
 *
 * <h2>What each rung is for</h2>
 *
 * <ul>
 *   <li>{@link #heldAcrossSafepoint} is the whole point. Three references are
 *       loaded by the three cov-01 bytecodes and then held live across an
 *       {@code invokestatic}, which is a safepoint the callee allocates hard
 *       at. The callee also OVERWRITES the static field, so after it returns
 *       the compiled frame's copy is the only live reference to the original
 *       object. If the frame does not publish that slot, the young collector
 *       reclaims the object and {@code b.v} reads freed memory.
 *   <li>{@link #hotString} / {@link #hotClass} / {@link #hotStaticRef} are the
 *       single-bytecode bodies, checked for identity stability against a
 *       reference taken BEFORE the loop — not against the previous iteration,
 *       because a stale baked address survives that check for a while and only
 *       then starts returning a relocated or reused one.
 *   <li>{@link #hotStaticInt} is the non-reference control. If the checks were
 *       vacuous — the methods never compiled, the loop never ran — this rung
 *       would pass in exactly the same way, so it is here to be compared
 *       against the compile log rather than to prove anything by itself.
 * </ul>
 *
 * <h2>Proving it is not vacuous</h2>
 *
 * Run it under {@code CRATONVM_DBG=ir-compiles} and confirm
 * {@code [ir] optimizing backend produced a body} names these methods. A rung
 * that never reached the optimizing tier tests the single-pass backend, which
 * has had all three lowerings for months.
 *
 * <p>Usage: {@code Cov01RootProbe [iterations]}
 *
 * <h2>The half this cannot reach</h2>
 *
 * "Fails to rewrite" needs a RELOCATING young collection to observe a live
 * compiled frame. It cannot: {@code JIT_PUBLISHES_RELOCATION_CONTRACT} is
 * {@code false}, so {@code collect_roots} refuses coverage for any frame with
 * no matching oop map — which is every IR frame, since the IR backend emits
 * none — and {@code gen_heap} diverts that cycle to non-relocating. So this
 * probe tests "does not LOSE the reference", which a non-relocating young
 * collection does exercise (it still sweeps), and the rewrite half becomes
 * testable only when that constant flips.
 */
public final class Cov01RootProbe {

    static final class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    /** Referenced only by {@link #hotClass()}, so that site is a real class `ldc`. */
    static final class Marker {}

    static final int MAGIC = 0x5EED_BEE;

    /** Iterations of {@link #churnAndRotate}'s allocation loop. */
    static final int CHURN_ROUNDS = 64;

    /**
     * What {@link #churnAndRotate} accumulates: {@code 0 + 1 + … + 63}. Derived
     * rather than written down, because a hand-computed expectation that
     * disagrees with the code fails at {@code i = 0} — before anything is
     * compiled — and says "a reference was reclaimed" about an arithmetic slip.
     */
    static final int CHURN_ACC = CHURN_ROUNDS * (CHURN_ROUNDS - 1) / 2;

    /**
     * A PLAIN static, deliberately not `final`: a `static final` reference is
     * exactly the shape a compiler is entitled to fold, and folding it would
     * remove the load this probe is about.
     */
    static Box sref = new Box(MAGIC);

    static int sint = 7;

    static final List<byte[]> CHURN = new ArrayList<>();

    static volatile int sink;

    /** `ldc <String>` and nothing else. */
    static String hotString() {
        return "cov-01-root-probe-literal";
    }

    /** `ldc <Class>` and nothing else. */
    static Class<?> hotClass() {
        return Marker.class;
    }

    /** `getstatic` of a reference and nothing else. */
    static Box hotStaticRef() {
        return sref;
    }

    /** `getstatic` of an int — the non-reference control. */
    static int hotStaticInt() {
        return sint;
    }

    /**
     * Allocates hard and then replaces the static, so the caller's frame holds
     * the only live reference to the object it loaded before the call.
     *
     * <p>Kept in its own method on purpose: a `putstatic` in the caller would
     * bail the IR builder (this lane owns the read arm only), and the method
     * under test has to reach the optimizing tier for the probe to mean
     * anything.
     */
    static int churnAndRotate(int n) {
        int acc = 0;
        for (int i = 0; i < CHURN_ROUNDS; i++) {
            byte[] b = new byte[256];
            b[0] = (byte) i;
            acc += b[0];
            CHURN.add(b);
            if (CHURN.size() > 128) {
                CHURN.clear();
            }
        }
        sref = new Box(MAGIC);
        return acc + n;
    }

    /**
     * The root question, stated as bytecode. `b`, `s` and `c` are live across
     * the call; the call is a safepoint that allocates.
     */
    static int heldAcrossSafepoint(int n) {
        Box b = sref; // getstatic <ref>
        String s = "cov-01-root-probe-literal"; // ldc <String>
        Class<?> c = Marker.class; // ldc <Class>
        int acc = churnAndRotate(n); // safepoint: allocates, then rotates `sref`
        // Every dereference below reads through a reference that was materialised
        // BEFORE the collection and used after it.
        return acc + b.v + s.length() + c.getName().length();
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        // Taken before the loop, exactly as `LdcClassProbe` does: comparing
        // against the previous iteration would accept a value that drifts once
        // and then stays consistent.
        String expectedString = hotString();
        Class<?> expectedClass = hotClass();
        int expectedStringLen = expectedString.length();
        int expectedClassNameLen = expectedClass.getName().length();

        long checked = 0;
        for (int i = 0; i < iters; i++) {
            String s = hotString();
            if (s != expectedString) {
                throw new AssertionError("ldc <String> lost identity at i=" + i);
            }
            Class<?> c = hotClass();
            if (c != expectedClass) {
                throw new AssertionError("ldc <Class> lost identity at i=" + i);
            }
            Box b = hotStaticRef();
            if (b.v != MAGIC) {
                throw new AssertionError(
                        "getstatic <ref> gave v=" + Integer.toHexString(b.v) + " at i=" + i);
            }
            if (hotStaticInt() != 7) {
                throw new AssertionError("getstatic <int> control failed at i=" + i);
            }

            int expected = CHURN_ACC + i + MAGIC + expectedStringLen + expectedClassNameLen;
            int got = heldAcrossSafepoint(i);
            if (got != expected) {
                throw new AssertionError(
                        "heldAcrossSafepoint at i="
                                + i
                                + ": got "
                                + got
                                + " expected "
                                + expected
                                + " (a reference held across the safepoint was reclaimed or"
                                + " corrupted)");
            }
            checked++;
            sink += got;
        }
        System.out.println("Cov01RootProbe OK iters=" + iters + " checked=" + checked);
    }
}
