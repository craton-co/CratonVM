import java.security.*;
import java.security.spec.ECGenParameterSpec;

// Full EC flow keycloak SdJwt needs: keygen + ECDSA sign + verify.
public class ECDSAProbe {
    public static void main(String[] a) {
        try {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("EC");
            kg.initialize(new ECGenParameterSpec("secp256r1"), new SecureRandom());
            KeyPair kp = kg.generateKeyPair();
            byte[] msg = "hello sd-jwt".getBytes();
            Signature s = Signature.getInstance("SHA256withECDSA");
            s.initSign(kp.getPrivate());
            s.update(msg);
            byte[] sig = s.sign();
            System.err.println("signed " + sig.length + " bytes, sigProvider=" + s.getProvider());
            Signature v = Signature.getInstance("SHA256withECDSA");
            v.initVerify(kp.getPublic());
            v.update(msg);
            boolean ok = v.verify(sig);
            System.err.println("verify=" + ok);
            System.err.println(ok ? "RESULT OK" : "RESULT FAIL (verify false)");
        } catch (Throwable t) {
            System.err.println("RESULT FAIL: " + t);
            Throwable c = t.getCause(); int d = 0;
            while (c != null && d++ < 8) { System.err.println("  caused by: " + c); c = c.getCause(); }
        }
        System.err.flush();
    }
}
