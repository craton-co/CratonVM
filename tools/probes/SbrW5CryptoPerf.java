import java.security.MessageDigest;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.IvParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * SBR wave 5, lane cryptoeng, measurement 7: the JCA engines TLS runs on,
 * each on a 1 MiB input (ms/MiB) plus the per-message shapes (us/op).
 * Best of {@code rounds}; prints a checksum so a wrong answer cannot pass for
 * a fast one. One process per arm; interleave arms externally (A B A B ...).
 *
 * <pre>
 *   sha256 / sha384   MessageDigest.getInstance + digest(1 MiB)
 *   sha256s           getInstance + digest(64 B), us/op
 *   hmac              HmacSHA256 init + doFinal(1 MiB)
 *   hmacs             HmacSHA256 init + doFinal(64 B), us/op
 *   gcm               AES-128-GCM encrypt 1 MiB
 *   cbc               AES-128-CBC/PKCS5 encrypt 1 MiB
 *   ccp               ChaCha20-Poly1305 encrypt 1 MiB
 *   gcmrec            AES-128-GCM encrypt of a 16 KiB TLS record, us/op
 * </pre>
 */
public class SbrW5CryptoPerf {
    static final int N = Integer.getInteger("n", 8);
    static final int ROUNDS = Integer.getInteger("rounds", 5);

    public static void main(String[] a) throws Exception {
        byte[] big = new byte[1 << 20];
        for (int i = 0; i < big.length; i++) big[i] = (byte) (i * 7 + (i >>> 11));
        byte[] key = new byte[32];
        for (int i = 0; i < 32; i++) key[i] = (byte) (i + 3);
        byte[] iv = new byte[12];
        byte[] iv16 = new byte[16];
        byte[] small = new byte[64];
        byte[] rec = new byte[16384];
        long ck = 0;
        String[] names = {"sha256", "sha384", "sha256s", "hmac", "hmacs", "gcm", "cbc", "ccp", "gcmrec"};
        double[] best = new double[names.length];
        java.util.Arrays.fill(best, 1e18);
        for (int r = 0; r < ROUNDS; r++) {
            double[] t = new double[names.length];
            long s = System.nanoTime();
            for (int i = 0; i < N; i++) ck += MessageDigest.getInstance("SHA-256").digest(big)[i & 31];
            t[0] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < N; i++) ck += MessageDigest.getInstance("SHA-384").digest(big)[i & 31];
            t[1] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < 20000; i++) { small[0] = (byte) i; ck += MessageDigest.getInstance("SHA-256").digest(small)[i & 31]; }
            t[2] = (System.nanoTime() - s) / 1e3 / 20000;
            s = System.nanoTime();
            for (int i = 0; i < N; i++) {
                Mac m = Mac.getInstance("HmacSHA256");
                m.init(new SecretKeySpec(key, "HmacSHA256"));
                ck += m.doFinal(big)[i & 31];
            }
            t[3] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < 20000; i++) {
                Mac m = Mac.getInstance("HmacSHA256");
                m.init(new SecretKeySpec(key, "HmacSHA256"));
                small[1] = (byte) i;
                ck += m.doFinal(small)[i & 31];
            }
            t[4] = (System.nanoTime() - s) / 1e3 / 20000;
            s = System.nanoTime();
            for (int i = 0; i < N; i++) {
                iv[0] = (byte) i; iv[1] = (byte) r;
                Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
                c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, 0, 16, "AES"), new GCMParameterSpec(128, iv));
                byte[] out = c.doFinal(big);
                ck += out[out.length - 1 - (i & 15)];
            }
            t[5] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < N; i++) {
                iv16[0] = (byte) i;
                Cipher c = Cipher.getInstance("AES/CBC/PKCS5Padding");
                c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, 0, 16, "AES"), new IvParameterSpec(iv16));
                byte[] out = c.doFinal(big);
                ck += out[out.length - 1 - (i & 15)];
            }
            t[6] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < N; i++) {
                iv[0] = (byte) i; iv[1] = (byte) r; iv[2] = 1;
                Cipher c = Cipher.getInstance("ChaCha20-Poly1305");
                c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, "ChaCha20"), new IvParameterSpec(iv));
                byte[] out = c.doFinal(big);
                ck += out[out.length - 1 - (i & 15)];
            }
            t[7] = (System.nanoTime() - s) / 1e6 / N;
            s = System.nanoTime();
            for (int i = 0; i < 400; i++) {
                iv[0] = (byte) i; iv[1] = (byte) (i >> 8); iv[2] = (byte) r; iv[3] = 2;
                Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
                c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, 0, 16, "AES"), new GCMParameterSpec(128, iv));
                c.updateAAD(small, 0, 13);
                byte[] out = c.doFinal(rec);
                ck += out[out.length - 1 - (i & 15)];
            }
            t[8] = (System.nanoTime() - s) / 1e3 / 400;
            StringBuilder sb = new StringBuilder("round " + r);
            for (int k = 0; k < names.length; k++) {
                best[k] = Math.min(best[k], t[k]);
                sb.append(String.format(" %s=%.2f", names[k], t[k]));
            }
            System.out.println(sb);
        }
        StringBuilder sb = new StringBuilder("BEST");
        for (int k = 0; k < names.length; k++) sb.append(String.format(" %s=%.2f", names[k], best[k]));
        System.out.println(sb + " ck=" + ck);
    }
}
