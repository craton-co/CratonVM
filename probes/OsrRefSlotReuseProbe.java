import java.util.ArrayList;
import java.util.List;

/**
 * A reference local that javac also reuses as a `long`'s high half must survive
 * an OSR entry — with no Tomcat, no jar and no BCEL parser.
 *
 * This is the minimisation the retired `annotation-scan-arrayread-sigsegv`
 * write-up reported as impossible ("It does not minimise ... The crash needs the real
 * `parseMem` stage — i.e. Tomcat's BCEL parser actually running — before it").
 * It does minimise. Two things that record's standalone attempts left out, and
 * neither is the workload:
 *
 * 1. **The trailing `long ns = System.nanoTime() - t0;`.** That single statement
 *    compiles to `lstore 4`, which makes slots 4 AND 5 a cat-2 pair in a range
 *    disjoint from the loop. javac's allocation for each stage method is:
 *
 *        slot 0/1 : long t0
 *        slot 2/3 : long sink
 *        slot 4   : Iterator (the for-each temporary)
 *        slot 5   : byte[] b        <-- live reference INSIDE the loop
 *        slot 6   : int i
 *
 *    so slot 5 is both a live `byte[]` in the loop and a dead high half after
 *    it. Drop the timing line and the hazard disappears with it.
 *
 * 2. **`main` must not get compiled first.** The OSR trigger fires on a
 *    back-edge taken *by the interpreter*; if `main` builds its fixture in a hot
 *    loop, `main` itself OSRs, and every stage it then calls is entered from
 *    compiled code and compiled at its ENTRY instead — no OSR, no bug. That is
 *    why `parseMem` looked load-bearing: it is not the parser that matters, it
 *    is that the fixture arrives without `main` going hot. Here the fill loops
 *    live in `setup()` and `main` has no loop of its own.
 *
 * The defect: `wide_local_high_halves` is a whole-method scan, so it reports
 * slot 5 for the `lstore 4` regardless of the loop. Stripping slot 5's OSR
 * register home on that basis leaves the trampoline seeding only its FRAME slot
 * while the compiled body keeps reading its REGISTER, so the loop runs with
 * whatever the caller left there: `b.length` reads at `garbage + 12` and the VM
 * dies with a SIGSEGV inside compiled code — or, if the garbage happens to be
 * readable, silently sums the wrong bytes. Both are failures here: the checksum
 * comes from a fixed pattern, so a wrong answer is as loud as a crash.
 *
 * Why it takes a WINDOWS host to fail: `x64::LOCAL_REGS` is
 * `[R12,R13,R14,R15,RBX,RSI,RDI]` on Windows and `[R12,R13,R14,R15,RBX]` on
 * SysV, because RSI/RDI are callee-saved only in the Win64 ABI. With five
 * register homes slot 5 is frame-resident and the strip is a no-op; with seven
 * it is register-resident and the strip is the bug. Running it on Linux is
 * still worth doing — it just cannot fail there, so a Linux pass is not
 * evidence.
 *
 * Exits 1 (or dies) on failure, 0 on success. Takes no arguments.
 */
public class OsrRefSlotReuseProbe {

    /** Enough elements, and enough bytes each, that the inner loop OSRs. */
    private static final int ARRAYS = 160;
    private static final int LEN = 2048;

    private static final List<byte[]> data = new ArrayList<>();
    private static long expected;

    /** The exact shape of `AnnotationScanSplitProbe.arrayRead`. */
    private static long arrayRead() {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : data) {
            for (int i = 0; i < b.length; i++) {
                sink += b[i];
            }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) {
            System.out.print("");
        }
        return (ns & 1L) == -1L ? 0 : sink;
    }

    /** The same shape with one more reference local, as `readBytes` has. */
    private static long arrayReadWide() {
        long t0 = System.nanoTime();
        long sink = 0;
        for (byte[] b : data) {
            byte[] alias = b;
            for (int i = 0; i < alias.length; i++) {
                sink += alias[i];
            }
        }
        long ns = System.nanoTime() - t0;
        if (sink == Long.MIN_VALUE) {
            System.out.print("");
        }
        return (ns & 1L) == -1L ? 0 : sink;
    }

    /**
     * The fixture. Kept out of `main` on purpose — see (2) in the class comment:
     * a hot loop in `main` compiles `main`, and a stage called from compiled
     * code never OSRs.
     */
    private static void setup() {
        long sum = 0;
        for (int a = 0; a < ARRAYS; a++) {
            byte[] b = new byte[LEN];
            for (int i = 0; i < LEN; i++) {
                // A pattern spanning the signed byte range, so a wrong read is
                // very unlikely to sum to the right total by accident.
                b[i] = (byte) ((a * 31 + i * 7) & 0xff);
                sum += b[i];
            }
            data.add(b);
        }
        expected = sum;
    }

    private static int failures;

    private static void check(String stage, int round, long got) {
        if (got != expected) {
            System.out.println("FAIL " + stage + " round=" + round
                    + " expected=" + expected + " got=" + got);
            failures++;
        }
    }

    public static void main(String[] args) {
        setup();
        // Straight-line calls, no loop: `main` stays interpreted, so the FIRST
        // call of each stage is entered from the interpreter and its inner loop
        // trips the back-edge counter into an OSR entry. The repeats confirm the
        // compiled body keeps agreeing.
        check("arrayRead", 1, arrayRead());
        check("arrayReadWide", 1, arrayReadWide());
        check("arrayRead", 2, arrayRead());
        check("arrayReadWide", 2, arrayReadWide());
        check("arrayRead", 3, arrayRead());
        check("arrayReadWide", 3, arrayReadWide());

        System.out.println("arrays=" + ARRAYS + " len=" + LEN
                + " expected=" + expected + " failures=" + failures);
        System.out.println("PROBE-DONE");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
