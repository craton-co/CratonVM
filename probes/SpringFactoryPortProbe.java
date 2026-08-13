import org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactory;
import org.springframework.boot.web.server.WebServer;

/**
 * Paired probe for the port Spring Boot's Tomcat factory actually ends up on,
 * diffed against the host JDK.
 *
 * `AbstractServletWebServerFactoryTests` builds every server through
 * `getFactory()`, which the Tomcat subclass implements as
 * `new TomcatServletWebServerFactory(0)` — port 0, "pick an ephemeral port".
 * Under CratonVM that class produced `http-nio-8080` 99 times and 82 tests
 * failed with "Connector configured to listen on port 8080 failed to start";
 * under HotSpot it passes 129/129. The synthetic field-initializer-ordering
 * probe and the `Connector.setPort` probe both came back identical between the
 * two VMs, so the divergence is somewhere in the real type chain, not in
 * either mechanism in isolation.
 *
 * This asks the real objects, one layer at a time, so the answer names the
 * layer that first disagrees rather than just the outcome:
 *   factory.getPort()  ->  the started server's getPort()
 *
 * A `getPort()` of 0 before start and a non-zero, non-8080 port after start is
 * correct behaviour. 8080 anywhere is the bug.
 */
public class SpringFactoryPortProbe {

    public static void main(String[] args) throws Exception {
        TomcatServletWebServerFactory f0 = new TomcatServletWebServerFactory(0);
        System.out.println("new TomcatServletWebServerFactory(0).getPort()    = " + f0.getPort());

        TomcatServletWebServerFactory f1234 = new TomcatServletWebServerFactory(1234);
        System.out.println("new TomcatServletWebServerFactory(1234).getPort() = " + f1234.getPort());

        TomcatServletWebServerFactory fdef = new TomcatServletWebServerFactory();
        System.out.println("new TomcatServletWebServerFactory().getPort()     = " + fdef.getPort());

        TomcatServletWebServerFactory fset = new TomcatServletWebServerFactory(1234);
        fset.setPort(0);
        System.out.println("setPort(0) after ctor(1234).getPort()             = " + fset.getPort());

        // The part that matters: what the started server binds. Ephemeral means
        // a high port that is neither 0 nor 8080.
        TomcatServletWebServerFactory f = new TomcatServletWebServerFactory(0);
        System.out.println("before start: factory.getPort()                   = " + f.getPort());
        WebServer ws = f.getWebServer();
        dumpConnectors("after getWebServer()", ws);
        try {
            ws.start();
            dumpConnectors("after start()", ws);
            int bound = ws.getPort();
            System.out.println("started server getPort()                          = " + bound);
            System.out.println("ephemeral? (not 0, not 8080)                      = "
                    + (bound != 0 && bound != 8080));
        } finally {
            try {
                ws.stop();
            } catch (Exception ignored) {
                // best effort
            }
        }
    }

    /**
     * Enumerate the connectors the embedded Tomcat actually holds, and the
     * identity of the Service/Server they hang off.
     *
     * Both identities are printed because "the connector list is wrong" and
     * "there are two Service objects and the list was read from the other one"
     * produce the same missing connector, and only the identity tells them
     * apart.
     */
    static void dumpConnectors(String label, Object webServer) {
        try {
            Object tomcat = webServer.getClass().getMethod("getTomcat").invoke(webServer);
            Object server = tomcat.getClass().getMethod("getServer").invoke(tomcat);
            Object service = tomcat.getClass().getMethod("getService").invoke(tomcat);
            Object service2 = tomcat.getClass().getMethod("getService").invoke(tomcat);
            Object[] connectors = (Object[]) service.getClass()
                    .getMethod("findConnectors").invoke(service);
            StringBuilder sb = new StringBuilder();
            for (Object c : connectors) {
                int port = (Integer) c.getClass().getMethod("getPort").invoke(c);
                if (sb.length() > 0) {
                    sb.append(", ");
                }
                sb.append(port).append('@').append(System.identityHashCode(c));
            }
            System.out.println(label + ": connectors=" + connectors.length + " [" + sb + "]"
                    + " serverId=" + System.identityHashCode(server)
                    + " serviceId=" + System.identityHashCode(service)
                    + " sameServiceTwice=" + (service == service2));
        } catch (Throwable t) {
            System.out.println(label + ": <unavailable: " + t.getClass().getName() + ">");
        }
    }
}
