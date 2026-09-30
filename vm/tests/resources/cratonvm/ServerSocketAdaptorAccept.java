package cratonvm;

import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;
import java.nio.channels.ServerSocketChannel;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The {@code java.net.ServerSocket} view of a {@code ServerSocketChannel} —
 * what {@code ServerSocketChannel.socket()} returns — must accept connections
 * and must honour {@code setSoTimeout}.
 *
 * This is Tomcat's {@code NioEndpoint.initServerSocket} shape. It exercises a
 * different implementation in each socket mode, and it was broken in the
 * synthetic one: the adapter's methods are split across two crates, and
 * {@code accept} reached the surface that only understands plain
 * ServerSockets, which reported {@code IOException: ServerSocket not bound}
 * because the listener lives in the channel registry.
 *
 * Every assertion here passes on HotSpot — that is the point.
 */
public final class ServerSocketAdaptorAccept {

    private static final int ACCEPT_TIMEOUT_MILLIS = 700;

    private ServerSocketAdaptorAccept() {
    }

    public static void main(String[] args) throws Exception {
        acceptTimesOut();
        acceptServesAConnection();
        acceptWithNoTimeoutServesAConnection();
        System.out.println("SERVER_SOCKET_ADAPTOR_ACCEPT_OK");
    }

    /** SO_TIMEOUT must bound the adapter's accept(), not be ignored. */
    private static void acceptTimesOut() throws Exception {
        try (ServerSocketChannel channel = ServerSocketChannel.open()) {
            channel.bind(new InetSocketAddress("127.0.0.1", 0));
            ServerSocket adapter = channel.socket();
            adapter.setSoTimeout(ACCEPT_TIMEOUT_MILLIS);
            check(adapter.getSoTimeout() == ACCEPT_TIMEOUT_MILLIS,
                    "getSoTimeout read back " + adapter.getSoTimeout());
            check(adapter.getLocalPort() > 0,
                    "adapter getLocalPort() is " + adapter.getLocalPort());

            final String[] outcome = {"<still blocked>"};
            Thread t = new Thread(() -> {
                long start = System.nanoTime();
                try {
                    adapter.accept();
                    outcome[0] = "accepted a connection nobody made";
                } catch (SocketTimeoutException expected) {
                    long ms = (System.nanoTime() - start) / 1_000_000L;
                    outcome[0] = ms >= (ACCEPT_TIMEOUT_MILLIS / 2) ? null
                            : "timed out after only " + ms + " ms";
                } catch (Throwable other) {
                    outcome[0] = "threw " + other.getClass().getName()
                            + " instead of SocketTimeoutException";
                }
            });
            t.setDaemon(true);
            t.start();
            t.join(ACCEPT_TIMEOUT_MILLIS * 10L);
            check(!t.isAlive(), "adapter accept() ignored SO_TIMEOUT and is still blocked");
            check(outcome[0] == null, "timed accept: " + outcome[0]);
        }
    }

    /** …and a real connection must still come through, timeout set. */
    private static void acceptServesAConnection() throws Exception {
        exchange(ACCEPT_TIMEOUT_MILLIS * 10);
    }

    /** …and with no timeout at all (the blocking path). */
    private static void acceptWithNoTimeoutServesAConnection() throws Exception {
        exchange(0);
    }

    private static void exchange(int soTimeoutMillis) throws Exception {
        try (ServerSocketChannel channel = ServerSocketChannel.open()) {
            channel.bind(new InetSocketAddress("127.0.0.1", 0));
            ServerSocket adapter = channel.socket();
            if (soTimeoutMillis > 0) {
                adapter.setSoTimeout(soTimeoutMillis);
            }
            int port = adapter.getLocalPort();

            AtomicReference<String> served = new AtomicReference<>("<none>");
            Thread server = new Thread(() -> {
                try (Socket accepted = adapter.accept()) {
                    InputStream in = accepted.getInputStream();
                    byte[] buf = new byte[4];
                    int n = 0;
                    while (n < 4) {
                        int r = in.read(buf, n, 4 - n);
                        if (r < 0) {
                            break;
                        }
                        n += r;
                    }
                    served.set(new String(buf, 0, n, "UTF-8"));
                    OutputStream out = accepted.getOutputStream();
                    out.write("PONG".getBytes("UTF-8"));
                    out.flush();
                } catch (Throwable t) {
                    served.set("ERR:" + t.getClass().getName());
                }
            });
            server.setDaemon(true);
            server.start();

            String reply;
            try (Socket client = new Socket("127.0.0.1", port)) {
                client.getOutputStream().write("PING".getBytes("UTF-8"));
                client.getOutputStream().flush();
                byte[] buf = new byte[4];
                int n = 0;
                InputStream in = client.getInputStream();
                while (n < 4) {
                    int r = in.read(buf, n, 4 - n);
                    if (r < 0) {
                        break;
                    }
                    n += r;
                }
                reply = new String(buf, 0, n, "UTF-8");
            }
            server.join(15000);
            check("PING".equals(served.get()),
                    "soTimeout=" + soTimeoutMillis + ": server received " + served.get());
            check("PONG".equals(reply),
                    "soTimeout=" + soTimeoutMillis + ": client received " + reply);
        }
    }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
