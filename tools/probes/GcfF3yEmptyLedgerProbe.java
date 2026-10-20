/**
 * gcf f3/y: a JIT-warm young-cycle shape whose pin ledger can be EMPTY.
 *
 * <p>With {@code CRATONVM_GEN_PINNED_YOUNG_COPY=1}, a young cycle that only the
 * conservative-JIT-root term (term 4) would divert reads the pause's young pin
 * ledger. When the ledger is complete and names no young object, the cycle runs
 * as an ORDINARY Cheney copy (decision {@code moving-no-young-pin-words},
 * {@code [GC] young_pinned_copy: pycopy_no_young_words=}), which flips the
 * semi-spaces and decommits the evacuated one. On every JIT-warm shape the
 * battery had (MtChurn, the divert probe, the evac probe) that never happens: a
 * compiled allocation loop keeps young references in its registers across every
 * allocation, so its deposit always names young words and every term-4 cycle is
 * a PINNED cycle ({@code pycopy_cycles}), never the Cheney one.
 *
 * <p>This probe separates the two roles. A SPINNER thread runs a compiled,
 * primitive-only loop (no reference local, no allocation) for the whole run --
 * the live compiled frame that makes young cycles term-4 cycles. The ALLOCATOR
 * is the main thread running {@link #churn}, which the row keeps in the
 * interpreter with {@code CRATONVM_JIT_DENY=GcfF3yEmptyLedgerProbe.churn} (an
 * interpreted frame is precise and deposits nothing). Everything the spinner's
 * frame could have picked up from its caller is allocated before, and promoted
 * by, the warm-up {@code System.gc()} calls, so its band holds no young word.
 *
 * <p>stdout (deterministic, = HotSpot): {@code EMPTYLEDGER_OK rounds=R checksum=C}.
 * stderr: {@code [emptyledger] spinner_iters=N} (timing-dependent, ignore).
 *
 * <p>Usage: {@code GcfF3yEmptyLedgerProbe [rounds]} (default 400).
 */
public final class GcfF3yEmptyLedgerProbe {

    static volatile int stop;
    static volatile long spinSink;

    /** Primitive-only; compiled (OSR) while the allocator runs. */
    static long spin(long seed) {
        long x = seed;
        long n = 0;
        while (stop == 0) {
            for (int i = 0; i < 100_000; i++) {
                x = x * 6364136223846793005L + 1442695040888963407L;
            }
            n++;
        }
        spinSink = x;
        return n;
    }

    /** The allocator: interpreted in the row (CRATONVM_JIT_DENY). */
    static long churn(int rounds) {
        long sum = 0;
        int[][] keep = new int[256][];
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 4096; i++) {
                int[] a = new int[30];
                a[0] = i ^ r;
                a[29] = r;
                keep[(i * 31 + r) & 255] = a;
                sum += a[0];
            }
            for (int k = 0; k < 256; k += 17) {
                int[] a = keep[k];
                if (a != null) {
                    sum = sum * 31 + a[0] + a[29];
                }
            }
        }
        return sum;
    }

    static long sum0;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        // Pre-start churn: about 3 young cycles at -Xmx64m (16 MiB semi).
        int prechurnRounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        final long[] iters = new long[1];
        Thread spinner = new Thread(() -> iters[0] = spin(7));
        spinner.setDaemon(true);
        // Promote the thread object, the lambda and the boot data first.
        for (int i = 0; i < 3; i++) {
            System.gc();
        }
        // gcf f5/y: vf3 read 3 NATIVE ledger words per cycle on the pinned arm
        // and none without it: with the pinned copy on, a deposit also sweeps
        // the Rust frames ABOVE the outermost compiled entry (the interpreter's
        // invoke path into `spin`), which hold the spinner's Thread, its lambda
        // and the `iters` array the lambda captured -- all still YOUNG, since
        // the requested majors above sweep young in place and keep root
        // values where they are. A JIT-cold churn before the start runs
        // ordinary moving young cycles (no compiled frame is live yet), which
        // tenure them under the row's -XX:MaxTenuringThreshold=1, so the
        // spinner's band then names only old objects.
        sum0 = churn(prechurnRounds);
        spinner.start();
        // Let the spinner reach its compiled (OSR) loop.
        Thread.sleep(500);
        long sum = churn(rounds);
        stop = 1;
        spinner.join();
        System.err.println("[emptyledger] spinner_iters=" + iters[0]);
        System.out.println("EMPTYLEDGER_OK rounds=" + rounds + " checksum=" + (sum + sum0));
    }
}
