import org.eclipse.jetty.server.Server;
import org.eclipse.jetty.server.ServerConnector;
import org.eclipse.jetty.websocket.api.Callback;
import org.eclipse.jetty.websocket.api.Session;
import org.eclipse.jetty.websocket.client.WebSocketClient;
import org.eclipse.jetty.ee11.websocket.server.JettyWebSocketServlet;
import org.eclipse.jetty.ee11.websocket.server.JettyWebSocketServletFactory;
import org.eclipse.jetty.ee11.servlet.ServletContextHandler;
import org.eclipse.jetty.ee11.servlet.ServletHolder;

import java.net.URI;
import java.nio.ByteBuffer;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

// JETTY-WSIO.1 repro: real Jetty embedded server + Jetty WebSocket client
// exchanging large payloads over many round trips. The original ban's
// comment says the websocket-core and io packages need to be compiled
// together to reproduce a large-payload receive stall/timeout. This probe
// opens one real WebSocket connection and sends many large binary frames
// echoed back by the server, verifying every payload round-trips intact and
// without stalling, to cross JIT invocation thresholds on both sides.
// Uses the programmatic Session.Listener interface (not the @WebSocket
// annotation API) to avoid reflection-based signature scanning entirely.
public class JettyWsIoProbe {

    public static class EchoServerSocket implements Session.Listener {
        private Session session;

        @Override
        public void onWebSocketOpen(Session session) {
            this.session = session;
            session.demand();
        }

        @Override
        public void onWebSocketBinary(ByteBuffer payload, Callback callback) {
            session.sendBinary(payload, Callback.from(() -> {
                callback.succeed();
                session.demand();
            }, callback::fail));
        }

        @Override
        public void onWebSocketError(Throwable cause) {
        }
    }

    public static class ClientSocket implements Session.Listener {
        final AtomicInteger received;
        final CountDownLatch[] latchHolder;
        final ByteBuffer[] lastReceived;
        private Session session;

        ClientSocket(AtomicInteger received, CountDownLatch[] latchHolder, ByteBuffer[] lastReceived) {
            this.received = received;
            this.latchHolder = latchHolder;
            this.lastReceived = lastReceived;
        }

        @Override
        public void onWebSocketOpen(Session session) {
            this.session = session;
            session.demand();
        }

        @Override
        public void onWebSocketBinary(ByteBuffer payload, Callback callback) {
            ByteBuffer copy = ByteBuffer.allocate(payload.remaining());
            copy.put(payload);
            copy.flip();
            lastReceived[0] = copy;
            received.incrementAndGet();
            callback.succeed();
            latchHolder[0].countDown();
            session.demand();
        }

        @Override
        public void onWebSocketError(Throwable cause) {
        }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 100;
        int payloadSize = args.length > 1 ? Integer.parseInt(args[1]) : 65536;

        Server server = new Server();
        ServerConnector connector = new ServerConnector(server);
        connector.setPort(0);
        server.addConnector(connector);

        ServletContextHandler context = new ServletContextHandler(ServletContextHandler.SESSIONS);
        context.setContextPath("/");
        server.setHandler(context);

        JettyWebSocketServlet servlet = new JettyWebSocketServlet() {
            @Override
            protected void configure(JettyWebSocketServletFactory factory) {
                factory.setMaxTextMessageSize(8 * 1024 * 1024);
                factory.setMaxBinaryMessageSize(8 * 1024 * 1024);
                factory.setIdleTimeout(java.time.Duration.ofSeconds(30));
                factory.setCreator((req, resp) -> new EchoServerSocket());
            }
        };
        context.addServlet(new ServletHolder(servlet), "/echo");
        org.eclipse.jetty.ee11.websocket.server.config.JettyWebSocketServletContainerInitializer
                .configure(context, null);

        server.start();
        int port = connector.getLocalPort();

        WebSocketClient client = new WebSocketClient();
        client.start();

        int failures = 0;
        try {
            URI uri = URI.create("ws://localhost:" + port + "/echo");
            org.eclipse.jetty.websocket.client.ClientUpgradeRequest req =
                    new org.eclipse.jetty.websocket.client.ClientUpgradeRequest();

            AtomicInteger received = new AtomicInteger(0);
            ByteBuffer[] lastReceived = new ByteBuffer[1];
            CountDownLatch[] latchHolder = new CountDownLatch[1];
            ClientSocket socket = new ClientSocket(received, latchHolder, lastReceived);

            var connectFuture = client.connect(socket, uri, req);
            Session clientSession = connectFuture.get(10, TimeUnit.SECONDS);

            java.util.Random rnd = new java.util.Random(42);
            for (int i = 0; i < iterations; i++) {
                byte[] payload = new byte[payloadSize];
                rnd.nextBytes(payload);
                payload[0] = (byte) (i & 0xFF);
                payload[1] = (byte) ((i >> 8) & 0xFF);

                latchHolder[0] = new CountDownLatch(1);
                clientSession.sendBinary(ByteBuffer.wrap(payload), Callback.NOOP);

                boolean got = latchHolder[0].await(10, TimeUnit.SECONDS);
                if (!got) {
                    failures++;
                    if (failures <= 5) {
                        System.out.println("STALL at i=" + i + " (no echo within 10s)");
                    }
                    continue;
                }
                ByteBuffer back = lastReceived[0];
                byte[] backArr = new byte[back.remaining()];
                back.get(backArr);
                boolean matches = backArr.length == payload.length
                        && java.util.Arrays.equals(backArr, payload);
                if (!matches) {
                    failures++;
                    if (failures <= 5) {
                        System.out.println("CORRUPTION at i=" + i + " sentLen=" + payload.length
                                + " gotLen=" + backArr.length);
                    }
                }

                if (i % 10 == 0) {
                    System.out.println("progress i=" + i + " received=" + received.get());
                    System.out.flush();
                }
            }

            clientSession.close();
        } finally {
            client.stop();
            server.stop();
        }

        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
