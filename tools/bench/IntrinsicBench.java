// Interpreter intrinsic-table benchmark — Phase 3 of
// docs/feature_roadmap_interpreter_intrinsic_table.md.
//
// Call-heavy, interpreter-bound loops over the intrinsic method set. Run with
// the JIT disabled (CRATONVM_DISABLE_JIT=1) so the hot loop stays in the
// interpreter and the measurement isolates intrinsic-dispatch cost. Compare
// two runs: intrinsics on (default) vs. off (CRATONVM_DISABLE_INTRINSICS=1).
public class IntrinsicBench {
    public static void main(String[] args) {
        String s = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ";
        int[] src = new int[64];
        int[] dst = new int[64];
        for (int i = 0; i < 64; i++) src[i] = i * 7 + 1;

        // Warmup — fill the monomorphic inline caches at every call site.
        long w = stringArraycopyLoop(s, src, dst, 100_000);
        w += stringBuilderLoop(200_000);

        long t0 = System.nanoTime();
        long r1 = stringArraycopyLoop(s, src, dst, 3_000_000);
        long e1 = System.nanoTime() - t0;

        long t1 = System.nanoTime();
        long r2 = stringBuilderLoop(2_000_000);
        long e2 = System.nanoTime() - t1;

        System.out.println("string_arraycopy_ms=" + (e1 / 1_000_000) + " checksum=" + r1);
        System.out.println("stringbuilder_ms=" + (e2 / 1_000_000) + " checksum=" + r2);
        System.out.println("warmup=" + w);
    }

    // String.length / charAt / isEmpty + System.arraycopy — the roadmap
    // acceptance-criterion target loop.
    static long stringArraycopyLoop(String s, int[] src, int[] dst, int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            int len = s.length();
            acc += len;
            if (!s.isEmpty()) {
                acc += s.charAt(i % len);
            }
            System.arraycopy(src, 0, dst, 0, 64);
            acc += dst[i & 63];
        }
        return acc;
    }

    // StringBuilder.append (String/int/char/boolean) + length + Integer
    // box/unbox + Math.abs.
    static long stringBuilderLoop(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            StringBuilder sb = new StringBuilder();
            sb.append("v=").append(i).append(' ').append(i % 2 == 0);
            acc += sb.length();
            acc += Math.abs(i - 12345);
            acc += Integer.valueOf(i & 255).intValue();
        }
        return acc;
    }
}
