import com.sun.net.httpserver.HttpServer;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.URI;
import java.net.URL;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.util.Locale;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executors;

/**
 * Differential oracle for `com.sun.net.httpserver` CONNECTION handling, diffed
 * line-for-line against HotSpot.
 *
 * CratonVM serves this API from native code (`net_phase_e.rs`, "RE.10"), and
 * until 2026-09-12 that server closed every connection after one response. A
 * keep-alive server has framing obligations a one-shot server can ignore — a
 * body boundary that is off by one byte is invisible when the connection closes
 * and fatal when the next request is read from the same stream — so every arm
 * below pins a boundary rather than a happy path:
 *
 *   pipeline  three requests in ONE write (GET, Content-Length POST, chunked
 *             POST) and a fourth after reading, on one connection
 *   head      HEAD then GET on one connection (a HEAD response has no body)
 *   204       204 then GET on one connection (a 204 has no body)
 *   big       a 256 KiB response body, then a GET on the same connection
 *   close     a `Connection: close` request: what comes back, and whether the
 *             server then closes
 *   http10    the same question for an HTTP/1.0 request
 *   ports     distinct client ports the server saw across 20 sequential
 *             requests, for both JDK client stacks (1 == reused)
 *   stop      `stop()` must close an idle kept-alive connection
 *
 * Prints facts only — status, lengths, content hashes, and whether a
 * `Connection` header was present — never a `Date` or a port number.
 */
public final class HttpServerKeepAliveMatrixProbe {

	private static final Set<Integer> PORTS = ConcurrentHashMap.newKeySet();
	private static final byte[] BIG = new byte[256 * 1024];

	public static void main(String[] args) throws Exception {
		for (int i = 0; i < BIG.length; i++) {
			BIG[i] = (byte) (i * 31 + 7);
		}
		HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 50);
		server.createContext("/echo", ex -> {
			PORTS.add(ex.getRemoteAddress().getPort());
			byte[] in;
			try (InputStream body = ex.getRequestBody()) {
				in = body.readAllBytes();
			}
			String q = ex.getRequestURI().getQuery();
			byte[] out = ("M=" + ex.getRequestMethod() + " Q=" + q + " BL=" + in.length + " H=" + hash(in))
					.getBytes(StandardCharsets.UTF_8);
			ex.sendResponseHeaders(200, out.length);
			try (OutputStream o = ex.getResponseBody()) {
				o.write(out);
			}
		});
		server.createContext("/head", ex -> {
			byte[] out = "hello".getBytes(StandardCharsets.UTF_8);
			if (ex.getRequestMethod().equals("HEAD")) {
				ex.sendResponseHeaders(200, -1);
				ex.close();
				return;
			}
			ex.sendResponseHeaders(200, out.length);
			try (OutputStream o = ex.getResponseBody()) {
				o.write(out);
			}
		});
		server.createContext("/nocontent", ex -> {
			ex.sendResponseHeaders(204, -1);
			ex.close();
		});
		server.createContext("/big", ex -> {
			ex.sendResponseHeaders(200, BIG.length);
			try (OutputStream o = ex.getResponseBody()) {
				o.write(BIG);
			}
		});
		server.setExecutor(Executors.newFixedThreadPool(4, r -> {
			Thread t = new Thread(r);
			t.setDaemon(true);
			return t;
		}));
		server.start();
		int port = server.getAddress().getPort();

		pipeline(port);
		sameConnection("head", port, "HEAD /head HTTP/1.1\r\nHost: x\r\n\r\n", true);
		sameConnection("204", port, "GET /nocontent HTTP/1.1\r\nHost: x\r\n\r\n", true);
		sameConnection("big", port, "GET /big HTTP/1.1\r\nHost: x\r\n\r\n", false);
		closeSemantics("close", port, "GET /echo?c HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
		closeSemantics("http10", port, "GET /echo?d HTTP/1.0\r\nHost: x\r\n\r\n");
		ports(port);
		stop(server, port);
		System.exit(0);
	}

	// ---- arms ---------------------------------------------------------------

	private static void pipeline(int port) throws IOException {
		try (Socket s = open(port)) {
			String three = "GET /echo?one HTTP/1.1\r\nHost: x\r\n\r\n"
					+ "POST /echo?two HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello"
					+ "POST /echo?three HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n"
					+ "3\r\nabc\r\n4\r\ndefg\r\n0\r\n\r\n";
			s.getOutputStream().write(three.getBytes(StandardCharsets.US_ASCII));
			s.getOutputStream().flush();
			for (int i = 1; i <= 3; i++) {
				System.out.println("pipeline #" + i + " " + readResponse(s.getInputStream(), false));
			}
			s.getOutputStream().write("GET /echo?four HTTP/1.1\r\nHost: x\r\n\r\n".getBytes(StandardCharsets.US_ASCII));
			System.out.println("pipeline #4 " + readResponse(s.getInputStream(), false));
		}
	}

	/** `first`, then an ordinary GET on the SAME connection. */
	private static void sameConnection(String label, int port, String first, boolean firstHasNoBody)
			throws IOException {
		try (Socket s = open(port)) {
			s.getOutputStream().write(first.getBytes(StandardCharsets.US_ASCII));
			System.out.println(label + " first  " + readResponse(s.getInputStream(), firstHasNoBody));
			s.getOutputStream().write("GET /echo?after HTTP/1.1\r\nHost: x\r\n\r\n".getBytes(StandardCharsets.US_ASCII));
			System.out.println(label + " second " + readResponse(s.getInputStream(), false));
		}
	}

	private static void closeSemantics(String label, int port, String request) throws IOException {
		try (Socket s = open(port)) {
			s.getOutputStream().write(request.getBytes(StandardCharsets.US_ASCII));
			System.out.println(label + " " + readResponse(s.getInputStream(), false));
			s.setSoTimeout(3000);
			String after;
			try {
				after = s.getInputStream().read() < 0 ? "server-closed" : "extra-bytes";
			} catch (java.net.SocketTimeoutException e) {
				after = "still-open";
			} catch (IOException e) {
				after = "server-closed";
			}
			System.out.println(label + " then " + after);
		}
	}

	private static void ports(int port) throws Exception {
		URL url = URI.create("http://127.0.0.1:" + port + "/echo?p").toURL();
		PORTS.clear();
		for (int i = 0; i < 20; i++) {
			HttpURLConnection c = (HttpURLConnection) url.openConnection();
			try (InputStream in = c.getInputStream()) {
				in.readAllBytes();
			}
		}
		System.out.println("ports HttpURLConnection distinct=" + PORTS.size() + " of 20");

		HttpClient client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
		HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/echo?q")).build();
		PORTS.clear();
		for (int i = 0; i < 20; i++) {
			client.send(req, HttpResponse.BodyHandlers.ofByteArray());
		}
		System.out.println("ports HttpClient        distinct=" + PORTS.size() + " of 20");
	}

	private static void stop(HttpServer server, int port) throws IOException {
		try (Socket s = open(port)) {
			s.getOutputStream().write("GET /echo?idle HTTP/1.1\r\nHost: x\r\n\r\n".getBytes(StandardCharsets.US_ASCII));
			System.out.println("stop before " + readResponse(s.getInputStream(), false));
			server.stop(0);
			s.setSoTimeout(5000);
			String outcome;
			try {
				outcome = s.getInputStream().read() < 0 ? "closed" : "bytes";
			} catch (java.net.SocketTimeoutException e) {
				outcome = "STILL-OPEN";
			} catch (IOException e) {
				outcome = "closed";
			}
			System.out.println("stop idle connection " + outcome);
		}
	}

	// ---- wire helpers -------------------------------------------------------

	private static Socket open(int port) throws IOException {
		Socket s = new Socket("127.0.0.1", port);
		s.setSoTimeout(10_000);
		s.setTcpNoDelay(true);
		return s;
	}

	/**
	 * One response: status, framing, a body hash, and whether a `Connection`
	 * header came back. `noBody` is the caller's knowledge that the request
	 * cannot have a body in its response (HEAD, 204), exactly as a real client
	 * must know it.
	 */
	private static String readResponse(InputStream in, boolean noBody) throws IOException {
		ByteArrayOutputStream head = new ByteArrayOutputStream();
		int matched = 0;
		while (matched < 4) {
			int b = in.read();
			if (b < 0) {
				return "EOF-in-head after " + head.size() + " bytes";
			}
			head.write(b);
			matched = (b == (matched % 2 == 0 ? '\r' : '\n')) ? matched + 1 : (b == '\r' ? 1 : 0);
		}
		String[] lines = head.toString(StandardCharsets.ISO_8859_1).split("\r\n");
		String status = lines[0].split(" ", 3)[1];
		long length = -1;
		String connection = "none";
		boolean chunked = false;
		for (int i = 1; i < lines.length; i++) {
			int colon = lines[i].indexOf(':');
			if (colon < 0) {
				continue;
			}
			String k = lines[i].substring(0, colon).trim().toLowerCase(Locale.ROOT);
			String v = lines[i].substring(colon + 1).trim();
			if (k.equals("content-length")) {
				length = Long.parseLong(v);
			} else if (k.equals("connection")) {
				connection = v.toLowerCase(Locale.ROOT);
			} else if (k.equals("transfer-encoding")) {
				chunked = v.toLowerCase(Locale.ROOT).contains("chunked");
			}
		}
		byte[] body;
		if (noBody || status.equals("204") || status.equals("304")) {
			body = new byte[0];
		} else if (chunked) {
			body = readChunked(in);
		} else if (length >= 0) {
			body = in.readNBytes((int) length);
		} else {
			body = in.readAllBytes();
		}
		String text = body.length <= 64 ? new String(body, StandardCharsets.UTF_8) : ("<" + body.length + " bytes>");
		return "status=" + status + " body=" + body.length + " hash=" + hash(body) + " conn=" + connection
				+ " [" + text + "]";
	}

	private static byte[] readChunked(InputStream in) throws IOException {
		ByteArrayOutputStream out = new ByteArrayOutputStream();
		while (true) {
			String size = line(in).split(";")[0].trim();
			int n = Integer.parseInt(size, 16);
			if (n == 0) {
				while (!line(in).isEmpty()) {
					// trailers
				}
				return out.toByteArray();
			}
			out.write(in.readNBytes(n));
			line(in);
		}
	}

	private static String line(InputStream in) throws IOException {
		StringBuilder sb = new StringBuilder();
		int b;
		while ((b = in.read()) >= 0 && b != '\n') {
			if (b != '\r') {
				sb.append((char) b);
			}
		}
		return sb.toString();
	}

	private static String hash(byte[] b) {
		long h = 1125899906842597L;
		for (byte x : b) {
			h = 31 * h + x;
		}
		return Long.toHexString(h);
	}
}
