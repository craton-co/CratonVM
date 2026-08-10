import org.apache.catalina.connector.Connector;
import org.apache.coyote.ProtocolHandler;
import org.apache.tomcat.util.IntrospectionUtils;

/**
 * Paired probe for {@code Connector.setPort}, diffed against the host JDK.
 *
 * Spring Boot's `TomcatWebServerFactory.customizeConnector` does exactly
 * `connector.setPort(Math.max(getPort(), 0))`, and the test fixture builds its
 * factory with port **0** — "pick an ephemeral port". Under CratonVM the
 * resulting server came up as `http-nio-8080` (Tomcat's compiled-in default)
 * 99 times over, and 82 tests failed with "Connector configured to listen on
 * port 8080 failed to start". Under HotSpot the same class passes.
 *
 * Tomcat's `Connector` does not hold the port by itself: it forwards to the
 * ProtocolHandler through `IntrospectionUtils.setProperty`, i.e. by *name*,
 * reflectively. A by-name write that silently no-ops leaves the handler on its
 * own default and leaves `Connector.getPort()` reporting whatever the handler
 * says — which is precisely 8080.
 *
 * So the probe reads the value back at three levels rather than asserting the
 * call "worked": the setter's return, the Connector, and the ProtocolHandler.
 * A layer that agrees with the request while the layer below it does not is
 * the whole bug, and only reading all three shows it.
 *
 * Compile and run against the module's own test classpath:
 *   module/spring-boot-tomcat/build/cratonvm-test-cp.txt
 */
public class TomcatConnectorPortProbe {

    static String show(Object o) {
        return (o == null) ? "null" : String.valueOf(o);
    }

    public static void main(String[] args) throws Exception {
        for (int requested : new int[] { 0, 1234, 8080, -1 }) {
            Connector c = new Connector("org.apache.coyote.http11.Http11NioProtocol");
            ProtocolHandler ph = c.getProtocolHandler();

            System.out.println("--- requested port " + requested + " ---");
            System.out.println("  before connector.getPort() = " + c.getPort());
            System.out.println("  before handler port        = "
                    + show(IntrospectionUtils.getProperty(ph, "port")));

            c.setPort(requested);

            System.out.println("  after  connector.getPort() = " + c.getPort());
            System.out.println("  after  handler port        = "
                    + show(IntrospectionUtils.getProperty(ph, "port")));
            System.out.println("  handler name               = " + show(ph.getClass().getName()));
        }

        // The by-name write in isolation, with no Tomcat wrapper in the way, so
        // a failure here separates "IntrospectionUtils is broken" from
        // "Connector does not call it".
        Connector c = new Connector("org.apache.coyote.http11.Http11NioProtocol");
        ProtocolHandler ph = c.getProtocolHandler();
        boolean ok = IntrospectionUtils.setProperty(ph, "port", "4321");
        System.out.println("--- direct IntrospectionUtils.setProperty ---");
        System.out.println("  setProperty returned       = " + ok);
        System.out.println("  handler port readback      = "
                + show(IntrospectionUtils.getProperty(ph, "port")));
        System.out.println("  connector.getPort()        = " + c.getPort());
    }
}
