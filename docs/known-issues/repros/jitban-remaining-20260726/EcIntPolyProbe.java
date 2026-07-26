import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;

// SUNEC-INTPOLY repro: sun/security/util/math/intpoly/'s field arithmetic
// is reported to progressively corrupt P-384/P-521 curve field-element limb
// arrays under a repeated keygen+sign+verify mix, eventually failing
// "point NOT ON CURVE" inside ECOperations.multiply. Pure-JDK
// java.security API (KeyPairGenerator/Signature "EC"), no external jar
// needed -- exercises sun.security.util.math.intpoly internally.
public class EcIntPolyProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        String curve = args.length > 1 ? args[1] : "secp384r1";
        int failures = 0;

        byte[] message = "the quick brown fox jumps over the lazy dog".getBytes("UTF-8");

        for (int i = 0; i < iterations; i++) {
            KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
            kpg.initialize(new ECGenParameterSpec(curve));
            KeyPair kp = kpg.generateKeyPair();
            PrivateKey priv = kp.getPrivate();
            PublicKey pub = kp.getPublic();

            Signature signer = Signature.getInstance("SHA256withECDSA");
            signer.initSign(priv);
            signer.update(message);
            byte[] sig = signer.sign();

            Signature verifier = Signature.getInstance("SHA256withECDSA");
            verifier.initVerify(pub);
            verifier.update(message);
            boolean ok;
            try {
                ok = verifier.verify(sig);
            } catch (Exception e) {
                ok = false;
                if (failures < 5) {
                    System.out.println("VERIFY EXCEPTION at i=" + i + ": " + e);
                }
            }

            if (!ok) {
                failures++;
                if (failures <= 5) {
                    System.out.println("VERIFY FAILED at i=" + i);
                }
            }

            // Also verify against a mutated message fails (sanity: not a
            // rubber-stamp verifier).
            Signature verifier2 = Signature.getInstance("SHA256withECDSA");
            verifier2.initVerify(pub);
            verifier2.update("different message".getBytes("UTF-8"));
            boolean shouldFail;
            try {
                shouldFail = verifier2.verify(sig);
            } catch (Exception e) {
                shouldFail = false;
            }
            if (shouldFail) {
                failures++;
                if (failures <= 5) {
                    System.out.println("RUBBER-STAMP at i=" + i + " (wrong message verified true)");
                }
            }

            if (i % 20 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE curve=" + curve + " iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
