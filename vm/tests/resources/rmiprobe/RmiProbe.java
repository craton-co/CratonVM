import javax.management.remote.JMXConnectorFactory;
import javax.management.remote.JMXServiceURL;

/** RMIConnector.<clinit> defines PRef (extends the encapsulated ProxyRef) in the dynamic module jdk.remoteref. */
public class RmiProbe {
    public static void main(String[] a) throws Exception {
        try {
            JMXConnectorFactory.connect(new JMXServiceURL("service:jmx:rmi:///jndi/rmi://localhost:1/x"));
        } catch (java.io.IOException expected) {
            System.out.println("RMI-CONNECT: io-exception");
            return;
        }
        System.out.println("RMI-CONNECT: connected");
    }
}
