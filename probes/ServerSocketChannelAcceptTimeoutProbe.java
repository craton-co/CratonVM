import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.SocketTimeoutException;
import java.nio.channels.ServerSocketChannel;

/**
 * The OTHER accept-with-timeout path: the `java.net.ServerSocket` adapter
 * `ServerSocketChannel.socket()` hands back. Under real sockets that is the JDK's
 * own `sun.nio.ch.ServerSocketAdaptor`, whose timed `accept()` runs
 * `ServerSocketChannelImpl.blockingAccept(nanos)` — real bytecode, driven by the
 * same `Net.accept` / `Net.poll` natives the plain `ServerSocket` path uses.
 *
 * This is Tomcat's `NioEndpoint.initServerSocket` → `setProperties` →
 * `setSoTimeout` shape, so it is worth asserting separately from the plain
 * socket: same natives underneath, different Java on top.
 *
 * Values are normalised so a correct VM prints exactly what HotSpot prints.
 */
public class ServerSocketChannelAcceptTimeoutProbe {

    static void kv(String k, Object v) {
        System.out.println(k + "=" + v);
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        adapterAcceptTimesOut();
        nonBlockingChannelAcceptReturnsNull();
        System.out.println("PROBE-DONE");
        System.out.flush();
        System.exit(0);
    }

    static void adapterAcceptTimesOut() throws Exception {
        try (ServerSocketChannel channel = ServerSocketChannel.open()) {
            channel.bind(new InetSocketAddress("127.0.0.1", 0));
            ServerSocket adapter = channel.socket();
            adapter.setSoTimeout(700);
            kv("adapter.soTimeout", adapter.getSoTimeout());
            kv("adapter.localPortPositive", adapter.getLocalPort() > 0);

            final String[] outcome = {"<still blocked>"};
            Thread t = new Thread(() -> {
                long start = System.nanoTime();
                try {
                    adapter.accept();
                    outcome[0] = "accepted-a-connection-nobody-made";
                } catch (SocketTimeoutException expected) {
                    long ms = (System.nanoTime() - start) / 1_000_000L;
                    outcome[0] = ms >= 350 ? "SocketTimeoutException" : "timed-out-too-early:" + ms;
                } catch (Throwable other) {
                    outcome[0] = other.getClass().getName();
                    System.out.println("adapter.acceptErrorDetail=" + other);
                }
            });
            t.setDaemon(true);
            t.start();
            t.join(15000);
            kv("adapter.acceptOutcome", outcome[0]);
            kv("adapter.HUNG", t.isAlive());
        }
    }

    /** A non-blocking channel's accept() must return null immediately, never park. */
    static void nonBlockingChannelAcceptReturnsNull() throws Exception {
        try (ServerSocketChannel channel = ServerSocketChannel.open()) {
            channel.bind(new InetSocketAddress("127.0.0.1", 0));
            channel.configureBlocking(false);
            final String[] outcome = {"<still blocked>"};
            Thread t = new Thread(() -> {
                try {
                    outcome[0] = channel.accept() == null ? "null" : "a-connection";
                } catch (Throwable other) {
                    outcome[0] = other.getClass().getName();
                }
            });
            t.setDaemon(true);
            t.start();
            t.join(10000);
            kv("nonBlocking.acceptResult", outcome[0]);
            kv("nonBlocking.HUNG", t.isAlive());
        }
    }
}
