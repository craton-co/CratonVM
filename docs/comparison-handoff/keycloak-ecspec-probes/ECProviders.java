import java.security.Provider;
import java.security.Security;

public class ECProviders {
    public static void main(String[] a) {
        Provider[] ps = Security.getProviders();
        System.out.println("provider count = " + (ps == null ? "NULL" : ps.length));
        if (ps == null) return;
        for (Provider p : ps) {
            System.out.println("PROVIDER " + p.getName() + " v" + p.getVersionStr());
        }
        System.out.println("--- EC-related services per provider ---");
        String[] svc = {
            "AlgorithmParameters.EC", "KeyFactory.EC", "KeyPairGenerator.EC",
            "Signature.SHA256withECDSA", "KeyAgreement.ECDH",
        };
        for (Provider p : ps) {
            for (String s : svc) {
                int dot = s.indexOf('.');
                Provider.Service sv = p.getService(s.substring(0, dot), s.substring(dot + 1));
                if (sv != null) System.out.println("  " + p.getName() + " : " + s + " -> " + sv.getClassName());
            }
        }
        // Direct SunEC presence
        Provider sunec = Security.getProvider("SunEC");
        System.out.println("getProvider(SunEC) = " + sunec);
    }
}
