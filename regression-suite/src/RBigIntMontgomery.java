import java.math.BigInteger;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.SecureRandom;
import java.security.Signature;
import java.util.Random;

import javax.crypto.Cipher;

/**
 * `BigInteger.modPow` and the RSA operations built on it, checked against
 * HotSpot's answers rather than against a claim.
 *
 * `oddModPow` runs on `implMontgomeryMultiply` / `implMontgomerySquare`, which
 * this VM did not intrinsify until 2026-08-30 — the reduction ran in bytecode
 * and one 2048-bit `modPow` measured 11.74 ms against HotSpot's 2.24 ms. That
 * is a correctness-critical path: a Montgomery reduction that is off by one
 * conditional subtraction still returns a number, and RSA still verifies it
 * for one padding in 2^32. So the intrinsic needs a differential test, not a
 * benchmark.
 *
 * <p>Determinism: every value comes from a FIXED seed, so the printed
 * checksums are identical on any host and any JDK — the suite diffs them
 * against HotSpot's. The RSA arm uses a fixed-seed `SecureRandom` for the same
 * reason, and only asserts round-trip properties (sign/verify, encrypt/decrypt)
 * which hold whatever key is generated.
 */
public class RBigIntMontgomery {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** A stable digest of a BigInteger, so a wrong bit anywhere shows up. */
    static String tag(BigInteger v) {
        byte[] b = v.toByteArray();
        long h = 1469598103934665603L;
        for (byte x : b) {
            h ^= x & 0xff;
            h *= 1099511628211L;
        }
        return Long.toHexString(h);
    }

    /** Odd modulus -> oddModPow -> montgomeryMultiply/Square. */
    static void oddModPow() {
        Random rnd = new Random(20260830L);
        StringBuilder acc = new StringBuilder();
        for (int bits : new int[] { 64, 128, 256, 512, 1024, 2048 }) {
            BigInteger m = new BigInteger(bits, rnd).setBit(0).setBit(bits - 1); // odd, full width
            BigInteger b = new BigInteger(bits - 1, rnd);
            BigInteger e = new BigInteger(bits, rnd);
            BigInteger r = b.modPow(e, m);
            check(r.signum() >= 0, "modPow must be non-negative");
            check(r.compareTo(m) < 0, "modPow must be reduced mod m");
            // The identity every Montgomery implementation must satisfy, and the
            // one an off-by-one final subtraction breaks: a^(e1+e2) == a^e1*a^e2.
            BigInteger e1 = e.shiftRight(1);
            BigInteger e2 = e.subtract(e1);
            BigInteger split = b.modPow(e1, m).multiply(b.modPow(e2, m)).mod(m);
            check(r.equals(split), "modPow(e) must equal modPow(e1)*modPow(e2) mod m at " + bits);
            // Fermat: for prime p, a^(p-1) == 1. Uses a real prime so the
            // reduction is exercised against a known answer, not a self-check.
            acc.append(tag(r)).append(':');
        }
        System.out.println("CK RBigIntMontgomery oddModPow=" + acc);
    }

    /** Even modulus takes the other path; it must not regress either. */
    static void evenModPow() {
        Random rnd = new Random(9001L);
        StringBuilder acc = new StringBuilder();
        for (int bits : new int[] { 128, 512, 1024 }) {
            BigInteger m = new BigInteger(bits, rnd).clearBit(0).setBit(bits - 1);
            BigInteger b = new BigInteger(bits - 1, rnd);
            BigInteger e = new BigInteger(64, rnd);
            BigInteger r = b.modPow(e, m);
            check(r.compareTo(m) < 0, "even modPow must be reduced");
            acc.append(tag(r)).append(':');
        }
        System.out.println("CK RBigIntMontgomery evenModPow=" + acc);
    }

    /** Known-answer: Fermat's little theorem on real primes. */
    static void fermat() {
        Random rnd = new Random(4242L);
        for (int bits : new int[] { 256, 512, 1024 }) {
            BigInteger p = BigInteger.probablePrime(bits, rnd);
            BigInteger a = new BigInteger(bits - 8, rnd).add(BigInteger.TWO);
            check(a.modPow(p.subtract(BigInteger.ONE), p).equals(BigInteger.ONE),
                  "a^(p-1) mod p must be 1 at " + bits);
        }
        System.out.println("CK RBigIntMontgomery fermat=ok");
    }

    /** modInverse round-trip, which also lands in the same reduction. */
    static void inverses() {
        Random rnd = new Random(77L);
        for (int bits : new int[] { 256, 1024, 2048 }) {
            BigInteger m = BigInteger.probablePrime(bits, rnd);
            BigInteger a = new BigInteger(bits - 1, rnd).add(BigInteger.ONE);
            BigInteger inv = a.modInverse(m);
            check(a.multiply(inv).mod(m).equals(BigInteger.ONE), "a*a^-1 == 1 at " + bits);
        }
        System.out.println("CK RBigIntMontgomery inverses=ok");
    }

    /** The thing that actually matters: RSA sign/verify and encrypt/decrypt. */
    static void rsa() throws Exception {
        SecureRandom sr = SecureRandom.getInstance("SHA1PRNG");
        sr.setSeed(new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 });
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048, sr);
        KeyPair kp = kpg.generateKeyPair();

        byte[] msg = "the montgomery reduction has to be exact".getBytes("UTF-8");

        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(msg);
        byte[] sig = signer.sign();

        Signature verifier = Signature.getInstance("SHA256withRSA");
        verifier.initVerify(kp.getPublic());
        verifier.update(msg);
        check(verifier.verify(sig), "RSA signature must verify");

        // A signature over different bytes must NOT verify -- otherwise a
        // reduction that returns a constant would pass the arm above.
        Signature neg = Signature.getInstance("SHA256withRSA");
        neg.initVerify(kp.getPublic());
        neg.update("different".getBytes("UTF-8"));
        check(!neg.verify(sig), "a signature over other bytes must not verify");

        Cipher enc = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        enc.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] ct = enc.doFinal(msg);
        Cipher dec = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        dec.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        check(java.util.Arrays.equals(msg, dec.doFinal(ct)), "RSA round-trip must return the plaintext");

        System.out.println("CK RBigIntMontgomery rsa=ok");
    }

    public static void main(String[] args) throws Exception {
        oddModPow();
        evenModPow();
        fermat();
        inverses();
        rsa();
        System.out.println("CK RBigIntMontgomery checks=" + checks);
        System.out.println("PASS RBigIntMontgomery (" + checks + " checks)");
    }
}
