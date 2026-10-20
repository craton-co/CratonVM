import java.security.MessageDigest;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.SecretKeySpec;

/**
 * SBR wave 1 lane JCA, measurement 7: the hot JCA engines on 1 MiB inputs.
 * Prints the best-of-rounds ms per operation for SHA-256, HMAC-SHA256 and
 * AES-128-GCM encrypt, plus a checksum so a wrong answer cannot pass for a
 * fast one. Interleave arms externally (A B A B ...), one process per arm.
 */
public class SbrW1JcaPerf {
    static final int N = Integer.getInteger("n", 8);
    static final int ROUNDS = Integer.getInteger("rounds", 5);

    public static void main(String[] a) throws Exception {
        byte[] big = new byte[1 << 20];
        for (int i = 0; i < big.length; i++) big[i] = (byte) (i * 7 + (i >>> 11));
        byte[] key = new byte[32];
        for (int i = 0; i < 32; i++) key[i] = (byte) (i + 3);
        byte[] iv = new byte[12];
        long ck = 0;
        double bestMd = 1e18, bestMac = 1e18, bestGcm = 1e18, bestSig = 1e18, bestSmall = 1e18;
        java.security.KeyPairGenerator kpg = java.security.KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        java.security.KeyPair kp = kpg.generateKeyPair();
        byte[] small = new byte[64];
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < N; i++) ck += MessageDigest.getInstance("SHA-256").digest(big)[i & 31];
            long t1 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                Mac m = Mac.getInstance("HmacSHA256");
                m.init(new SecretKeySpec(key, "HmacSHA256"));
                ck += m.doFinal(big)[i & 31];
            }
            long t2 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                iv[0] = (byte) i; iv[1] = (byte) r;
                Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
                c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(key, 0, 16, "AES"), new GCMParameterSpec(128, iv));
                byte[] out = c.doFinal(big);
                ck += out[out.length - 1 - (i & 15)];
            }
            long t3 = System.nanoTime();
            // RSA-2048 SHA256withRSA sign + verify of 1 KiB, 4x per unit.
            for (int i = 0; i < N * 4; i++) {
                java.security.Signature s = java.security.Signature.getInstance("SHA256withRSA");
                s.initSign(kp.getPrivate());
                s.update(big, i * 1024, 1024);
                byte[] g = s.sign();
                java.security.Signature v = java.security.Signature.getInstance("SHA256withRSA");
                v.initVerify(kp.getPublic());
                v.update(big, i * 1024, 1024);
                ck += v.verify(g) ? 1 : 0;
            }
            long t4 = System.nanoTime();
            // 64-byte SHA-256 x 20000: getInstance + digest, the per-message shape.
            for (int i = 0; i < 20000; i++) {
                small[0] = (byte) i;
                ck += MessageDigest.getInstance("SHA-256").digest(small)[i & 31];
            }
            long t5 = System.nanoTime();
            bestMd = Math.min(bestMd, (t1 - t0) / 1e6 / N);
            bestMac = Math.min(bestMac, (t2 - t1) / 1e6 / N);
            bestGcm = Math.min(bestGcm, (t3 - t2) / 1e6 / N);
            bestSig = Math.min(bestSig, (t4 - t3) / 1e6 / (N * 4));
            bestSmall = Math.min(bestSmall, (t5 - t4) / 1e3 / 20000);
            System.out.printf("round %d sha256=%.2f hmac=%.2f gcm=%.2f ms/MiB rsa=%.2f ms/op small=%.2f us/op%n", r, (t1 - t0) / 1e6 / N, (t2 - t1) / 1e6 / N, (t3 - t2) / 1e6 / N, (t4 - t3) / 1e6 / (N * 4), (t5 - t4) / 1e3 / 20000);
        }
        System.out.printf("BEST sha256=%.2f hmac=%.2f gcm=%.2f ms/MiB rsa=%.2f ms/op small=%.2f us/op ck=%d%n", bestMd, bestMac, bestGcm, bestSig, bestSmall, ck);
    }
}
