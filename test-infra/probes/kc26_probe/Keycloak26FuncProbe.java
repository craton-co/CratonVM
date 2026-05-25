import org.keycloak.common.Version;
import org.keycloak.common.Profile;
public class Keycloak26FuncProbe {
    public static void main(String[] args) {
        try {
            System.out.println("Keycloak name=" + Version.NAME);
            System.out.println("Keycloak version=" + Version.VERSION);
            // Functional check: enumerate the Profile.Feature enum so the
            // probe exercises the feature catalogue + its annotations.
            Profile.Feature[] features = Profile.Feature.values();
            int count = 0;
            for (Profile.Feature f : features) {
                if (f != null) count++;
            }
            System.out.println("Feature count: " + count);
            if (count == 0) {
                System.out.println("FAIL: no features enumerated");
                System.exit(1);
            }
            // Check a known feature exists.
            boolean found = false;
            for (Profile.Feature f : features) {
                if ("ACCOUNT_API".equals(f.name()) || "AUTHORIZATION".equals(f.name())) {
                    found = true;
                    break;
                }
            }
            if (!found) {
                System.out.println("FAIL: well-known feature ACCOUNT_API / AUTHORIZATION not found");
                System.exit(1);
            }
            System.out.println("OK");
        } catch (Throwable t) {
            t.printStackTrace();
            System.exit(1);
        }
        System.exit(0);
    }
}
