import org.bouncycastle.crypto.digests.SHA256Digest;

/**
 * The native-call-dense kernel the getfield page measures with — bc-java's
 * `SHA256Digest` over a 64-byte block — reduced to something that runs in
 * seconds without a bc-java checkout.
 *
 * It exists to price ONE change: "validate once per native accessor call".
 * Every native call membership-walks each of its reference arguments, and this
 * kernel makes ~27 walks per iteration, so it is where that change can show up
 * at all. A/B it in one binary:
 *
 *   default                          validate once
 *   CRATONVM_GC_NO_VALIDATE_ONCE=1   re-validate, two and three walks per call
 *
 * The checksum is printed rather than merely computed: an arm that got the
 * answer wrong and an arm that got it right must be distinguishable by
 * something other than the time.
 */
public final class Sha256WalkProbe {
    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int warm = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;

        byte[] block = new byte[64];
        for (int i = 0; i < block.length; i++) {
            block[i] = (byte) (i * 7 + 1);
        }
        byte[] out = new byte[32];

        long sink = 0;
        for (int i = 0; i < warm; i++) {
            sink += digest(block, out);
        }

        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += digest(block, out);
        }
        long ns = System.nanoTime() - t0;

        long checksum = 0;
        for (byte b : out) {
            checksum = checksum * 31 + (b & 0xFF);
        }
        System.out.println("iters=" + iters
                + " ms=" + (ns / 1_000_000)
                + " ns/iter=" + String.format("%.1f", (double) ns / iters)
                + " checksum=" + checksum
                + " sinkNonZero=" + (sink != 0 ? 1 : 0));
    }

    private static int digest(byte[] block, byte[] out) {
        SHA256Digest d = new SHA256Digest();
        d.update(block, 0, block.length);
        d.doFinal(out, 0);
        return out[0];
    }
}
