import java.security.*;
import javax.crypto.*;
import javax.crypto.spec.*;
import java.security.spec.ECGenParameterSpec;

// Smoke test that the real-JCA default doesn't break RSA / AES / EC / digest.
public class CryptoSmoke {
    static void t(String name, Runnable r) {
        try { r.run(); System.out.println("OK   " + name); }
        catch (Throwable e) { System.out.println("FAIL " + name + " -> " + e); }
    }
    public static void main(String[] a) {
        t("MessageDigest SHA-256", () -> { try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest("x".getBytes());
            if (d.length != 32) throw new RuntimeException("len " + d.length);
        } catch (Exception e) { throw new RuntimeException(e); } });

        t("AES-GCM round-trip", () -> { try {
            KeyGenerator kg = KeyGenerator.getInstance("AES"); kg.init(128);
            SecretKey k = kg.generateKey();
            Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
            byte[] iv = new byte[12]; new SecureRandom().nextBytes(iv);
            c.init(Cipher.ENCRYPT_MODE, k, new GCMParameterSpec(128, iv));
            byte[] ct = c.doFinal("hello".getBytes());
            Cipher d = Cipher.getInstance("AES/GCM/NoPadding");
            d.init(Cipher.DECRYPT_MODE, k, new GCMParameterSpec(128, iv));
            if (!new String(d.doFinal(ct)).equals("hello")) throw new RuntimeException("mismatch");
        } catch (Exception e) { throw new RuntimeException(e); } });

        t("RSA keygen + sign/verify", () -> { try {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("RSA"); kg.initialize(2048);
            KeyPair kp = kg.generateKeyPair();
            Signature s = Signature.getInstance("SHA256withRSA");
            s.initSign(kp.getPrivate()); s.update("m".getBytes());
            byte[] sig = s.sign();
            Signature v = Signature.getInstance("SHA256withRSA");
            v.initVerify(kp.getPublic()); v.update("m".getBytes());
            if (!v.verify(sig)) throw new RuntimeException("verify false");
        } catch (Exception e) { throw new RuntimeException(e); } });

        t("EC keygen + ECDSA + getParams", () -> { try {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("EC");
            kg.initialize(new ECGenParameterSpec("secp256r1"), new SecureRandom());
            KeyPair kp = kg.generateKeyPair();
            java.security.interfaces.ECPublicKey pub = (java.security.interfaces.ECPublicKey) kp.getPublic();
            if (pub.getParams().getCurve().getField().getFieldSize() != 256) throw new RuntimeException("not P-256");
            Signature s = Signature.getInstance("SHA256withECDSA");
            s.initSign(kp.getPrivate()); s.update("m".getBytes());
            byte[] sig = s.sign();
            Signature v = Signature.getInstance("SHA256withECDSA");
            v.initVerify(kp.getPublic()); v.update("m".getBytes());
            if (!v.verify(sig)) throw new RuntimeException("verify false");
        } catch (Exception e) { throw new RuntimeException(e); } });

        t("SecureRandom nextBytes", () -> {
            byte[] b = new byte[16]; new SecureRandom().nextBytes(b);
        });
        System.out.println("DONE");
    }
}
