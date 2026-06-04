import java.security.*;
import java.security.spec.ECGenParameterSpec;
import java.security.interfaces.ECPublicKey;

// Isolates keycloak KeyUtils.generateEcKeyPair: KeyPairGenerator(EC) +
// ECGenParameterSpec(curve) + generateKeyPair() + ((ECPublicKey)pub).getParams()
public class ECKeyGenProbe {
    public static void main(String[] a) {
        String curve = a.length > 0 ? a[0] : "secp256r1";
        System.err.println("curve=" + curve);
        try {
            KeyPairGenerator kg = KeyPairGenerator.getInstance("EC");
            System.err.println("KPG provider=" + kg.getProvider() + " class=" + kg.getClass().getName());
            kg.initialize(new ECGenParameterSpec(curve), new SecureRandom());
            System.err.println("initialized");
            KeyPair kp = kg.generateKeyPair();
            System.err.println("keypair generated, pub=" + kp.getPublic().getClass().getName());
            ECPublicKey pub = (ECPublicKey) kp.getPublic();
            System.err.println("params=" + (pub.getParams() == null ? "NULL" : "non-null, bits=" + pub.getParams().getCurve().getField().getFieldSize()));
            System.err.println("RESULT OK");
        } catch (Throwable t) {
            System.err.println("RESULT FAIL: " + t);
            Throwable c = t.getCause();
            int depth = 0;
            while (c != null && depth++ < 8) { System.err.println("  caused by: " + c); c = c.getCause(); }
            t.printStackTrace();
        }
        System.err.flush();
    }
}
