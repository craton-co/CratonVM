import com.sun.net.httpserver.HttpServer;
import java.io.OutputStream;
import java.net.*;
import java.net.http.*;
import java.util.*;
import java.util.concurrent.Executors;

/** Per-request latency distribution, sequential, after warm-up, for the two
 *  JDK client stacks against com.sun.net.httpserver. Also counts distinct
 *  client ports the server saw (1 == keep-alive honoured). */
public final class HttpLat {
	public static void main(String[] a) throws Exception {
		int warm = 300, n = 1500;
		HttpServer hs = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 128);
		Set<Integer> ports = java.util.concurrent.ConcurrentHashMap.newKeySet();
		byte[] body = new byte[64];
		hs.createContext("/", ex -> {
			ports.add(ex.getRemoteAddress().getPort());
			ex.getRequestBody().readAllBytes();
			ex.sendResponseHeaders(200, body.length);
			try (OutputStream o = ex.getResponseBody()) { o.write(body); }
		});
		hs.setExecutor(Executors.newFixedThreadPool(2));
		hs.start();
		URI u = URI.create("http://127.0.0.1:" + hs.getAddress().getPort() + "/");

		HttpClient c = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
		HttpRequest rq = HttpRequest.newBuilder(u).build();
		for (int i = 0; i < warm; i++) c.send(rq, HttpResponse.BodyHandlers.ofByteArray());
		ports.clear();
		long[] x = new long[n];
		for (int i = 0; i < n; i++) { long t = System.nanoTime(); c.send(rq, HttpResponse.BodyHandlers.ofByteArray()); x[i] = System.nanoTime() - t; }
		p("HttpClient", x, ports.size());

		URL url = u.toURL();
		for (int i = 0; i < warm; i++) huc(url);
		ports.clear();
		for (int i = 0; i < n; i++) { long t = System.nanoTime(); huc(url); x[i] = System.nanoTime() - t; }
		p("HttpURLConnection", x, ports.size());
		System.exit(0);
	}
	static void huc(URL url) throws Exception {
		HttpURLConnection h = (HttpURLConnection) url.openConnection();
		try (var in = h.getInputStream()) { in.readAllBytes(); }
	}
	static void p(String k, long[] x, int ports) {
		long[] s = x.clone(); Arrays.sort(s);
		System.out.printf(Locale.ROOT, "%-18s p50=%7.0f p90=%7.0f p99=%7.0f us   server-saw-ports=%d of %d requests%n",
				k, s[s.length / 2] / 1e3, s[s.length * 9 / 10] / 1e3, s[s.length * 99 / 100] / 1e3, ports, x.length);
	}
}
