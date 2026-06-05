import org.bouncycastle.crypto.engines.ChaChaEngine;

// Direct stress of ChaChaEngine.chachaCore (public static) — the method the
// SPHINCS hang's watchdog caught spinning. Verifies output + measures throughput.
// Prints with explicit flush so output survives an abrupt rc!=0 exit.
public class ChaProbe {
    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int[] in = new int[16];
        int[] x  = new int[16];
        for (int i = 0; i < 16; i++) in[i] = 0x11111111 * (i + 1);

        // Warm + checksum: fold all outputs so the JIT can't dead-code it.
        long checksum = 0;
        long t0 = System.nanoTime();
        for (int n = 0; n < iters; n++) {
            in[12] = n;                 // vary the counter word each call
            ChaChaEngine.chachaCore(20, in, x);
            checksum += x[0] ^ ((long) x[15] << 32);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("ChaProbe: iters=" + iters + " checksum=" + checksum + " ms=" + ms);
        System.out.flush();
        System.err.flush();
    }
}
