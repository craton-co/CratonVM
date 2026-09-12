import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.Locale;
import java.util.concurrent.Executors;

/**
 * End-to-end NIO throughput, on loopback, with no dependency outside the JDK.
 *
 * Three arms, each isolating a different layer of the same stack:
 *
 *   1. `socket-echo`  — raw `SocketChannel` read/write of a direct
 *                       `ByteBuffer`. No HTTP, no codec: this is the floor
 *                       the other two build on.
 *   2. `http-small`   — `HttpServer` + `HttpClient`, 64-byte bodies. Dominated
 *                       by header parse/format, i.e. per-byte `ByteBuffer` and
 *                       `String` work.
 *   3. `http-large`   — the same, 256 KiB bodies. Dominated by bulk copies.
 *
 * Reports microseconds per request (or per echo round trip), MINIMUM over
 * rounds, so a loaded host inflates the number rather than corrupting the
 * comparison. Run the same command against HotSpot and against CratonVM; the
 * ratio between the two is what this probe is for.
 *
 * Usage: `NioHttpThroughputProbe [requests] [rounds]`
 */
public final class NioHttpThroughputProbe {

	private static int requests = 2000;
	private static int rounds = 3;

	private static final int LARGE = 256 * 1024;

	public static void main(String[] args) throws Exception {
		if (args.length > 0) {
			requests = Integer.parseInt(args[0]);
		}
		if (args.length > 1) {
			rounds = Integer.parseInt(args[1]);
		}

		double bestEcho = Double.MAX_VALUE;
		double bestSmall = Double.MAX_VALUE;
		double bestLarge = Double.MAX_VALUE;

		try (EchoServer echo = new EchoServer()) {
			HttpFixture http = new HttpFixture();
			try {
				// Untimed warm-up of all three arms.
				echoArm(echo, Math.min(requests, 200));
				httpArm(http, 64, Math.min(requests, 200));
				httpArm(http, LARGE, Math.min(requests / 10, 50));

				for (int r = 0; r < rounds; r++) {
					bestEcho = Math.min(bestEcho, echoArm(echo, requests));
					bestSmall = Math.min(bestSmall, httpArm(http, 64, requests));
					bestLarge = Math.min(bestLarge, httpArm(http, LARGE, Math.max(1, requests / 10)));
				}
			} finally {
				http.close();
			}
		}

		System.out.println("arm,micros_per_request");
		System.out.printf(Locale.ROOT, "socket-echo (1 KiB direct),%10.2f%n", bestEcho);
		System.out.printf(Locale.ROOT, "http-small (64 B body)    ,%10.2f%n", bestSmall);
		System.out.printf(Locale.ROOT, "http-large (256 KiB body) ,%10.2f%n", bestLarge);
		System.out.flush();
		// The HttpServer executor and the HttpClient selector are non-daemon;
		// the measurement is printed, so leave rather than wait for them.
		System.exit(0);
	}

	// ---- arm 1: raw SocketChannel echo -------------------------------------

	private static double echoArm(EchoServer server, int n) throws IOException {
		ByteBuffer out = ByteBuffer.allocateDirect(1024);
		ByteBuffer in = ByteBuffer.allocateDirect(1024);
		for (int i = 0; i < 1024; i++) {
			out.put(i, (byte) i);
		}
		try (SocketChannel ch = SocketChannel.open(server.address())) {
			ch.configureBlocking(true);
			ch.setOption(java.net.StandardSocketOptions.TCP_NODELAY, Boolean.TRUE);
			long start = System.nanoTime();
			for (int i = 0; i < n; i++) {
				out.position(0).limit(1024);
				while (out.hasRemaining()) {
					ch.write(out);
				}
				in.clear();
				while (in.position() < 1024) {
					if (ch.read(in) < 0) {
						throw new IOException("server closed early");
					}
				}
			}
			return (System.nanoTime() - start) / 1_000.0 / n;
		}
	}

	/** Reads 1 KiB and writes it straight back, forever, on one connection. */
	private static final class EchoServer implements AutoCloseable {
		private final ServerSocketChannel listener;
		private final Thread thread;
		private volatile boolean running = true;

		EchoServer() throws IOException {
			listener = ServerSocketChannel.open();
			listener.bind(new InetSocketAddress("127.0.0.1", 0));
			thread = new Thread(this::serve, "echo-server");
			thread.setDaemon(true);
			thread.start();
		}

		InetSocketAddress address() throws IOException {
			return new InetSocketAddress("127.0.0.1",
					((InetSocketAddress) listener.getLocalAddress()).getPort());
		}

		private void serve() {
			ByteBuffer buf = ByteBuffer.allocateDirect(1024);
			while (running) {
				try (SocketChannel ch = listener.accept()) {
					ch.configureBlocking(true);
					ch.setOption(java.net.StandardSocketOptions.TCP_NODELAY, Boolean.TRUE);
					while (running) {
						buf.clear();
						int read = ch.read(buf);
						if (read < 0) {
							break;
						}
						buf.flip();
						while (buf.hasRemaining()) {
							ch.write(buf);
						}
					}
				} catch (IOException e) {
					if (running) {
						return;
					}
				}
			}
		}

		@Override public void close() throws IOException {
			running = false;
			listener.close();
		}
	}

	// ---- arms 2 and 3: HttpServer + HttpClient -----------------------------

	private static final class HttpFixture implements AutoCloseable {
		final HttpServer server;
		final HttpClient client;
		final int port;

		HttpFixture() throws IOException {
			server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 128);
			server.createContext("/echo", HttpFixture::handle);
			server.setExecutor(Executors.newFixedThreadPool(2));
			server.start();
			port = server.getAddress().getPort();
			client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
		}

		private static void handle(HttpExchange exchange) throws IOException {
			byte[] body;
			try (InputStream in = exchange.getRequestBody()) {
				body = in.readAllBytes();
			}
			exchange.getResponseHeaders().add("X-Probe", "cratonvm");
			exchange.sendResponseHeaders(200, body.length);
			try (OutputStream out = exchange.getResponseBody()) {
				out.write(body);
			}
		}

		@Override public void close() {
			server.stop(0);
		}
	}

	private static double httpArm(HttpFixture fixture, int bodySize, int n) throws Exception {
		byte[] body = new byte[bodySize];
		for (int i = 0; i < bodySize; i++) {
			body[i] = (byte) i;
		}
		URI uri = URI.create("http://127.0.0.1:" + fixture.port + "/echo");
		long start = System.nanoTime();
		for (int i = 0; i < n; i++) {
			HttpRequest request = HttpRequest.newBuilder(uri)
					.POST(HttpRequest.BodyPublishers.ofByteArray(body))
					.header("Content-Type", "application/octet-stream")
					.build();
			HttpResponse<byte[]> response =
					fixture.client.send(request, HttpResponse.BodyHandlers.ofByteArray());
			if (response.statusCode() != 200 || response.body().length != bodySize) {
				throw new IllegalStateException(
						"bad response: " + response.statusCode() + " len " + response.body().length);
			}
		}
		return (System.nanoTime() - start) / 1_000.0 / n;
	}

}
