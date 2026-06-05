import java.security.Provider;
import java.security.Security;

public class SvcProbe {
    static void dump(String prov) {
        Provider p = Security.getProvider(prov);
        if (p == null) { System.out.println(prov + " = null"); return; }
        java.util.Set<Provider.Service> svcs = p.getServices();
        System.out.println(prov + " getServices().size() = " + (svcs == null ? "null" : svcs.size())
            + "  entrySet().size()=" + p.entrySet().size());
        if (svcs != null) {
            int n = 0;
            for (Provider.Service s : svcs) {
                System.out.println("    " + s.getType() + "." + s.getAlgorithm());
                if (++n >= 6) { System.out.println("    ..."); break; }
            }
        }
    }
    public static void main(String[] a) {
        dump("SUN");
        dump("SunEC");
        dump("SunRsaSign");
        dump("SunJCE");
        // Sanity: do non-EC services resolve?
        System.out.println("SUN MessageDigest.SHA-256 -> " +
            (Security.getProvider("SUN") != null ? Security.getProvider("SUN").getService("MessageDigest", "SHA-256") : "no SUN"));
    }
}
