import java.lang.reflect.Field;
import java.util.Map;

import org.apache.catalina.Service;
import org.apache.catalina.connector.Connector;
import org.apache.catalina.startup.Tomcat;

import org.springframework.boot.tomcat.TomcatWebServer;
import org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactory;
import org.springframework.boot.web.server.WebServer;
import org.springframework.boot.web.servlet.ServletContextInitializer;

/**
 * The sequence every `AbstractClientHttpRequestFactoryBuilderTests` redirect
 * test runs: a port-0 `TomcatServletWebServerFactory`, `getWebServer`, `start`,
 * `getPort`.
 *
 * `TomcatWebServer` parks the service's connectors in a
 * `Map<Service, Connector[]>` during context start and puts them back in
 * `start()`. If they do not come back, `Tomcat.getConnector()` fabricates a
 * port-8080 connector on the already-running service, which then fails to bind
 * — the shape seen in the suite logs. This prints what the service holds at
 * every step so the losing step names itself.
 */
public final class TomcatWebServerConnectorProbe {

	public static void main(String[] args) throws Exception {
		TomcatServletWebServerFactory factory = new TomcatServletWebServerFactory(0);
		WebServer webServer = factory.getWebServer(new ServletContextInitializer[0]);
		TomcatWebServer tomcatWebServer = (TomcatWebServer) webServer;
		Tomcat tomcat = tomcatWebServer.getTomcat();

		report("after getWebServer", tomcat, tomcatWebServer);
		probeParkedLookup(tomcat, tomcatWebServer);
		webServer.start();
		report("after start", tomcat, tomcatWebServer);
		System.out.println("getPort()=" + webServer.getPort());
		webServer.stop();
		// Tomcat's non-daemon await thread outlives stop(); this is a probe, not
		// a server.
		System.exit(0);
	}

	private static void report(String stage, Tomcat tomcat, TomcatWebServer webServer) throws Exception {
		StringBuilder line = new StringBuilder(stage).append(": ");
		Service[] services = tomcat.getServer().findServices();
		line.append("server=").append(id(tomcat.getServer())).append(" findServices=").append(services.length)
				.append(' ');
		for (Service candidate : services) {
			line.append(id(candidate)).append(' ');
		}
		Service service = tomcat.getService();
		Connector[] connectors = service.findConnectors();
		line.append("service=").append(id(service)).append(" connectors=").append(connectors.length).append(" [");
		for (int i = 0; i < connectors.length; i++) {
			if (i > 0) {
				line.append(", ");
			}
			line.append(connectors[i].getPort());
		}
		line.append("] parked=").append(describeParked(webServer));
		System.out.println(line);
	}

	/**
	 * The exact lookup `addPreviouslyRemovedConnectors` performs: for every
	 * service the server reports, ask the parked map for that service's
	 * connectors.
	 */
	@SuppressWarnings("unchecked")
	private static void probeParkedLookup(Tomcat tomcat, TomcatWebServer webServer) throws Exception {
		Field field = TomcatWebServer.class.getDeclaredField("serviceConnectors");
		field.setAccessible(true);
		Map<Service, Connector[]> parked = (Map<Service, Connector[]>) field.get(webServer);
		Service parkedKey = parked.keySet().iterator().next();
		for (Service service : tomcat.getServer().findServices()) {
			Connector[] found = parked.get(service);
			System.out.println("parked lookup: service=" + id(service) + " sameAsParkedKey="
					+ (service == parkedKey) + " equals=" + service.equals(parkedKey) + " hash=" + service.hashCode()
					+ " parkedKeyHash=" + parkedKey.hashCode() + " -> "
					+ ((found == null) ? "NULL" : String.valueOf(found.length)));
		}
		System.out.println("parked containsKey(parkedKey)=" + parked.containsKey(parkedKey) + " get(parkedKey)="
				+ ((parked.get(parkedKey) == null) ? "NULL" : parked.get(parkedKey).length));
		System.out.println("parked impl=" + parked.getClass().getName() + " size=" + parked.size() + " isEmpty="
				+ parked.isEmpty() + " keySet.size=" + parked.keySet().size() + " entrySet.size="
				+ parked.entrySet().size());
		for (Map.Entry<Service, Connector[]> entry : parked.entrySet()) {
			Service key = entry.getKey();
			System.out.println("  entry key=" + id(key) + " keyHash=" + key.hashCode() + " sameAsIteratorKey="
					+ (key == parkedKey) + " reGet=" + (parked.get(key) == null ? "NULL" : "ok"));
		}
		Map<Object, String> control = new java.util.HashMap<>();
		control.put(parkedKey, "control");
		System.out.println("fresh HashMap with the same key: containsKey=" + control.containsKey(parkedKey) + " get="
				+ control.get(parkedKey));
		// If the stored entry is filed under a different hash than the key
		// answers now, re-putting the SAME key adds a SECOND entry. Only worth
		// asking when the lookup already failed — the put is destructive, and
		// on a healthy VM it would strand the connectors this probe wants
		// `start()` to restore.
		if (parked.get(parkedKey) == null) {
			((Map<Service, Connector[]>) parked).put(parkedKey, new Connector[0]);
			System.out.println("after re-put with the same key: size=" + parked.size() + " (1 means the stored entry "
					+ "was found and replaced; 2 means it is filed under a stale hash)");
		}
		safeDumpTable("control", control);
		safeDumpTable("parked", parked);
	}

	private static void safeDumpTable(String label, Map<?, ?> map) {
		try {
			dumpTable(label, map);
		}
		catch (Exception ex) {
			System.out.println(label + " dumpTable failed: " + ex);
		}
	}

	/**
	 * The stored `Node.hash` is what `getNode` compares against
	 * `hash(key)`; printing both says whether the key's hash moved after the
	 * entry was written or the table itself is at fault.
	 */
	private static void dumpTable(String label, Map<?, ?> map) throws Exception {
		Field tableField = java.util.HashMap.class.getDeclaredField("table");
		tableField.setAccessible(true);
		Object[] table = (Object[]) tableField.get(map);
		if (table == null) {
			System.out.println(label + " table=null");
			return;
		}
		Field nodeHash = null;
		Field nodeKey = null;
		System.out.println(label + " table.length=" + table.length);
		for (int i = 0; i < table.length; i++) {
			Object node = table[i];
			while (node != null) {
				if (nodeHash == null) {
					System.out.println(label + " node class=" + node.getClass().getName() + " fields="
							+ java.util.Arrays.toString(node.getClass().getDeclaredFields()));
					nodeHash = findField(node.getClass(), "hash");
					nodeHash.setAccessible(true);
					nodeKey = findField(node.getClass(), "key");
					nodeKey.setAccessible(true);
				}
				int stored = nodeHash.getInt(node);
				Object key = nodeKey.get(node);
				int live = key.hashCode();
				System.out.println("  " + label + " bucket=" + i + " storedHash=" + stored + " liveHashCode=" + live
						+ " spread=" + (live ^ (live >>> 16)) + " match=" + (stored == (live ^ (live >>> 16)))
						+ " bucketForLive=" + ((table.length - 1) & (live ^ (live >>> 16))));
				Field next = findField(node.getClass(), "next");
				next.setAccessible(true);
				node = next.get(node);
			}
		}
	}

	@SuppressWarnings("unchecked")
	private static String describeParked(TomcatWebServer webServer) throws Exception {
		Field field = TomcatWebServer.class.getDeclaredField("serviceConnectors");
		field.setAccessible(true);
		Map<Service, Connector[]> parked = (Map<Service, Connector[]>) field.get(webServer);
		StringBuilder out = new StringBuilder("size=").append(parked.size());
		for (Map.Entry<Service, Connector[]> entry : parked.entrySet()) {
			out.append(" {key=").append(id(entry.getKey())).append(" n=").append(entry.getValue().length).append('}');
		}
		return out.toString();
	}

	private static Field findField(Class<?> type, String name) throws NoSuchFieldException {
		for (Class<?> c = type; c != null; c = c.getSuperclass()) {
			try {
				return c.getDeclaredField(name);
			}
			catch (NoSuchFieldException ex) {
				// keep walking
			}
		}
		throw new NoSuchFieldException(name + " not found on " + type.getName() + " or its supertypes");
	}

	private static String id(Object o) {
		return (o == null) ? "null"
				: o.getClass().getSimpleName() + "@" + Integer.toHexString(System.identityHashCode(o));
	}

}
