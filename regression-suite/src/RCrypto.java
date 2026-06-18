import java.security.*;
import java.util.*;
import javax.crypto.*;
import javax.crypto.spec.*;

/**
 * Regression: JCA crypto — digests, HMAC, symmetric (AES-GCM) and asymmetric
 * (RSA OAEP + PKCS1 + sign/verify) round-trips. The RSA paths regressed once
 * (a constant-time-unpad bug broke decryption), so they are exercised here.
 */
public class RCrypto {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }
    static String hex(byte[] b) { StringBuilder s = new StringBuilder(); for (byte x : b) s.append(String.format("%02x", x)); return s.toString(); }

    public static void main(String[] a) throws Exception {
        // ---- SHA-256 known-answer (digest of "abc") ----
        byte[] sha = MessageDigest.getInstance("SHA-256").digest("abc".getBytes("UTF-8"));
        check(hex(sha).equals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"), "SHA-256 KAT");
        check(MessageDigest.getInstance("SHA-1").digest(new byte[0]).length == 20, "SHA-1 length");

        // ---- HMAC-SHA256 known-answer (key "key", msg "The quick brown fox jumps over the lazy dog") ----
        Mac mac = Mac.getInstance("HmacSHA256");
        mac.init(new SecretKeySpec("key".getBytes("UTF-8"), "HmacSHA256"));
        byte[] h = mac.doFinal("The quick brown fox jumps over the lazy dog".getBytes("UTF-8"));
        check(hex(h).equals("f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"), "HMAC-SHA256 KAT");

        // ---- AES-256-GCM round-trip (fixed key/IV → deterministic) ----
        byte[] keyBytes = new byte[32]; for (int i = 0; i < 32; i++) keyBytes[i] = (byte) (i + 1);
        SecretKey key = new SecretKeySpec(keyBytes, "AES");
        byte[] iv = new byte[12]; for (int i = 0; i < 12; i++) iv[i] = (byte) (i * 7);
        byte[] pt = "AES-GCM regression payload".getBytes("UTF-8");
        Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
        enc.init(Cipher.ENCRYPT_MODE, key, new GCMParameterSpec(128, iv));
        byte[] ct = enc.doFinal(pt);
        Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
        dec.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
        check(Arrays.equals(dec.doFinal(ct), pt), "AES-GCM round-trip");

        // ---- RSA-2048: OAEP + PKCS1 encrypt/decrypt + sign/verify ----
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA"); kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        byte[] msg = "rsa regression message".getBytes("UTF-8");

        Cipher rOaepE = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
        rOaepE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] oaepCt = rOaepE.doFinal(msg);
        Cipher rOaepD = Cipher.getInstance("RSA/ECB/OAEPWithSHA-256AndMGF1Padding");
        rOaepD.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        check(Arrays.equals(rOaepD.doFinal(oaepCt), msg), "RSA-OAEP round-trip");

        Cipher rPkcsE = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        rPkcsE.init(Cipher.ENCRYPT_MODE, kp.getPublic());
        byte[] pkcsCt = rPkcsE.doFinal(msg);
        Cipher rPkcsD = Cipher.getInstance("RSA/ECB/PKCS1Padding");
        rPkcsD.init(Cipher.DECRYPT_MODE, kp.getPrivate());
        check(Arrays.equals(rPkcsD.doFinal(pkcsCt), msg), "RSA-PKCS1 round-trip");

        Signature sig = Signature.getInstance("SHA256withRSA");
        sig.initSign(kp.getPrivate()); sig.update(msg);
        byte[] sigBytes = sig.sign();
        Signature ver = Signature.getInstance("SHA256withRSA");
        ver.initVerify(kp.getPublic()); ver.update(msg);
        check(ver.verify(sigBytes), "RSA sign/verify");

        System.out.println("PASS RCrypto (" + checks + " checks)");
    }
}
