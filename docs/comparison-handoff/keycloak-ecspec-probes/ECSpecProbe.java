import java.security.AlgorithmParameters;
import java.security.Provider;
import java.security.Security;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECParameterSpec;

// Minimal probe for the keycloak SdJwt "Error obtaining ECParameterSpec for
// P-256 curve" failure: the standard SunEC named-curve -> ECParameterSpec path.
public class ECSpecProbe {
    public static void main(String[] a) {
        // Which providers claim AlgorithmParameters.EC?
        try {
            for (Provider p : Security.getProviders()) {
                if (p.getService("AlgorithmParameters", "EC") != null) {
                    System.out.println("provider for AlgorithmParameters.EC: " + p.getName());
                }
            }
        } catch (Throwable t) { System.out.println("provider scan threw: " + t); }

        try {
            AlgorithmParameters ap = AlgorithmParameters.getInstance("EC");
            System.out.println("AlgorithmParameters.getInstance(EC) provider=" + ap.getProvider());
            ap.init(new ECGenParameterSpec("secp256r1"));
            System.out.println("init(secp256r1) OK");
            ECParameterSpec sp = ap.getParameterSpec(ECParameterSpec.class);
            System.out.println("spec=" + (sp == null ? "NULL" : "non-null"));
            if (sp != null) {
                System.out.println("  field bits=" + sp.getCurve().getField().getFieldSize());
                System.out.println("  order=" + sp.getOrder().bitLength() + " bits");
                System.out.println("  cofactor=" + sp.getCofactor());
            }
            System.out.println("RESULT: OK");
        } catch (Throwable t) {
            System.out.println("RESULT: FAILED -> " + t);
            t.printStackTrace();
        }
    }
}
