import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * A direct-{@code ByteBuffer} {@code getInt}/{@code putInt} loop in one
 * method, so the body is compiled by the OSR door -- the door {@code H12-1}
 * O1 converted onto {@code admit_direct_native_entry}.
 *
 * <p>Read with {@code CRATONVM_DBG_JIT_METHOD_STATS=1}. Three things it
 * establishes, all MEASURED 2026-09-22 on one host with the O1 binary and the
 * binary built from its parent, which report the figures below identically:
 *
 * <pre>
 *   --jdk-only    Buffer.session      sites sp:2  served=1,045,000  declined=0
 *                 reachabilityFence   14 sites
 *                 ScopedMemoryAccess  sites sp:0/ir:0/osr:0  served=0
 *   --compatible  every one of the above: 0
 * </pre>
 *
 * <p><b>1. The registered-{@code bridge} helpers bind and serve under strict,
 * and O1 did not move them.</b> {@code java/nio/Buffer.session()} is
 * {@code bridge} in both modes, so {@code admit_direct_native_entry} admits
 * it; a million served calls with zero declines is the callee-side gate
 * agreeing. Both binaries report the same numbers, which is what a bind-time
 * change that admits the same rows should look like.
 *
 * <p><b>2. The whole strict-mode direct-helper population is invisible in
 * {@code --compatible} on this workload.</b> Every counter is zero there,
 * because a CratonVM native overlay answers {@code ByteBuffer.getInt} before
 * the real {@code DirectByteBuffer.getInt} bytecode -- and its inner
 * {@code session()} and {@code ScopedMemoryAccess} call sites -- ever exists.
 * So timings taken on this probe in {@code --compatible} say nothing about
 * any thin helper: nothing is bound to time.
 *
 * <p><b>3. The sixteen {@code ScopedMemoryAccess} helpers are never bound, by
 * any door, in either mode.</b> Not here, and not by {@code RJdkFfmSegment},
 * {@code RSegmentBulkCopy} or {@code RDirectBufferElem} either -- all four
 * report {@code sites sp:0/ir:0/osr:0}. They have helper bodies, sixteen
 * {@code DirectHelperGate}s and sixteen {@code DIRECT_CALL_HELPER_NATIVES}
 * rows, and no site in the tree reaches one. That is the inert-bind shape
 * {@code THREAD_CURRENT_THREAD_SITES_OSR} exists because of, and it is why
 * this probe's banner says what the counters say rather than what the loop
 * looks like it should do. See
 * {@code docs/internal/retired/scoped-memory-direct-helpers-are-bound-by-nothing-20260922-RETIRED-20260922.md}.
 *
 * <p>Usage: {@code NioIntLoopProbe [mebibytes]} (default 8).
 */
public class NioIntLoopProbe {
    public static void main(String[] args) {
        int mib = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int bytes = mib * 1024 * 1024;
        ByteBuffer buf = ByteBuffer.allocateDirect(4096).order(ByteOrder.LITTLE_ENDIAN);
        int ints = bytes / 4;
        int slots = 4096 / 4;
        long acc = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < ints; i++) {
            int off = (i % slots) * 4;
            buf.putInt(off, i);
            acc += buf.getInt(off);
        }
        long ms = (System.nanoTime() - t0) / 1000000L;
        System.out.println("PROBE mib=" + mib + " ints=" + ints + " ms=" + ms
                + " acc=" + acc);
    }
}
