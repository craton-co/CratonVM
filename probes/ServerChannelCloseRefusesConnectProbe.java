import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.concurrent.CountDownLatch;

/**
 * `ServerSocketChannel.close()` must make the port stop listening by the time
 * it returns.
 *
 * This is the contract Jetty's graceful shutdown rests on:
 * `GracefulShutdown.shutDownGracefully` calls `connector.shutdown()`, which
 * calls `ServerConnector.close()`, which calls `IO.close(_acceptChannel)` —
 * all synchronously, on the caller's thread — and Spring Boot's
 * `JettyServletWebServerFactoryTests
 * .whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` then
 * connects and asserts it is REFUSED (`HttpHostConnectException`).
 *
 * The shape that broke it: an acceptor thread parked in a blocking `accept()`
 * held a `try_clone()`d duplicate of the listening socket, so closing the
 * original left the port open until the acceptor next looked — and any
 * connection that arrived first was accepted and served. The test saw
 * `404 Not Found` instead of a refusal, only under load, because the window
 * is one accept-poll wide.
 *
 * So the probe reproduces that exact shape and nothing else: a real acceptor
 * thread blocked in `accept()`, a `close()` from another thread, and a connect
 * attempt immediately afterwards. `-Dprobe.rounds=N` raises the round count;
 * each round is a fresh port, and the connect is retried a few times per round
 * because a single attempt can miss a window this narrow.
 *
 *   cratonvm --java-home &lt;jdk&gt; -cp probes ServerChannelCloseRefusesConnectProbe
 *
 * Prints `PASS` and exits 0 when every post-close connect was refused.
 */
public class ServerChannelCloseRefusesConnectProbe {

    private static final int CONNECTS_PER_ROUND = 4;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 25;
        int served = 0;
        int refused = 0;

        for (int round = 0; round < rounds; round++) {
            ServerSocketChannel server = ServerSocketChannel.open();
            server.bind(new InetSocketAddress("127.0.0.1", 0), 50);
            int port = ((InetSocketAddress) server.getLocalAddress()).getPort();

            CountDownLatch accepting = new CountDownLatch(1);
            Thread acceptor = new Thread(() -> {
                accepting.countDown();
                try {
                    // Blocking accept, exactly like a Jetty acceptor thread.
                    // Everything accepted here arrived before the close, or is
                    // the defect this probe exists for.
                    while (true) {
                        SocketChannel accepted = server.accept();
                        if (accepted == null) {
                            return;
                        }
                        accepted.close();
                    }
                } catch (ClosedChannelException expected) {
                    // close() reached us — the correct way out.
                } catch (IOException expected) {
                    // Windows reports the closed listener as a plain IOException.
                }
            }, "probe-acceptor-" + round);
            acceptor.setDaemon(true);
            acceptor.start();
            accepting.await();
            // Let the acceptor actually reach accept() before closing, so the
            // close lands with a thread parked inside it.
            Thread.sleep(20);

            server.close();

            for (int i = 0; i < CONNECTS_PER_ROUND; i++) {
                try (SocketChannel client = SocketChannel.open()) {
                    if (client.connect(new InetSocketAddress("127.0.0.1", port))) {
                        served++;
                        System.out.println("FAIL round=" + round + " attempt=" + i
                                + " connected to port " + port + " AFTER close() returned");
                    } else {
                        refused++;
                    }
                } catch (IOException refusedAsExpected) {
                    refused++;
                }
            }
            acceptor.join(2000);
        }

        System.out.println("CK rounds=" + rounds + " refused=" + refused + " served=" + served);
        if (served != 0) {
            throw new AssertionError(served + " connection(s) were accepted after "
                    + "ServerSocketChannel.close() returned; the listening socket "
                    + "outlived the close");
        }
        System.out.println("PASS ServerChannelCloseRefusesConnectProbe");
    }
}
