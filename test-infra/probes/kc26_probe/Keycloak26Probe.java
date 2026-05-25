import org.keycloak.common.Version;
public class Keycloak26Probe {
    public static void main(String[] args) {
        try {
            System.out.println("Keycloak name: " + Version.NAME);
            System.out.println("Keycloak version: " + Version.VERSION);
            System.out.println("OK");
        } catch (Throwable t) { t.printStackTrace(); }
    }
}
