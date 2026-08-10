import org.apache.catalina.Service;
import org.apache.catalina.connector.Connector;
import org.apache.catalina.startup.Tomcat;

/**
 * Paired probe for {@code StandardService.addConnector} /
 * {@code findConnectors()} and the {@code Tomcat.getConnector()} fallback,
 * diffed against the host JDK.
 *
 * `Tomcat.getConnector()` is not a getter. When `service.findConnectors()`
 * comes back empty it *fabricates* a connector on port 8080 and adds it to the
 * service. Spring Boot's `TomcatWebServer.initialize()` calls it, so a
 * `findConnectors()` that under-reports turns "start my ephemeral server" into
 * "also start a second server on 8080" — which then collides with anything
 * already on 8080 and fails the whole start.
 *
 * That is the observed CratonVM behaviour: `SpringFactoryPortProbe` logs
 * `Initializing ProtocolHandler ["http-nio-auto-1"]` (the correct ephemeral
 * connector) immediately followed by `["http-nio-8080"]` (the fabricated one),
 * and the 8080 one fails.
 *
 * The probe reads the array back after every mutation, and prints identity and
 * length rather than a verdict, because "the connector I added is in there" and
 * "the array has the right length" are different claims and the bug can break
 * either.
 */
public class TomcatFindConnectorsProbe {

    static String describe(Connector[] cs) {
        if (cs == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder("len=").append(cs.length).append(" [");
        for (int i = 0; i < cs.length; i++) {
            if (i > 0) {
                sb.append(", ");
            }
            sb.append(cs[i] == null ? "null" : (cs[i].getPort() + "@" + System.identityHashCode(cs[i])));
        }
        return sb.append("]").toString();
    }

    public static void main(String[] args) throws Exception {
        Tomcat tomcat = new Tomcat();
        tomcat.setBaseDir(System.getProperty("java.io.tmpdir"));
        Service service = tomcat.getService();
        System.out.println("service class            = " + service.getClass().getName());
        System.out.println("findConnectors() initial = " + describe(service.findConnectors()));

        Connector c = new Connector("org.apache.coyote.http11.Http11NioProtocol");
        c.setPort(0);
        System.out.println("created connector        = port " + c.getPort()
                + " id=" + System.identityHashCode(c));

        service.addConnector(c);
        System.out.println("after addConnector       = " + describe(service.findConnectors()));

        // Two consecutive reads: a view that is rebuilt per call and gets it
        // wrong only sometimes shows up here.
        System.out.println("second read              = " + describe(service.findConnectors()));

        // The fallback under test. If findConnectors() is right, this returns
        // the connector added above (port 0) and adds nothing. If it is wrong,
        // this returns a NEW connector on 8080 and the service now has two.
        Connector got = tomcat.getConnector();
        System.out.println("getConnector() port      = " + got.getPort()
                + " id=" + System.identityHashCode(got));
        System.out.println("same object as added?    = " + (got == c));
        System.out.println("after getConnector()     = " + describe(service.findConnectors()));

        // Add a second one, to check growth rather than just the empty->one step.
        Connector c2 = new Connector("org.apache.coyote.http11.Http11NioProtocol");
        c2.setPort(0);
        service.addConnector(c2);
        System.out.println("after 2nd addConnector   = " + describe(service.findConnectors()));

        service.removeConnector(c2);
        System.out.println("after removeConnector    = " + describe(service.findConnectors()));

        // Spring's `TomcatWebServer` parks removed connectors in a
        // `Map<Service, Connector[]>` and restores them on start() by iterating
        // `getServer().findServices()` and looking each one up in that map. The
        // two sides of that must agree on object identity: it stashes under one
        // Service reference and looks up under whatever `findServices()` hands
        // back. If those are different objects the lookup misses, no connector
        // is restored, and `Tomcat.getConnector()` fabricates one on 8080.
        org.apache.catalina.Server srv = tomcat.getServer();
        Service[] viaServer = srv.findServices();
        Service viaGetter = tomcat.getService();
        System.out.println("--- Service identity ---");
        System.out.println("  findServices().length    = " + viaServer.length);
        System.out.println("  getService() id          = " + System.identityHashCode(viaGetter));
        for (int i = 0; i < viaServer.length; i++) {
            System.out.println("  findServices()[" + i + "] id     = "
                    + System.identityHashCode(viaServer[i])
                    + " same=" + (viaServer[i] == viaGetter));
        }
        System.out.println("  getServer() twice same   = " + (srv == tomcat.getServer()));

        // And the map round-trip Spring actually performs.
        java.util.Map<Service, Connector[]> parked = new java.util.HashMap<>();
        parked.put(viaGetter, service.findConnectors());
        Connector[] back = parked.get(viaServer.length > 0 ? viaServer[0] : viaGetter);
        System.out.println("  park/restore round-trip  = " + describe(back));
    }
}
