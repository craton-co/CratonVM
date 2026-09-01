import java.security.Provider;
import java.security.Security;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Set;

/**
 * SunJCE is PRESENT in this VM's provider list and `KeyStore.getInstance("JCEKS")`
 * still fails "JCEKS not found", while JKS and PKCS12 both resolve through SUN.
 * So the gap is not the `java.security` file (it reads byte-identically to
 * HotSpot, 74132 bytes) and not a missing provider. It is inside the provider's
 * own service map.
 *
 * This asks the provider directly, three ways, because they fail differently:
 *   * `getServices()` -- the whole map, which says whether the provider was
 *     populated at all or is an empty shell that merely has the right name;
 *   * `getService("KeyStore", "JCEKS")` -- the exact lookup `KeyStore.getInstance`
 *     performs, on the provider that is supposed to answer it;
 *   * the `KeyStore.` property keys -- the older registration channel, in case
 *     the service map and the legacy properties disagree.
 */
public final class SunJceServices {
    static int n = 0;
    static void p(String l, Object v) { System.out.println(++n + " " + l + " |" + v + "|"); }

    public static void main(String[] args) {
        for (String name : new String[] {"SunJCE", "SUN"}) {
            Provider pr = Security.getProvider(name);
            p(name + " present", pr != null);
            if (pr == null) continue;
            p(name + " class", pr.getClass().getName());
            Set<Provider.Service> svcs = null;
            try {
                svcs = pr.getServices();
                p(name + " getServices size", svcs.size());
            } catch (Throwable t) {
                p(name + " getServices", t.getClass().getName() + ": " + t.getMessage());
            }
            if (svcs != null) {
                List<String> ks = new ArrayList<>();
                for (Provider.Service s : svcs) {
                    if ("KeyStore".equals(s.getType())) ks.add(s.getAlgorithm());
                }
                Collections.sort(ks);
                p(name + " KeyStore algorithms", ks);
                List<String> types = new ArrayList<>();
                for (Provider.Service s : svcs) {
                    if (!types.contains(s.getType())) types.add(s.getType());
                }
                Collections.sort(types);
                p(name + " service types", types.size() + " " + types);
            }
            try {
                Provider.Service s = pr.getService("KeyStore", "JCEKS");
                p(name + " getService KeyStore/JCEKS", s == null ? "null" : s.getClassName());
            } catch (Throwable t) {
                p(name + " getService KeyStore/JCEKS", t.getClass().getName() + ": " + t.getMessage());
            }
            p(name + " prop KeyStore.JCEKS", pr.getProperty("KeyStore.JCEKS"));
            p(name + " prop size", pr.size());
        }
        // Does the class the JDK would instantiate even exist here?
        try {
            Class<?> c = Class.forName("com.sun.crypto.provider.JceKeyStore");
            p("JceKeyStore class", c.getName());
        } catch (Throwable t) {
            p("JceKeyStore class", t.getClass().getName() + ": " + t.getMessage());
        }
    }
}
