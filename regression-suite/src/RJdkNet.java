import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.PrintWriter;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketException;
import java.net.SocketTimeoutException;
import java.net.StandardSocketOptions;
import java.net.URI;
import java.net.UnknownHostException;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * JDK-only corpus: networking -- loopback TCP/UDP, DNS, socket options,
 * interruption.
 *
 * Everything is loopback-only and ephemeral-port: this vector must run on a CI
 * box with no outbound network. Determinism: PORT NUMBERS, host names and
 * interface names are NEVER printed -- they are per-run or per-host. Only the
 * bytes this vector chose to send, and shape predicates, are emitted.
 */
public class RJdkNet {
    static final long T = 30;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void dns() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        check(lo.isLoopbackAddress(), "getLoopbackAddress must be a loopback address");
        check(!lo.isAnyLocalAddress(), "loopback is not the wildcard address");

        InetAddress byName = InetAddress.getByName("localhost");
        check(byName.isLoopbackAddress(), "localhost must resolve to loopback");

        // Numeric literals must not need a resolver at all.
        InetAddress v4 = InetAddress.getByName("127.0.0.1");
        check(v4 instanceof Inet4Address, "127.0.0.1 is an Inet4Address");
        check(v4.getHostAddress().equals("127.0.0.1"), "getHostAddress: " + v4.getHostAddress());
        check(Arrays.equals(v4.getAddress(), new byte[] { 127, 0, 0, 1 }), "raw address bytes");
        check(v4.equals(InetAddress.getByAddress(new byte[] { 127, 0, 0, 1 })), "getByAddress");
        check(v4.hashCode() == InetAddress.getByAddress(new byte[] { 127, 0, 0, 1 }).hashCode(),
                "InetAddress hashCode");

        InetAddress v6 = InetAddress.getByName("::1");
        check(v6.isLoopbackAddress(), "::1 is loopback");
        check(v6.getAddress().length == 16, "IPv6 address length");

        // A guaranteed-nonexistent name (RFC 6761 reserves .invalid) must fail.
        boolean threw = false;
        try {
            InetAddress.getByName("cratonvm-no-such-host-20260731.invalid");
        } catch (UnknownHostException expected) {
            threw = true;
        }
        check(threw, "an unresolvable host must raise UnknownHostException");

        // Malformed literals are rejected, not silently coerced.
        threw = false;
        try {
            InetAddress.getByAddress(new byte[] { 1, 2, 3 });
        } catch (UnknownHostException expected) {
            threw = true;
        }
        check(threw, "a 3-byte address must be rejected");

        // InetSocketAddress arithmetic.
        InetSocketAddress sa = new InetSocketAddress(v4, 8080);
        check(sa.getPort() == 8080 && !sa.isUnresolved(), "InetSocketAddress");
        check(sa.getAddress().equals(v4), "socket address address");
        check(InetSocketAddress.createUnresolved("h", 1).isUnresolved(), "createUnresolved");
        threw = false;
        try {
            new InetSocketAddress(v4, 70000);
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "an out-of-range port must be rejected");

        // The loopback interface must be enumerable.
        NetworkInterface loIface = NetworkInterface.getByInetAddress(lo);
        check(loIface == null || loIface.isLoopback() || loIface.isUp(),
                "loopback interface shape");
        List<String> ifaceKinds = new ArrayList<>();
        for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
            ifaceKinds.add(ni.isLoopback() ? "loopback" : "other");
        }
        check(ifaceKinds.contains("loopback"), "at least one loopback interface must exist");

        // URI parsing is pure and must be exact.
        URI u = URI.create("http://example.invalid:8080/a/b?q=1#frag");
        check(u.getScheme().equals("http"), "URI scheme");
        check(u.getHost().equals("example.invalid"), "URI host");
        check(u.getPort() == 8080, "URI port");
        check(u.getPath().equals("/a/b"), "URI path");
        check(u.getQuery().equals("q=1"), "URI query");
        check(u.getFragment().equals("frag"), "URI fragment");
        System.out.println("CK RJdkNet dns loopback=" + v4.getHostAddress()
                + " v6len=" + v6.getAddress().length + " uri=" + u.getPath());
    }

    static void loopbackTcp() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        try (ServerSocket server = new ServerSocket(0, 16, lo)) {
            check(server.isBound() && !server.isClosed(), "server bound");
            check(server.getLocalPort() > 0, "an ephemeral port was assigned");
            final int port = server.getLocalPort();

            AtomicReference<String> echoed = new AtomicReference<>("none");
            CountDownLatch done = new CountDownLatch(1);
            Thread acceptor = new Thread(() -> {
                try (Socket s = server.accept();
                        BufferedReader in = new BufferedReader(
                                new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
                        PrintWriter out = new PrintWriter(
                                new java.io.OutputStreamWriter(s.getOutputStream(),
                                        StandardCharsets.UTF_8), true)) {
                    String line = in.readLine();
                    out.println("echo:" + line);
                    echoed.set(line == null ? "null" : line);
                } catch (IOException e) {
                    echoed.set("IOException");
                } finally {
                    done.countDown();
                }
            });
            acceptor.setDaemon(true);
            acceptor.start();

            try (Socket client = new Socket()) {
                client.connect(new InetSocketAddress(lo, port), (int) T * 1000);
                check(client.isConnected(), "client connected");
                check(client.getPort() == port, "client remote port");
                check(client.getInetAddress().isLoopbackAddress(), "connected to loopback");
                check(client.getLocalPort() > 0, "client local port assigned");

                PrintWriter out = new PrintWriter(new java.io.OutputStreamWriter(
                        client.getOutputStream(), StandardCharsets.UTF_8), true);
                out.println("ping");
                BufferedReader in = new BufferedReader(new InputStreamReader(
                        client.getInputStream(), StandardCharsets.UTF_8));
                String reply = in.readLine();
                check("echo:ping".equals(reply), "echo reply: " + reply);
            }
            check(done.await(T, TimeUnit.SECONDS), "acceptor never finished");
            check("ping".equals(echoed.get()), "server saw: " + echoed.get());

            // Socket options round-trip. Buffer sizes are OS-adjusted, so we
            // assert the boolean options only and never print any value.
            try (Socket s = new Socket()) {
                s.setTcpNoDelay(true);
                check(s.getTcpNoDelay(), "TCP_NODELAY");
                s.setKeepAlive(true);
                check(s.getKeepAlive(), "SO_KEEPALIVE");
                s.setReuseAddress(true);
                check(s.getReuseAddress(), "SO_REUSEADDR");
                s.setSoTimeout(1234);
                check(s.getSoTimeout() == 1234, "SO_TIMEOUT round-trip");
                s.setSoLinger(true, 5);
                check(s.getSoLinger() == 5, "SO_LINGER round-trip");
                check(s.getReceiveBufferSize() > 0, "SO_RCVBUF is positive");
                check(s.supportedOptions().contains(StandardSocketOptions.TCP_NODELAY),
                        "supportedOptions must include TCP_NODELAY");
                check(Boolean.TRUE.equals(s.getOption(StandardSocketOptions.TCP_NODELAY)),
                        "getOption(TCP_NODELAY)");
            }

            // Connecting to a closed port must be refused, not hang.
            boolean threw = false;
            try (ServerSocket tmp = new ServerSocket(0, 1, lo)) {
                int dead = tmp.getLocalPort();
                tmp.close();
                try (Socket s = new Socket()) {
                    s.connect(new InetSocketAddress(lo, dead), 2000);
                }
            } catch (IOException expected) {
                threw = true;
            }
            check(threw, "connecting to a closed loopback port must fail");
        }
        System.out.println("CK RJdkNet tcp echo=echo:ping");
    }

    static void soTimeoutAndAsyncClose() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        // SO_TIMEOUT on a read that nobody answers.
        try (ServerSocket server = new ServerSocket(0, 4, lo)) {
            int port = server.getLocalPort();
            try (Socket client = new Socket(lo, port);
                    Socket accepted = server.accept()) {
                client.setSoTimeout(150);
                boolean threw = false;
                try {
                    client.getInputStream().read();
                } catch (SocketTimeoutException expected) {
                    threw = true;
                }
                check(threw, "a read past SO_TIMEOUT must throw SocketTimeoutException");
                check(client.isConnected() && !client.isClosed(),
                        "the socket must survive a read timeout");
            }
        }

        // Closing a socket from another thread must break a blocked read.
        try (ServerSocket server = new ServerSocket(0, 4, lo)) {
            int port = server.getLocalPort();
            final Socket client = new Socket(lo, port);
            try (Socket accepted = server.accept()) {
                CountDownLatch reading = new CountDownLatch(1);
                CountDownLatch done = new CountDownLatch(1);
                AtomicReference<String> outcome = new AtomicReference<>("none");
                Thread reader = new Thread(() -> {
                    reading.countDown();
                    try {
                        int r = client.getInputStream().read();
                        outcome.set("returned:" + r);
                    } catch (SocketException e) {
                        outcome.set("SocketException");
                    } catch (IOException e) {
                        outcome.set(e.getClass().getSimpleName());
                    }
                    done.countDown();
                });
                reader.setDaemon(true);
                reader.start();
                check(reading.await(T, TimeUnit.SECONDS), "reader never started");
                Thread.sleep(200);
                client.close();
                check(done.await(T, TimeUnit.SECONDS), "the blocked reader never woke up");
                check(outcome.get().equals("SocketException"),
                        "close-during-read outcome: " + outcome.get());
                check(client.isClosed(), "socket closed");
                System.out.println("CK RJdkNet asyncClose=" + outcome.get());
            }
        }
    }

    static void loopbackUdp() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        try (DatagramSocket server = new DatagramSocket(0, lo);
                DatagramSocket client = new DatagramSocket(0, lo)) {
            check(server.isBound() && server.getLocalPort() > 0, "UDP server bound");
            server.setSoTimeout((int) T * 1000);

            byte[] payload = "udp-ping".getBytes(StandardCharsets.UTF_8);
            client.send(new DatagramPacket(payload, payload.length, lo, server.getLocalPort()));

            byte[] buf = new byte[64];
            DatagramPacket in = new DatagramPacket(buf, buf.length);
            server.receive(in);
            String got = new String(in.getData(), in.getOffset(), in.getLength(),
                    StandardCharsets.UTF_8);
            check(got.equals("udp-ping"), "UDP payload: " + got);
            check(in.getLength() == payload.length, "UDP length");
            check(in.getAddress().isLoopbackAddress(), "UDP source is loopback");
            check(in.getPort() == client.getLocalPort(), "UDP source port");

            // Reply to the sender's reported address.
            byte[] reply = "udp-pong".getBytes(StandardCharsets.UTF_8);
            server.send(new DatagramPacket(reply, reply.length, in.getAddress(), in.getPort()));
            client.setSoTimeout((int) T * 1000);
            DatagramPacket back = new DatagramPacket(new byte[64], 64);
            client.receive(back);
            check(new String(back.getData(), back.getOffset(), back.getLength(),
                    StandardCharsets.UTF_8).equals("udp-pong"), "UDP reply");

            // A timed-out receive must throw, not block forever.
            server.setSoTimeout(150);
            boolean threw = false;
            try {
                server.receive(new DatagramPacket(new byte[8], 8));
            } catch (SocketTimeoutException expected) {
                threw = true;
            }
            check(threw, "UDP receive past SO_TIMEOUT must throw SocketTimeoutException");

            // connect() pins the peer.
            client.connect(lo, server.getLocalPort());
            check(client.isConnected(), "UDP connect");
            check(client.getPort() == server.getLocalPort(), "UDP connected port");
            client.disconnect();
            check(!client.isConnected(), "UDP disconnect");
            System.out.println("CK RJdkNet udp=" + got + "/udp-pong");
        }
    }

    public static void main(String[] args) throws Exception {
        dns();
        loopbackTcp();
        soTimeoutAndAsyncClose();
        loopbackUdp();
        System.out.println("CK RJdkNet checks=" + checks);
        System.out.println("PASS RJdkNet (" + checks + " checks)");
    }
}
