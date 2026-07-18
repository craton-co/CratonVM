package cratonvm;

import java.io.FileInputStream;
import java.security.KeyStore;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManagerFactory;
import com.unboundid.ldap.listener.InMemoryDirectoryServer;
import com.unboundid.ldap.listener.InMemoryDirectoryServerConfig;
import com.unboundid.ldap.listener.InMemoryListenerConfig;
import com.unboundid.ldap.listener.LDAPListenerClientConnection;
import com.unboundid.ldap.listener.LDAPListenerExceptionHandler;
import com.unboundid.ldap.sdk.LDAPException;

/**
 * Exercises the exact JKS-backed LDAPS factory arrangement used by Spring
 * Boot's EmbeddedLdapAutoConfigurationTests.  This deliberately uses
 * UnboundID's {@code getConnection("LDAPS")} path, which first asks a
 * SocketFactory for an unconnected socket and then falls back to its
 * InetAddress overload.
 */
public final class UnboundIdLdapsJks {

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            throw new IllegalArgumentException("expected JKS path");
        }
        char[] password = "secret".toCharArray();
        KeyStore keyStore = KeyStore.getInstance("JKS");
        try (FileInputStream input = new FileInputStream(args[0])) {
            keyStore.load(input, password);
        }
        KeyManagerFactory keyManagers = KeyManagerFactory.getInstance(
                KeyManagerFactory.getDefaultAlgorithm());
        keyManagers.init(keyStore, password);
        TrustManagerFactory trustManagers = TrustManagerFactory.getInstance(
                TrustManagerFactory.getDefaultAlgorithm());
        trustManagers.init(keyStore);
        SSLContext context = SSLContext.getInstance("TLSv1.2");
        context.init(keyManagers.getKeyManagers(), trustManagers.getTrustManagers(), null);

        InMemoryListenerConfig listener = InMemoryListenerConfig.createLDAPSConfig(
                "LDAPS", null, 0, context.getServerSocketFactory(), context.getSocketFactory());
        InMemoryDirectoryServerConfig configuration =
                new InMemoryDirectoryServerConfig("dc=spring,dc=org");
        configuration.setListenerConfigs(listener);
        configuration.setListenerExceptionHandler(new LDAPListenerExceptionHandler() {
            @Override
            public void connectionCreationFailure(java.net.Socket socket, Throwable failure) {
                System.err.println("LDAPS_CONNECTION_CREATION_FAILURE");
                failure.printStackTrace();
            }

            @Override
            public void connectionTerminated(LDAPListenerClientConnection connection, LDAPException failure) {
                System.err.println("LDAPS_CONNECTION_TERMINATED");
                failure.printStackTrace();
            }
        });
        InMemoryDirectoryServer server = new InMemoryDirectoryServer(configuration);
        server.startListening();
        try {
            if (server.getConnection("LDAPS").getSSLSession() == null) {
                throw new AssertionError("LDAPS connection did not negotiate SSL");
            }
            System.out.println("UNBOUNDID_LDAPS_JKS_OK");
        }
        finally {
            server.shutDown(true);
        }
    }
}
