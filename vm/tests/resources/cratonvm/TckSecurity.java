package cratonvm;

import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.KeyStore;

/**
 * JCK-style conformance tests for java.security (NEW-16.2).
 *
 * All tests are static, take no arguments, and return 1 on pass / 0 on fail.
 * Tests exercise standard algorithm identifiers guaranteed by the platform
 * (JCA: "SHA-256", "SHA-1", "MD5", and SecureRandom).
 */
public class TckSecurity {

    // SHA-256 digest of empty input is a well-known constant (32 bytes).
    public static int md_sha256_empty() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] out = md.digest(new byte[0]);
            if (out == null) return 0;
            if (out.length != 32) return 0;
            // First byte of SHA-256("") is 0xE3
            if ((out[0] & 0xFF) != 0xE3) return 0;
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // SHA-256 of "abc" has known first byte 0xBA.
    public static int md_sha256_abc() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] out = md.digest("abc".getBytes());
            if (out.length != 32) return 0;
            if ((out[0] & 0xFF) != 0xBA) return 0;
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // SHA-1 digest length = 20
    public static int md_sha1_length() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-1");
            byte[] out = md.digest(new byte[] { 1, 2, 3 });
            if (out.length != 20) return 0;
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // MD5 digest length = 16
    public static int md_md5_length() {
        try {
            MessageDigest md = MessageDigest.getInstance("MD5");
            byte[] out = md.digest(new byte[] { 1, 2, 3 });
            if (out.length != 16) return 0;
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // getAlgorithm returns the requested name
    public static int md_getAlgorithm() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            if (!"SHA-256".equals(md.getAlgorithm())) return 0;
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // reset() clears accumulated state — two digests must match
    public static int md_reset() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update((byte) 1);
            md.reset();
            byte[] a = md.digest(new byte[] { 2, 3 });
            byte[] b = MessageDigest.getInstance("SHA-256").digest(new byte[] { 2, 3 });
            if (a.length != b.length) return 0;
            for (int i = 0; i < a.length; i++) {
                if (a[i] != b[i]) return 0;
            }
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // Unknown algorithm throws NoSuchAlgorithmException
    public static int md_unknown_throws() {
        try {
            MessageDigest.getInstance("NOT-A-REAL-ALG-999");
            return 0;
        } catch (NoSuchAlgorithmException e) {
            return 1;
        }
    }

    // SecureRandom produces non-all-zero output
    public static int sr_nextBytes() {
        SecureRandom sr = new SecureRandom();
        byte[] buf = new byte[32];
        sr.nextBytes(buf);
        int nonZero = 0;
        for (byte b : buf) {
            if (b != 0) nonZero++;
        }
        // Extremely unlikely for a correct PRNG to produce all zeros
        if (nonZero == 0) return 0;
        return 1;
    }

    // SecureRandom.nextInt produces a full 32-bit range (just check doesn't crash)
    public static int sr_nextInt() {
        SecureRandom sr = new SecureRandom();
        int a = sr.nextInt();
        int b = sr.nextInt();
        // Vanishingly unlikely both are exactly zero
        if (a == 0 && b == 0) return 0;
        return 1;
    }

    // MessageDigest.update with single byte then digest
    public static int md_incremental_update() {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            md.update((byte) 'a');
            md.update((byte) 'b');
            md.update((byte) 'c');
            byte[] out1 = md.digest();
            byte[] out2 = MessageDigest.getInstance("SHA-256").digest("abc".getBytes());
            if (out1.length != out2.length) return 0;
            for (int i = 0; i < out1.length; i++) {
                if (out1[i] != out2[i]) return 0;
            }
            return 1;
        } catch (NoSuchAlgorithmException e) {
            return 0;
        }
    }

    // T4.5: Cipher.getInstance for AES/GCM/NoPadding
    public static int cipher_getInstance() {
        try {
            return javax.crypto.Cipher.getInstance("AES/GCM/NoPadding") != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: Cipher.getAlgorithm preserves algorithm name
    public static int cipher_getAlgorithm() {
        try {
            javax.crypto.Cipher c = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding");
            return "AES/GCM/NoPadding".equals(c.getAlgorithm()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: KeyStore.getInstance for JKS
    public static int keyStore_getInstance() {
        try {
            return KeyStore.getInstance("JKS") != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: Signature.getInstance for SHA256withRSA
    public static int signature_getInstance() {
        try {
            return Signature.getInstance("SHA256withRSA") != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: Signature.getAlgorithm preserves algorithm name
    public static int signature_getAlgorithm() {
        try {
            Signature sig = Signature.getInstance("SHA256withRSA");
            return "SHA256withRSA".equals(sig.getAlgorithm()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: Security.getProviders returns at least one provider with a name
    public static int provider_getName() {
        try {
            java.security.Provider[] providers = java.security.Security.getProviders();
            if (providers == null || providers.length == 0) return 0;
            String name = providers[0].getName();
            return (name != null && name.length() > 0) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: KeyPairGenerator.getInstance for RSA
    public static int keypairgen_getInstance() {
        try {
            return KeyPairGenerator.getInstance("RSA") != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: KeyPairGenerator.getAlgorithm preserves algorithm name
    public static int keypairgen_getAlgorithm() {
        try {
            KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
            return "RSA".equals(kpg.getAlgorithm()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // T4.5: Mac.getInstance for HmacSHA256
    public static int mac_getInstance() {
        try {
            return javax.crypto.Mac.getInstance("HmacSHA256") != null ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
}
