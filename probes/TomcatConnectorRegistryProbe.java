import org.apache.catalina.Server;
import org.apache.catalina.Service;
import org.apache.catalina.connector.Connector;
import org.apache.catalina.startup.Tomcat;

/**
 * `Tomcat.getConnector()` fabricates a port-8080 connector only when
 * `getService().findConnectors()` comes back empty. Spring Boot's
 * `TomcatServletWebServerFactory` adds its own port-0 connector first, so on
 * HotSpot that fabrication never happens — yet under CratonVM every
 * `AbstractClientHttpRequestFactoryBuilderTests` server came up with a second
 * `http-nio-8080` connector that then failed to start.
 *
 * This walks the same sequence and prints what the service actually holds at
 * each step, plus the identity of the Server/Service the accessors hand back.
 */
public final class TomcatConnectorRegistryProbe {

	public static void main(String[] args) {
		Tomcat tomcat = new Tomcat();
		tomcat.setBaseDir(System.getProperty("java.io.tmpdir") + "/tomcat-connector-registry-probe");

		Server server1 = tomcat.getServer();
		Server server2 = tomcat.getServer();
		System.out.println("server identity stable=" + (server1 == server2) + " (" + id(server1) + " vs "
				+ id(server2) + ")");
		System.out.println("server.findServices().length=" + server1.findServices().length);

		Service service1 = tomcat.getService();
		Service service2 = tomcat.getService();
		System.out.println("service identity stable=" + (service1 == service2) + " (" + id(service1) + " vs "
				+ id(service2) + ")");

		System.out.println("connectors before add=" + service1.findConnectors().length);

		Connector connector = new Connector("org.apache.coyote.http11.Http11NioProtocol");
		connector.setPort(0);
		service1.addConnector(connector);

		System.out.println("connectors after addConnector (same Service ref)=" + service1.findConnectors().length);
		System.out.println("connectors after addConnector (fresh getService())="
				+ tomcat.getService().findConnectors().length);

		tomcat.setConnector(connector);
		System.out.println("connectors after setConnector=" + tomcat.getService().findConnectors().length);

		Connector got = tomcat.getConnector();
		System.out.println("getConnector() returned port=" + got.getPort() + " sameInstance=" + (got == connector));
		System.out.println("connectors after getConnector()=" + tomcat.getService().findConnectors().length);
		for (Connector c : tomcat.getService().findConnectors()) {
			System.out.println("  connector port=" + c.getPort() + " protocol=" + c.getProtocol());
		}
	}

	private static String id(Object o) {
		return (o == null) ? "null"
				: o.getClass().getSimpleName() + "@" + Integer.toHexString(System.identityHashCode(o));
	}

}
