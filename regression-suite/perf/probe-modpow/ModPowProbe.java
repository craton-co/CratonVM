// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.math.BigInteger;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.SecureRandom;
import java.security.Signature;
import java.util.Arrays;
import java.util.Random;
import javax.crypto.BadPaddingException;
import javax.crypto.Cipher;
import javax.crypto.IllegalBlockSizeException;

/**
 * The probe behind {@code perf/biginteger-modpow-has-no-montgomery-reduction-20260817}.
 *
 * Reproduces the doc's table row for row, and — because the doc's own warning is
 * that a subtly wrong Montgomery modPow produces plausible-looking wrong answers
 * — every timed loop is preceded by a correctness gate whose expected values are
 * computed by an independent route. A run that is fast and wrong fails loudly
 * instead of printing a good number.
 *
 * <pre>
 *   &lt;cratonvm&gt; --java-home &lt;jdk-25&gt; -cp &lt;this dir&gt; ModPowProbe [reps]
 *   java -cp &lt;this dir&gt; ModPowProbe [reps]      # HotSpot oracle
 * </pre>
 */
public final class ModPowProbe {

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        correctness();
        System.out.println("correctness: OK");
        modPow2048();
        probablePrime1024();
        rsaKeygen(reps);
        signSha256WithRsa();
        rsaCipherDecrypt();
    }

    // ------------------------------------------------------------------
    // Correctness gate. Runs before any timing.
    // ------------------------------------------------------------------

    /**
     * Every branch the Montgomery rewrite introduced: odd modulus (Montgomery),
     * even modulus (division fallback), negative base, negative exponent
     * (modInverse), zero/one exponent, modulus 1, and exponents whose top word
     * is mostly leading zeros.
     */
    private static void correctness() {
        // 1. Small closed-form cases with hand-checkable answers.
        expect(BigInteger.valueOf(2).modPow(BigInteger.TEN, BigInteger.valueOf(1000)), "24");
        expect(BigInteger.valueOf(-2).modPow(BigInteger.valueOf(3), BigInteger.valueOf(5)), "2");
        expect(BigInteger.valueOf(-2).modPow(BigInteger.valueOf(2), BigInteger.valueOf(5)), "4");
        expect(BigInteger.valueOf(7).modPow(BigInteger.ZERO, BigInteger.valueOf(13)), "1");
        expect(BigInteger.ZERO.modPow(BigInteger.valueOf(5), BigInteger.valueOf(13)), "0");
        expect(BigInteger.valueOf(123).modPow(BigInteger.valueOf(456), BigInteger.ONE), "0");
        // even moduli, including a power of two: the fallback arm
        expect(BigInteger.valueOf(3).modPow(BigInteger.valueOf(100), BigInteger.valueOf(1024)),
                bigPow(3, 100).mod(BigInteger.valueOf(1024)).toString());
        expect(BigInteger.valueOf(5).modPow(BigInteger.valueOf(77), BigInteger.valueOf(1000000008L)),
                bigPow(5, 77).mod(BigInteger.valueOf(1000000008L)).toString());

        // 2. Fermat: a^(p-1) == 1 (mod p) for prime p. An independent identity —
        //    it does not name any particular residue, so a wrong implementation
        //    cannot satisfy it by accident.
        BigInteger p = new BigInteger(
                "115792089237316195423570985008687907853269984665640564039457584007913129639747");
        Random rnd = new Random(20260817L);
        for (int i = 0; i < 8; i++) {
            BigInteger a = new BigInteger(200, rnd).mod(p);
            if (a.signum() == 0) continue;
            if (!a.modPow(p.subtract(BigInteger.ONE), p).equals(BigInteger.ONE)) {
                throw new AssertionError("Fermat failed for a=" + a);
            }
        }

        // 3. modPow against a from-scratch square-and-multiply that shares no
        //    code with it, over odd AND even moduli of assorted widths.
        for (int mbits : new int[] {8, 31, 32, 33, 64, 127, 128, 257}) {
            for (int t = 0; t < 6; t++) {
                BigInteger m = new BigInteger(mbits, rnd).setBit(mbits - 1);
                for (BigInteger mod : new BigInteger[] {m.setBit(0), m.setBit(0).add(BigInteger.ONE)}) {
                    if (mod.compareTo(BigInteger.ONE) <= 0) continue;
                    BigInteger base = new BigInteger(mbits, rnd);
                    if (t % 2 == 0) base = base.negate();
                    for (int ebits : new int[] {1, 17, 33, 70, 200}) {
                        BigInteger e = new BigInteger(ebits, rnd).setBit(ebits - 1);
                        BigInteger want = slowModPow(base, e, mod);
                        BigInteger got = base.modPow(e, mod);
                        if (!got.equals(want)) {
                            throw new AssertionError(
                                    base + "^" + e + " mod " + mod + " = " + got + ", want " + want);
                        }
                    }
                }
            }
        }

        // 4. Negative exponent: defined as modInverse(m).modPow(-e, m).
        BigInteger n = new BigInteger("1000000007");
        BigInteger b = BigInteger.valueOf(123456789);
        BigInteger negOne = b.modPow(BigInteger.valueOf(-1), n);
        if (!b.multiply(negOne).mod(n).equals(BigInteger.ONE)) {
            throw new AssertionError("b^-1 is not the inverse of b");
        }
        BigInteger negFive = b.modPow(BigInteger.valueOf(-5), n);
        if (!negFive.equals(b.modInverse(n).modPow(BigInteger.valueOf(5), n))) {
            throw new AssertionError("negative-exponent modPow disagrees with modInverse route");
        }
        // A non-invertible base must throw, not return a plausible number.
        try {
            BigInteger.valueOf(2).modPow(BigInteger.valueOf(-3), BigInteger.valueOf(4));
            throw new AssertionError("expected ArithmeticException for non-invertible base");
        } catch (ArithmeticException expected) {
            // ok
        }

        // 5. RSA algebra end to end: (m^e)^d == m (mod n), and the CRT halves.
        BigInteger pp = new BigInteger("177250851143413106261106096231831452933");
        BigInteger qq = new BigInteger("329539864610483636976818094878219646841");
        BigInteger nn = pp.multiply(qq);
        BigInteger ee = BigInteger.valueOf(65537);
        BigInteger phi = pp.subtract(BigInteger.ONE).multiply(qq.subtract(BigInteger.ONE));
        BigInteger dd = ee.modInverse(phi);
        for (int i = 0; i < 6; i++) {
            BigInteger msg = new BigInteger(200, rnd).mod(nn);
            BigInteger c = msg.modPow(ee, nn);
            if (!c.modPow(dd, nn).equals(msg)) throw new AssertionError("RSA round trip");
            BigInteger sig = msg.modPow(dd, nn);
            if (!sig.modPow(ee, nn).equals(msg)) throw new AssertionError("RSA sign/verify");
            if (!msg.modPow(dd.mod(pp.subtract(BigInteger.ONE)), pp).equals(sig.mod(pp))) {
                throw new AssertionError("CRT half mod p");
            }
            if (!msg.modPow(dd.mod(qq.subtract(BigInteger.ONE)), qq).equals(sig.mod(qq))) {
                throw new AssertionError("CRT half mod q");
            }
        }
    }

    /** Square-and-multiply written independently of anything under test. */
    private static BigInteger slowModPow(BigInteger base, BigInteger e, BigInteger m) {
        BigInteger result = BigInteger.ONE;
        BigInteger b = base.mod(m);
        for (int i = e.bitLength() - 1; i >= 0; i--) {
            result = result.multiply(result).mod(m);
            if (e.testBit(i)) result = result.multiply(b).mod(m);
        }
        return result;
    }

    private static BigInteger bigPow(int base, int e) {
        BigInteger r = BigInteger.ONE;
        for (int i = 0; i < e; i++) r = r.multiply(BigInteger.valueOf(base));
        return r;
    }

    private static void expect(BigInteger got, String want) {
        if (!got.toString().equals(want)) {
            throw new AssertionError("got " + got + ", want " + want);
        }
    }

    // ------------------------------------------------------------------
    // Timings — the doc's table.
    // ------------------------------------------------------------------

    private static void modPow2048() {
        Random rnd = new Random(1234567L);
        BigInteger m = BigInteger.probablePrime(1024, rnd).multiply(BigInteger.probablePrime(1024, rnd));
        BigInteger base = new BigInteger(2040, rnd);
        BigInteger e = new BigInteger(2048, rnd).setBit(2047).setBit(0);
        long t0 = System.nanoTime();
        BigInteger acc = BigInteger.ZERO;
        for (int i = 0; i < 20; i++) {
            acc = acc.xor(base.add(BigInteger.valueOf(i)).modPow(e, m));
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("modPow 2048-bit x20: " + ms + " ms  (checksum "
                + Integer.toHexString(acc.hashCode()) + ")");
    }

    private static void probablePrime1024() {
        Random rnd = new Random(76543L);
        long t0 = System.nanoTime();
        BigInteger p = BigInteger.probablePrime(1024, rnd);
        long ms = (System.nanoTime() - t0) / 1_000_000;
        if (!p.isProbablePrime(40)) throw new AssertionError("probablePrime returned a composite");
        System.out.println("probablePrime(1024): " + ms + " ms");
    }

    private static void rsaKeygen(int reps) throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048, SecureRandom.getInstance("SHA1PRNG"));
        StringBuilder sb = new StringBuilder("RSA-2048 keygen:");
        for (int i = 0; i < reps; i++) {
            long t0 = System.nanoTime();
            KeyPair kp = kpg.generateKeyPair();
            long ms = (System.nanoTime() - t0) / 1_000_000;
            if (kp.getPublic() == null || kp.getPrivate() == null) {
                throw new AssertionError("null keypair");
            }
            sb.append(' ').append(ms).append(i + 1 < reps ? " /" : " ms");
        }
        System.out.println(sb);
    }

    private static void signSha256WithRsa() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        byte[] msg = "the floor under every certificate test".getBytes("UTF-8");
        Signature signer = Signature.getInstance("SHA256withRSA");
        // Warm up and verify before timing: a fast wrong signature is the
        // failure this doc warns about.
        signer.initSign(kp.getPrivate());
        signer.update(msg);
        byte[] sig = signer.sign();
        Signature v = Signature.getInstance("SHA256withRSA");
        v.initVerify(kp.getPublic());
        v.update(msg);
        if (!v.verify(sig)) throw new AssertionError("signature does not verify");
        v.initVerify(kp.getPublic());
        v.update("tampered".getBytes("UTF-8"));
        if (v.verify(sig)) throw new AssertionError("signature verified a different message");

        long t0 = System.nanoTime();
        for (int i = 0; i < 10; i++) {
            signer.initSign(kp.getPrivate());
            signer.update(msg);
            signer.sign();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("SHA256withRSA sign x10: " + ms + " ms");
    }

    /**
     * RSA private-key DECRYPT — the other half of the private-key path, and the
     * one that reaches it through {@code javax.crypto.Cipher} rather than
     * {@code Signature}. Gated on a round trip and on a corrupted ciphertext
     * still raising {@link BadPaddingException}, because a CRT private op that
     * silently returns a wrong value would otherwise show up here only as a
     * pleasing number.
     */
    private static void rsaCipherDecrypt() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        byte[] msg = "the other half of the private-key path".getBytes("UTF-8");

        Cipher enc = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        enc.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] ct = enc.doFinal(msg);

        Cipher dec = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        dec.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        if (!Arrays.equals(dec.doFinal(ct), msg)) {
            throw new AssertionError("RSA decrypt did not round-trip");
        }
        // A flipped byte must still be refused, not decrypted to something.
        byte[] bad = ct.clone();
        bad[200] ^= 0x01;
        try {
            Cipher d2 = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            d2.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            d2.doFinal(bad);
            throw new AssertionError("a corrupted ciphertext must not decrypt");
        } catch (BadPaddingException | IllegalBlockSizeException expected) {
            // ok
        }

        long t0 = System.nanoTime();
        for (int i = 0; i < 10; i++) {
            Cipher d = Cipher.getInstance("RSA/ECB/PKCS1Padding");
            d.init(Cipher.DECRYPT_MODE, kp.getPrivate());
            d.doFinal(ct);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("RSA decrypt x10: " + ms + " ms");
    }
}
