import java.security.KeyStore;
import java.security.Provider;
import java.security.Security;
import org.wildfly.security.keystore.AtomicLoadKeyStore;

/** Contract probe for the WildFly Elytron provider-qualified JKS path. */
public final class ElytronKeyStoreProviderProbe {
    private static void require(boolean condition, String message) {
        if (!condition) throw new AssertionError(message);
    }

    public static void main(String[] args) throws Exception {
        Provider[] jksProviders = Security.getProviders("KeyStore.JKS");
        require(jksProviders != null && jksProviders.length == 1,
                "KeyStore.JKS filter must return exactly SUN");
        Provider sun = jksProviders[0];
        require("SUN".equals(sun.getName()), "JKS filter must resolve SUN");

        Provider[] digestProviders = Security.getProviders("MessageDigest.SHA-256");
        require(digestProviders != null && digestProviders.length >= 1,
                "MessageDigest.SHA-256 filter must resolve a provider");
        Provider.Service direct = sun.getService("KeyStore", "JKS");
        require(direct != null && direct.toString().contains("KeyStore.JKS"),
                "JKS service must render safely");

        int servicesVisited = 0;
        for (Provider.Service service : sun.getServices()) {
            require(!service.toString().isEmpty(), "Provider service must render safely");
            servicesVisited++;
        }
        require(servicesVisited >= 1, "SUN service enumeration must include JKS");

        KeyStore directKs = KeyStore.getInstance("JKS", sun);
        directKs.load(null, "changeit".toCharArray());
        require(directKs.size() == 0, "provider-qualified blank JKS must load empty");
        Provider defaultProvider = KeyStore.getInstance("JKS").getProvider();
        require("SUN".equals(defaultProvider.getName()), "default JKS must retain SUN provider");
        KeyStore defaultKs = KeyStore.getInstance("JKS", defaultProvider);
        defaultKs.load(null, "changeit".toCharArray());
        require(defaultKs.size() == 0, "default-provider JKS must load empty");

        AtomicLoadKeyStore atomicDefault = AtomicLoadKeyStore.newInstance("JKS");
        atomicDefault.load(null, "changeit".toCharArray());
        require(atomicDefault.size() == 0,
                "Elytron default-provider AtomicLoadKeyStore must load empty");

        AtomicLoadKeyStore atomic = AtomicLoadKeyStore.newInstance("JKS", defaultProvider);
        atomic.load(null, "changeit".toCharArray());
        require(atomic.size() == 0, "Elytron AtomicLoadKeyStore must load empty");

        System.out.println("@@RESULT jksProviders=" + jksProviders.length
                + " digestProviders=" + digestProviders.length
                + " servicesVisited=" + servicesVisited
                + " provider=" + sun + " keystoreSize=" + atomic.size());
    }
}
