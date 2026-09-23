import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/**
 * Regression: `sun.nio.ch.SocketChannelImpl.toString()` threw
 * `NullPointerException: Cannot enter synchronized block because
 * "this.stateLock" is null`.
 *
 * <h2>What broke</h2>
 *
 * `stateLock` is a `private final Object stateLock = new Object()` declared on
 * the CONCRETE `sun.nio.ch.SocketChannelImpl` / `ServerSocketChannelImpl`, and
 * every method on those classes that reports state locks it. CratonVM builds
 * its channels through `native-io`'s factories, which do not run the JDK
 * constructor, and `init_channel_locks` seeded only the three INHERITED monitor
 * fields (`closeLock`, `keyLock`, `regLock`). So `stateLock` read null and the
 * real `toString()` bytecode did `monitorenter` on it.
 *
 * <h2>Why that is not a cosmetic logging fault</h2>
 *
 * Every path Tomcat reaches `toString()` through is an ERROR or TEARDOWN path
 * that does not expect it to throw — `AbstractProcessorLight.process` building
 * a diagnostic string, `NioSocketWrapper.doClose`, and `StringManager.getString`
 * formatting the socket into a log message. The NPE propagated out of those and
 * aborted the request: 12 failures across `TestFlowControl`,
 * `TestHttp2Section_5_1`, `TestHttp2Section_6_1` and `TestAsyncContextImpl` in
 * the 2026-09-05 640-class ZGC run, each visible on the client as a truncated
 * response ("End of input stream with [9] bytes left to read", "connection
 * closed before response head", "Broken pipe").
 *
 * <h2>What this vector asserts</h2>
 *
 * That every channel state renders, and renders the way HotSpot renders it.
 * The suite diffs this output against HotSpot's, so the strings below ARE the
 * assertion — a VM that throws prints nothing and fails, and a VM that answers
 * a different string fails the diff. Ports are masked because they are
 * ephemeral; the surrounding structure is not.
 *
 * `configureBlocking(false)` is exercised between the two connected reads
 * because the real `AbstractSelectableChannel.configureBlocking` bytecode calls
 * `SocketChannelImpl.implConfigureBlocking`, whose body takes `readLock` (a
 * `private final ReentrantLock`, the same never-populated family one field
 * over) and then `synchronized (stateLock)`. That is the shape Tomcat's Tribes
 * `NioSender.configureSocket` hit as a bare NPE in the same run.
 */
public class RChannelToString {

    static int checks;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError("FAIL RChannelToString: " + what);
        }
    }

    /**
     * Replace every `:<digits>` port with `:P` so two runs of the same VM — and
     * a CratonVM run against a HotSpot run — produce identical text. The
     * literal `127.0.0.1` is deliberately NOT masked: which address the channel
     * reports is part of what is under test.
     */
    static String mask(Object channel) {
        return String.valueOf(channel).replaceAll(":\\d+", ":P");
    }

    public static void main(String[] args) throws IOException {
        ServerSocketChannel ssc = null;
        SocketChannel client = null;
        SocketChannel accepted = null;
        try {
            ssc = ServerSocketChannel.open();
            System.out.println("CK ssc-unbound " + mask(ssc));

            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            System.out.println("CK ssc-bound " + mask(ssc));

            int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();
            check(port > 0, "the listener must report a bound port");

            client = SocketChannel.open();
            System.out.println("CK sc-fresh " + mask(client));

            check(client.connect(new InetSocketAddress("127.0.0.1", port)),
                    "a blocking loopback connect must complete");
            accepted = ssc.accept();
            check(accepted != null, "accept must produce a channel");

            System.out.println("CK sc-connected " + mask(client));
            System.out.println("CK sc-accepted " + mask(accepted));

            // The `implConfigureBlocking` half: this runs the real
            // `AbstractSelectableChannel.configureBlocking` bytecode path, and
            // the rendering afterwards must be unchanged.
            client.configureBlocking(false);
            check(!client.isBlocking(), "configureBlocking(false) must take effect");
            System.out.println("CK sc-nonblocking " + mask(client));

            client.close();
            accepted.close();
            ssc.close();
            System.out.println("CK sc-closed " + mask(client));
            System.out.println("CK ssc-closed " + mask(ssc));

            System.out.println("CK RChannelToString checks=" + checks);
            System.out.println("PASS RChannelToString");
        } finally {
            closeQuietly(client);
            closeQuietly(accepted);
            closeQuietly(ssc);
        }
    }

    static void closeQuietly(java.nio.channels.Channel c) {
        if (c != null) {
            try {
                c.close();
            } catch (IOException ignored) {
                // teardown only
            }
        }
    }
}
