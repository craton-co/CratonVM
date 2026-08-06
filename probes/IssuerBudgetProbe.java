import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;

import org.springframework.security.oauth2.jwt.JwtDecoders;

/**
 * What read-timeout budget does Spring Security's issuer-location discovery
 * actually apply, and is it the same on HotSpot and CratonVM?
 *
 * <p>{@code oauth2-issuer-uri-mock-server-read-timeout-flake-20260803.md} measured
 * `configured=500ms` from inside CratonVM's `http_url_connection.rs` and could not
 * identify which client set it -- the VM-generated Java stack blamed
 * `SimpleClientHttpRequestFactory.prepareConnection`, which is never called. The
 * page left one fork open: <em>does HotSpot budget 500 ms here too?</em> If it
 * does, the flake is a pure CratonVM latency defect. If HotSpot budgets more,
 * there is also a configuration path CratonVM gets wrong.
 *
 * <p>This answers that WITHOUT instrumenting either VM, so the same measurement
 * runs on both. It stands up a plain-`ServerSocket` OIDC endpoint that delays
 * every response by a fixed amount, then calls the same entry point the failing
 * test reaches (`JwtDecoders.fromIssuerLocation`, via `SupplierJwtDecoder`'s
 * delegate) and reports, per delay, whether the call survived and which request
 * died. The smallest delay that fails IS the effective budget, per request.
 *
 * <p>Two requests happen, and they may carry DIFFERENT budgets -- discovery, then
 * the JWK Set fetch that `getJWSAlgorithms` performs to pick the algorithm. The
 * probe reports which one failed, because "the budget" is not necessarily one
 * number. Run it against both VMs and diff the two tables.
 */
public final class IssuerBudgetProbe {

	private static final String JWK_SET = "{\"keys\":[{\"kty\":\"RSA\",\"e\":\"AQAB\",\"use\":\"sig\","
			+ "\"kid\":\"one\",\"n\":\"oXJ8OyOv_eRnce4akdanR4KYRfnC2zLV4uYNQpcFn6oHL0dj7D6kxQmsXoYgJV8ZVDn71KGm"
			+ "uLvolxsDncc2UrhyMBY6DVQVgMSVYaPCTgW76iYEKGgzTEw5IBRQL9w3SRJWd3VJTZZQjkXef48Ocz06PGF3lhbz4t5UEZtd"
			+ "F4rIe7u-977QwHuh7yRPBQ3sII-cVoOUMgaXB9SHcGF2iZCtPzL_IffDUcfhLQteGebhW8A6eUHgpD5A1PQ-JCw_G7UOzZAj"
			+ "jDjtNM2eqm8j-Ms_gqnm4MiCZ4E-9pDN77CAAPVN7kuX6ejs9KBXpk01z48i9fORYk9u7rAkh1HuQw\"}]}";

	/** Which request the server last served, so a failure can be attributed. */
	private static volatile String lastPath = "(none)";

	/** `-Dprobe.srvTiming=true` prints the server side's own phase timings. */
	private static final boolean SERVER_TIMING = Boolean.getBoolean("probe.srvTiming");

	public static void main(String[] args) throws Exception {
		// `headroom <maxDelay> <repeats>` -- the mode that matters for the
		// latency question. The budget is 500 ms per request, so a server delay
		// of D leaves 500-D ms of headroom for everything the VM adds. Sweeping D
		// upward and finding where a VM starts failing converts an
		// unreproducible flake into a continuous quantity: the difference between
		// two VMs' last-passing D IS the latency one adds over the other, and it
		// needs no suite, no load and no instrumentation.
		if (args.length > 0 && args[0].equals("headroom")) {
			int max = args.length > 1 ? Integer.parseInt(args[1]) : 480;
			int repeats = args.length > 2 ? Integer.parseInt(args[2]) : 3;
			headroomSweep(max, repeats);
			return;
		}

		// `breakdown <iters>` -- split the per-exchange cost into "HTTP layer" and
		// "everything Spring/Nimbus does on top". The headroom sweep says
		// CratonVM spends ~100 ms per exchange that HotSpot does not, but the
		// exchange is two HTTP round trips PLUS Jackson parsing, Nimbus JWK
		// construction and RestTemplate machinery. Those are very different
		// defects, and the sweep cannot tell them apart.
		if (args.length > 0 && args[0].equals("breakdown")) {
			breakdown(args.length > 1 ? Integer.parseInt(args[1]) : 12);
			return;
		}

		// `server <port>` / `client <host> <port> <iters>` -- the SAME exchange
		// split across two processes, so the two VMs can be mixed. The in-process
		// breakdown cannot tell "the client's read wakes late" from "the server
		// writes late": both sides are the same VM. Running HotSpot-server +
		// CratonVM-client and the reverse attributes the cost to one side.
		if (args.length > 0 && args[0].equals("server")) {
			int port = Integer.parseInt(args[1]);
			try (ServerSocket server = new ServerSocket(port, 50, InetAddress.getByName("127.0.0.1"))) {
				System.out.println("SERVER-READY " + server.getLocalPort());
				System.out.flush();
				serve(server, "http://127.0.0.1:" + server.getLocalPort() + "/test", 0);
			}
			return;
		}
		if (args.length > 0 && args[0].equals("client")) {
			String host = args[1];
			int port = Integer.parseInt(args[2]);
			int iters = args.length > 3 ? Integer.parseInt(args[3]) : 8;
			System.out.println("phase\titer\tms\tdetail");
			for (int i = 0; i < iters; i++) {
				long t0 = System.nanoTime();
				long[] p = rawSocketGetPhases(host, port, "/test/.well-known/openid-configuration");
				System.out.println("split-client\t" + i + "\t" + (System.nanoTime() - t0) / 1_000_000L
						+ "\tconnect=" + p[0] + " write=" + p[1] + " firstByte=" + p[2] + " drain=" + p[3]
						+ " close=" + p[4]);
			}
			System.out.println("PROBE-DONE");
			return;
		}

		// `readcost` -- per-call cost of InputStream.read() on a connected socket
		// whose data has ALREADY arrived. No wakeup, no blocking: the bytes are
		// sitting in the receive buffer before the clock starts, so what this
		// measures is purely the cost of the call itself.
		if (args.length > 0 && args[0].equals("readcost")) {
			readCost();
			return;
		}

		// `soak <iters>` -- the same exchange, zero server delay, repeated, with
		// every slow one reported. Run several of these concurrently: if the
		// 500 ms budget can be blown here, the repro no longer needs the Spring
		// class or MockWebServer, and the stall becomes freely instrumentable.
		if (args.length > 0 && args[0].equals("soak")) {
			soak(args.length > 1 ? Integer.parseInt(args[1]) : 60);
			return;
		}

		int[] delays = { 0, 200, 400, 600, 900, 1500, 3000 };
		if (args.length > 0) {
			String[] parts = args[0].split(",");
			delays = new int[parts.length];
			for (int i = 0; i < parts.length; i++) {
				delays[i] = Integer.parseInt(parts[i].trim());
			}
		}

		System.out.println("delayMs\toutcome\telapsedMs\tlastServedPath\tdetail");
		for (int delay : delays) {
			runOne(delay);
		}
		System.out.println("PROBE-DONE");
	}

	/**
	 * Report, per delay, how many of {@code repeats} attempts survived. The
	 * highest delay with a full pass count is that VM's headroom limit.
	 */
	private static void headroomSweep(int maxDelay, int repeats) throws Exception {
		System.out.println("delayMs\theadroomMs\tpassed\tof\tmaxElapsedMs");
		int lastAllPass = -1;
		for (int delay = 0; delay <= maxDelay; delay += 40) {
			int passed = 0;
			long worst = 0;
			for (int i = 0; i < repeats; i++) {
				long[] out = new long[1];
				if (attempt(delay, out)) {
					passed++;
				}
				worst = Math.max(worst, out[0]);
			}
			System.out.println(delay + "\t" + (500 - delay) + "\t" + passed + "\t" + repeats + "\t" + worst);
			if (passed == repeats) {
				lastAllPass = delay;
			}
		}
		System.out.println("HEADROOM-LIMIT " + lastAllPass);
		System.out.println("PROBE-DONE");
	}

	/**
	 * Per-iteration cost of (a) a bare `HttpURLConnection` GET against the same
	 * local server and (b) the full `fromIssuerLocation` exchange. Zero server
	 * delay throughout, so every millisecond reported is the VM's own.
	 */
	private static void breakdown(int iters) throws Exception {
		try (ServerSocket server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
			int port = server.getLocalPort();
			String issuer = "http://127.0.0.1:" + port + "/test";
			Thread acceptor = new Thread(() -> serve(server, issuer, 0), "oidc-server");
			acceptor.setDaemon(true);
			acceptor.start();

			System.out.println("phase\titer\tms\tdetail");
			// How long does it cost merely to START a thread? The server below
			// spawns one per connection, as MockWebServer does, so a slow
			// Thread.start() would masquerade as socket latency.
			for (int i = 0; i < 5; i++) {
				long t0 = System.nanoTime();
				Thread t = new Thread(() -> {
				}, "spawn-probe");
				t.start();
				t.join();
				System.out.println("thread-start-join\t" + i + "\t" + (System.nanoTime() - t0) / 1_000_000L + "\t-");
			}
			// Raw TCP, per phase. If this is fast and `bare-http` is slow, the
			// cost is in `http_url_connection.rs`; if both are slow it is below
			// them -- and those are different fixes.
			for (int i = 0; i < iters; i++) {
				long t0 = System.nanoTime();
				long[] p = rawSocketGetPhases("127.0.0.1", port, "/test/.well-known/openid-configuration");
				System.out.println("raw-socket\t" + i + "\t" + (System.nanoTime() - t0) / 1_000_000L
						+ "\tconnect=" + p[0] + " write=" + p[1] + " firstByte=" + p[2] + " drain=" + p[3]
						+ " close=" + p[4]);
			}
			for (int i = 0; i < iters; i++) {
				long t0 = System.nanoTime();
				bareGet(issuer + "/.well-known/openid-configuration");
				System.out.println("bare-http\t" + i + "\t" + (System.nanoTime() - t0) / 1_000_000L);
			}
			for (int i = 0; i < iters; i++) {
				long t0 = System.nanoTime();
				String outcome = "ok";
				try {
					JwtDecoders.fromIssuerLocation(issuer);
				}
				catch (Throwable ex) {
					outcome = "FAIL";
				}
				System.out.println("full-exchange\t" + i + "\t" + (System.nanoTime() - t0) / 1_000_000L + "\t"
						+ outcome);
			}
		}
		System.out.println("PROBE-DONE");
	}

	/** Repeat the exchange at zero delay, reporting every slow one and any failure. */
	private static void soak(int iters) throws Exception {
		long worst = 0;
		int failures = 0;
		int slow = 0;
		for (int i = 0; i < iters; i++) {
			long[] elapsed = new long[1];
			boolean ok = attempt(0, elapsed);
			if (!ok) {
				failures++;
			}
			if (elapsed[0] > worst) {
				worst = elapsed[0];
			}
			// 400 ms of a 500 ms budget on a zero-delay localhost exchange is
			// already a near-miss worth seeing, not just the outright failures.
			if (elapsed[0] > 400 || !ok) {
				slow++;
				System.out.println("[soak] iter=" + i + " ms=" + elapsed[0] + " ok=" + ok);
				System.out.flush();
			}
		}
		System.out.println("SOAK-RESULT iters=" + iters + " failures=" + failures + " slow=" + slow + " worstMs="
				+ worst);
		System.out.println("PROBE-DONE");
	}

	/**
	 * Per-call cost of a socket read, with the data already in the receive
	 * buffer. Two shapes over the identical payload: N single-byte reads (what
	 * any header parser does) and bulk reads. If single-byte is slow while bulk
	 * is fast, the defect is fixed overhead per `read()` call, not wakeup
	 * latency -- a distinction the exchange-level numbers cannot make.
	 */
	private static void readCost() throws Exception {
		final int payload = 8192;
		try (ServerSocket ss = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
			int port = ss.getLocalPort();
			final int rounds = 6;
			Thread sender = new Thread(() -> {
				try (Socket s = new Socket("127.0.0.1", port)) {
					s.setTcpNoDelay(true);
					byte[] buf = new byte[payload];
					// EXACTLY one payload per round, with generous slack, so the
					// receiver's timed loop never waits on the network. A sender
					// that under-delivers turns this into a measurement of
					// blocking, which is the opposite of what it is for.
					for (int r = 0; r < rounds; r++) {
						s.getOutputStream().write(buf);
						s.getOutputStream().flush();
						Thread.sleep(400);
					}
					Thread.sleep(1000);
				}
				catch (Exception ignored) {
					// receiver closed first; not the case under test
				}
			}, "readcost-sender");
			sender.setDaemon(true);
			sender.start();

			try (Socket in = ss.accept()) {
				in.setTcpNoDelay(true);
				in.setSoTimeout(5000);
				InputStream is = in.getInputStream();
				System.out.println("shape\tbytes\ttotalMs\tnsPerCall");

				for (int round = 0; round < rounds; round++) {
					// One payload per round, consumed by ONE shape, so the bytes
					// are already in the receive buffer when the clock starts.
					Thread.sleep(250);
					if (round % 2 == 0) {
						long t0 = System.nanoTime();
						for (int i = 0; i < payload; i++) {
							if (is.read() < 0) {
								break;
							}
						}
						long d = System.nanoTime() - t0;
						System.out.println("single-byte\t" + payload + "\t" + d / 1_000_000L + "\t" + d / payload);
					}
					else {
						byte[] buf = new byte[payload];
						int got = 0;
						int calls = 0;
						long t0 = System.nanoTime();
						while (got < payload) {
							int n = is.read(buf, got, payload - got);
							calls++;
							if (n < 0) {
								break;
							}
							got += n;
						}
						long d = System.nanoTime() - t0;
						System.out.println("bulk\t" + got + "\t" + d / 1_000_000L + "\t"
								+ (d / Math.max(calls, 1)) + "\t(" + calls + " calls)");
					}
				}
			}
		}
		System.out.println("PROBE-DONE");
	}

	/**
	 * The same GET over a bare `Socket`, timed per phase, so "the socket layer
	 * is slow" becomes a statement about WHICH call is slow. Returns
	 * {@code [connectMs, writeMs, firstByteMs, drainMs, closeMs]}.
	 */
	private static long[] rawSocketGetPhases(String host, int port, String path) throws IOException {
		long[] out = new long[5];
		long t = System.nanoTime();
		Socket s = new Socket(host, port);
		out[0] = ms(t);
		try {
			s.setTcpNoDelay(true);
			s.setSoTimeout(500);
			t = System.nanoTime();
			OutputStream os = s.getOutputStream();
			os.write(("GET " + path + " HTTP/1.1\r\nHost: " + host + ":" + port + "\r\nConnection: close\r\n\r\n")
				.getBytes(StandardCharsets.US_ASCII));
			os.flush();
			out[1] = ms(t);

			InputStream in = s.getInputStream();
			byte[] buf = new byte[4096];
			t = System.nanoTime();
			int n = in.read(buf);
			out[2] = ms(t);
			t = System.nanoTime();
			while (n >= 0) {
				n = in.read(buf);
			}
			out[3] = ms(t);
		}
		finally {
			t = System.nanoTime();
			s.close();
			out[4] = ms(t);
		}
		return out;
	}

	private static long ms(long sinceNanos) {
		return (System.nanoTime() - sinceNanos) / 1_000_000L;
	}

	private static void rawSocketGet(String host, int port, String path) throws IOException {
		rawSocketGetPhases(host, port, path);
	}

	/** One plain GET, response fully drained -- no Spring, no Jackson, no Nimbus. */
	private static void bareGet(String url) throws IOException {
		java.net.HttpURLConnection c = (java.net.HttpURLConnection) java.net.URI.create(url).toURL()
			.openConnection();
		c.setConnectTimeout(500);
		c.setReadTimeout(500);
		try (InputStream in = c.getInputStream()) {
			byte[] buf = new byte[4096];
			while (in.read(buf) >= 0) {
				// drain
			}
		}
		finally {
			c.disconnect();
		}
	}

	/** One discovery+JWKS exchange at {@code delayMs}; returns whether it survived. */
	private static boolean attempt(int delayMs, long[] elapsedOut) throws Exception {
		try (ServerSocket server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
			int port = server.getLocalPort();
			String issuer = "http://127.0.0.1:" + port + "/test";
			Thread acceptor = new Thread(() -> serve(server, issuer, delayMs), "oidc-server");
			acceptor.setDaemon(true);
			acceptor.start();
			long t0 = System.nanoTime();
			try {
				JwtDecoders.fromIssuerLocation(issuer);
				return true;
			}
			catch (Throwable ex) {
				return false;
			}
			finally {
				elapsedOut[0] = (System.nanoTime() - t0) / 1_000_000L;
			}
		}
	}

	private static void runOne(int delayMs) throws Exception {
		try (ServerSocket server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))) {
			int port = server.getLocalPort();
			String issuer = "http://127.0.0.1:" + port + "/test";
			lastPath = "(none)";

			Thread acceptor = new Thread(() -> serve(server, issuer, delayMs), "oidc-server");
			acceptor.setDaemon(true);
			acceptor.start();

			long t0 = System.nanoTime();
			String outcome;
			String detail;
			try {
				JwtDecoders.fromIssuerLocation(issuer);
				outcome = "OK";
				detail = "-";
			}
			catch (Throwable ex) {
				outcome = "FAIL";
				detail = rootCause(ex);
				// `-Dprobe.stack=true` dumps the whole chain. Worth doing on
				// HotSpot, whose stacks can be trusted, to name the client that
				// owns the budget -- the page this probe serves was misled for
				// hours by a VM-generated stack.
				if (Boolean.getBoolean("probe.stack")) {
					System.out.println("---- full stack for delay=" + delayMs + " ----");
					ex.printStackTrace(System.out);
					System.out.println("---- end stack ----");
				}
			}
			long ms = (System.nanoTime() - t0) / 1_000_000L;
			System.out.println(delayMs + "\t" + outcome + "\t" + ms + "\t" + lastPath + "\t" + detail);
		}
	}

	private static String rootCause(Throwable ex) {
		Throwable t = ex;
		while (t.getCause() != null && t.getCause() != t) {
			t = t.getCause();
		}
		String msg = t.getMessage();
		return t.getClass().getName() + (msg != null ? ": " + msg.replace('\n', ' ') : "");
	}

	private static void serve(ServerSocket server, String issuer, int delayMs) {
		while (!server.isClosed()) {
			try {
				Socket s = server.accept();
				Thread worker = new Thread(() -> handle(s, issuer, delayMs), "oidc-conn");
				worker.setDaemon(true);
				worker.start();
			}
			catch (IOException ex) {
				return; // server closed between iterations -- expected
			}
		}
	}

	private static void handle(Socket s, String issuer, int delayMs) {
		long tAccepted = System.nanoTime();
		try (Socket sock = s) {
			sock.setTcpNoDelay(true);
			// `-Dprobe.bufferedServer=true` wraps the request read the way a real
			// server does (MockWebServer reads through Okio segments). Without it
			// `readLine` issues one `read()` per BYTE, and on a VM with high
			// per-call read overhead that alone dominates the exchange -- which
			// would make this probe measure the probe, not the product.
			InputStream in = Boolean.getBoolean("probe.bufferedServer")
					? new java.io.BufferedInputStream(sock.getInputStream(), 8192) : sock.getInputStream();
			long tBeforeRead = System.nanoTime();
			String requestLine = readLine(in);
			long firstReadMs = ms(tBeforeRead);
			if (requestLine == null) {
				return;
			}
			// Drain headers so the client's write completes before we stall.
			String line;
			while ((line = readLine(in)) != null && !line.isEmpty()) {
				// header, ignored
			}
			if (SERVER_TIMING) {
				System.out.println("[srv] setup=" + (tBeforeRead - tAccepted) / 1_000_000L + "ms firstRead="
						+ firstReadMs + "ms headers=" + ms(tBeforeRead) + "ms");
				System.out.flush();
			}
			String path = requestLine.split(" ").length > 1 ? requestLine.split(" ")[1] : requestLine;
			lastPath = path;

			// The stall is AFTER the request is fully read, so it is a pure
			// response-latency delay -- exactly the shape a slow VM produces.
			if (delayMs > 0) {
				Thread.sleep(delayMs);
			}

			String body = path.contains("jwks") ? JWK_SET : discoveryDocument(issuer);
			byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
			long tWrite = System.nanoTime();
			OutputStream out = sock.getOutputStream();
			out.write(("HTTP/1.1 200 OK\r\n" + "Content-Type: application/json\r\n" + "Content-Length: " + bytes.length
					+ "\r\n" + "Connection: close\r\n" + "\r\n").getBytes(StandardCharsets.US_ASCII));
			out.write(bytes);
			out.flush();
			if (SERVER_TIMING) {
				System.out.println("[srv] write=" + ms(tWrite) + "ms totalSinceAccept="
						+ (System.nanoTime() - tAccepted) / 1_000_000L + "ms");
				System.out.flush();
			}
		}
		catch (Exception ex) {
			// A client that already timed out closes on us; that is the case
			// under test, not an error in the probe.
		}
	}

	private static String readLine(InputStream in) throws IOException {
		StringBuilder sb = new StringBuilder();
		int c;
		while ((c = in.read()) != -1) {
			if (c == '\n') {
				int len = sb.length();
				if (len > 0 && sb.charAt(len - 1) == '\r') {
					sb.setLength(len - 1);
				}
				return sb.toString();
			}
			sb.append((char) c);
		}
		return sb.length() > 0 ? sb.toString() : null;
	}

	/** Same shape as the failing test's `getResponse(issuer)`. */
	private static String discoveryDocument(String issuer) {
		return "{" + "\"authorization_endpoint\":\"https://example.com/o/oauth2/v2/auth\","
				+ "\"claims_supported\":[]," + "\"code_challenge_methods_supported\":[],"
				+ "\"id_token_signing_alg_values_supported\":[]," + "\"issuer\":\"" + issuer + "\","
				+ "\"jwks_uri\":\"" + issuer + "/.well-known/jwks.json\"," + "\"response_types_supported\":[],"
				+ "\"revocation_endpoint\":\"https://example.com/o/oauth2/revoke\"," + "\"scopes_supported\":[\"openid\"],"
				+ "\"subject_types_supported\":[\"public\"]," + "\"grant_types_supported\":[\"authorization_code\"],"
				+ "\"token_endpoint\":\"https://example.com/oauth2/v4/token\","
				+ "\"token_endpoint_auth_methods_supported\":[\"client_secret_basic\"],"
				+ "\"userinfo_endpoint\":\"https://example.com/oauth2/v3/userinfo\"" + "}";
	}

	private IssuerBudgetProbe() {
	}
}
