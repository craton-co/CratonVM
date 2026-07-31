import java.io.OutputStream;
import java.net.InetSocketAddress;

import com.sun.net.httpserver.HttpServer;

import org.springframework.http.ResponseEntity;
import org.springframework.web.client.RestClient;

/**
 * Minimal stand-in for AbstractRequestMappingIntegrationTests.performGet: a
 * plain com.sun HttpServer plus the RestClient the Spring tests build with
 * `RestClient.builder().baseUrl(...).build()`, so whichever request factory
 * that picks on this classpath is the one under test.
 */
public class RcProbe {

	public static void main(String[] args) throws Exception {
		HttpServer server = HttpServer.create(new InetSocketAddress("localhost", 0), 0);
		server.createContext("/hello", exchange -> {
			byte[] body = "{\"name\":\"Robert\"}".getBytes("UTF-8");
			exchange.getResponseHeaders().add("Content-Type", "application/json");
			exchange.sendResponseHeaders(200, body.length);
			try (OutputStream out = exchange.getResponseBody()) {
				out.write(body);
			}
		});
		server.start();
		int port = server.getAddress().getPort();
		System.out.println("PROBE server on " + port);

		for (String cn : new String[] {
				"org.apache.hc.client5.http.classic.HttpClient",
				"org.eclipse.jetty.client.HttpClient",
				"java.net.http.HttpClient" }) {
			boolean present;
			try {
				Class.forName(cn, false, RcProbe.class.getClassLoader());
				present = true;
			}
			catch (Throwable t) {
				present = false;
			}
			System.out.println("PROBE candidate " + cn + " present=" + present);
		}

		try {
			RestClient client = RestClient.builder().baseUrl("http://localhost:" + port).build();
			ResponseEntity<String> entity = client.get().uri("/hello").retrieve().toEntity(String.class);
			System.out.println("PROBE OK status=" + entity.getStatusCode() + " body=" + entity.getBody());
		}
		catch (Throwable ex) {
			System.out.println("PROBE THREW " + ex);
			ex.printStackTrace(System.out);
		}
		server.stop(0);
		System.out.println("PROBE done");
		System.exit(0);
	}
}
