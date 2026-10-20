import java.util.ArrayList;
import java.util.List;

/**
 * zr z3/rv3: does the ZGC crawl backstop (CRATONVM_ZGC_OVERHEAD_BACKSTOP) throw
 * OutOfMemoryError at a program that is still making steady progress?
 *
 * Phase 1 retains {@code fillPermille} / 1000 of Runtime.maxMemory() in 64 KiB
 * byte[] chunks. Phase 2 runs {@code rounds} rounds; each round allocates
 * {@code churnKb} KiB of short-lived byte[1024] garbage and folds it into a
 * checksum, so every round is real work whose garbage is dead at the round end.
 * On ZGC the backstop's wedge test is "under 2 % of the heap free after the
 * collection", and on a whole-heap backend the mutators can never allocate
 * more than that free space between two forced collections, so with the fill
 * above 98 % every forced collection counts toward the streak however much
 * work the program does in between.
 *
 * stdout: one line, {@code done rounds=R checksum=C} or
 * {@code oome phase=P round=R}. Compare with java -XX:+UseZGC on the same -Xmx.
 * Args: fillPermille rounds churnKb (defaults 982 400 1024).
 */
public class Zr3Rv3NearFullSteady {
    static final int CHUNK = 64 * 1024;

    public static void main(String[] args) {
        int fillPermille = args.length > 0 ? Integer.parseInt(args[0]) : 982;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int churnKb = args.length > 2 ? Integer.parseInt(args[2]) : 1024;
        long target = Runtime.getRuntime().maxMemory() / 1000L * fillPermille;
        List<byte[]> keep = new ArrayList<>();
        long held = 0;
        try {
            while (held + CHUNK <= target) {
                byte[] b = new byte[CHUNK];
                b[keep.size() % CHUNK] = (byte) keep.size();
                keep.add(b);
                held += CHUNK;
            }
        } catch (OutOfMemoryError e) {
            int n = keep.size();
            keep = null;
            System.out.println("oome phase=fill chunks=" + n);
            return;
        }
        long checksum = 0;
        int r = 0;
        try {
            for (r = 0; r < rounds; r++) {
                for (int i = 0; i < churnKb; i++) {
                    byte[] g = new byte[1024];
                    g[i & 1023] = (byte) (r + i);
                    checksum += g[i & 1023];
                }
                checksum += keep.get(r % keep.size())[0];
            }
        } catch (OutOfMemoryError e) {
            keep = null;
            System.out.println("oome phase=steady round=" + r);
            return;
        }
        System.out.println("done rounds=" + rounds + " checksum=" + checksum);
    }
}
