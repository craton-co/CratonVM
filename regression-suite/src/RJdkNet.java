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

    /**
     * The WRITE and ACCEPT twins of the async-close family (W7-53), on the
     * surfaces a default run actually takes.
     *
     * WHY THIS BLOCK EXISTS. `soTimeoutAndAsyncClose` above covers exactly one
     * of the family's thirteen Java-reachable shapes -- a blocked READ. The
     * write twin was fixed on 2026-08-12 (`native-io/src/net.rs`,
     * `net_write_close_aware`) and the accept twin on 2026-05-17
     * (`net_accept_close_aware`), and NEITHER had a scheduled assertion: the
     * family's own instrument, `probes/AsyncCloseProbe.java`, is not in
     * `src/` and `run.sh` reads a word list, so it has never run in any suite.
     *
     * WHICH NATIVE THIS REACHES. `real_net_sockets` is default-ON, so
     * `java.net.Socket`/`ServerSocket` run real JDK bytecode down to
     * `sun/nio/ch/SocketDispatcher.write0` and `sun/nio/ch/Net.accept`, both
     * registered `NativeKind::Bridge` by `native-io`'s
     * `net::register_sun_nio_ch_net` -- reached from
     * `nio_native::register_t16_channel_overrides` and thence from
     * `register_io_natives`, which `vm_init` calls on all three boot arms. So
     * this is the SHIPPING registrar, and `Bridge` is not a kind `--jdk-only`
     * drops: Compatible and strict run the same bodies.
     *
     * WHY `SocketException` IS THE RIGHT ASSERTION ON BOTH VMS.
     * `NioSocketImpl.implWrite` (JDK 25, src.zip) catches every `IOException`
     * from the dispatcher and rethrows `asSocketException(ioe)` -- "throw
     * SocketException to maintain compatibility" -- and `endWrite`/`endAccept`
     * throw `SocketException("Socket closed")` in their `finally` whenever the
     * call did not complete and the impl is `>= ST_CLOSING`. So the concrete
     * type does not depend on which error the native chose; what the native
     * must do is RETURN. Before the fixes it did not: `close_net_fd` marks the
     * registry slot and issues `shutdown(Both)`, but cannot take the OS handle
     * away from a thread holding an `Arc` clone of the `TcpStream`, and Winsock
     * has no `shutdown` that aborts a pending blocking call.
     *
     * IT CANNOT HANG, AND IT CANNOT BE VACUOUS. Every wait is bounded by `T`,
     * so a native that stays parked yields a FAIL rather than a suite timeout;
     * both workers are daemons; the write row closes the ACCEPTED end in a
     * `finally` BEFORE it asserts, so even a completely unfixed VM has its
     * writer released by the peer's reset instead of left in the kernel. The
     * `...WasBlocked` checks are the anti-vacuity guards: a row whose worker had
     * already returned before the close was issued tested nothing, and says so
     * instead of passing.
     */
    static void asyncCloseWriteAndAccept() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();

        // A peer that never reads: 64 MiB cannot fit in any socket buffer pair,
        // so the writer is certain to be parked in `send` when the close lands.
        // Sliced into 256 KiB writes so the vector never allocates 64 MiB.
        final int chunk = 256 * 1024;
        final int chunks = 256;

        String writeOutcome = "none";
        boolean writeWasBlocked = false;
        boolean writeWoke = false;
        try (ServerSocket server = new ServerSocket(0, 4, lo)) {
            final Socket client = new Socket(lo, server.getLocalPort());
            Socket accepted = server.accept();
            CountDownLatch writing = new CountDownLatch(1);
            CountDownLatch done = new CountDownLatch(1);
            AtomicReference<String> outcome = new AtomicReference<>("none");
            try {
                Thread writer = new Thread(() -> {
                    byte[] payload = new byte[chunk];
                    try {
                        java.io.OutputStream out = client.getOutputStream();
                        writing.countDown();
                        for (int i = 0; i < chunks; i++) {
                            out.write(payload);
                        }
                        outcome.set("completed");
                    } catch (SocketException e) {
                        outcome.set("SocketException");
                    } catch (IOException e) {
                        outcome.set(e.getClass().getSimpleName());
                    }
                    done.countDown();
                });
                writer.setDaemon(true);
                writer.start();
                check(writing.await(T, TimeUnit.SECONDS), "the writer never started");
                Thread.sleep(300);
                writeWasBlocked = done.getCount() == 1;
                client.close();
                writeWoke = done.await(T, TimeUnit.SECONDS);
            } finally {
                // Releases the writer even on a VM where the close is invisible
                // to it, so a FAIL here never leaves a thread in the kernel.
                accepted.close();
                try {
                    client.close();
                } catch (IOException ignored) {
                    // close() is idempotent; a second one is not a failure.
                }
            }
            writeOutcome = outcome.get();
        }
        check(writeWasBlocked,
                "64 MiB into an undrained loopback socket must still be in flight when the "
                        + "close is issued, else the write row proves nothing: " + writeOutcome);
        check(writeWoke, "the blocked writer never woke up: " + writeOutcome);
        check(writeOutcome.equals("SocketException"),
                "close-during-write outcome: " + writeOutcome);
        System.out.println("CK RJdkNet asyncCloseWrite=" + writeOutcome);

        String acceptOutcome = "none";
        boolean acceptWasBlocked = false;
        boolean acceptWoke = false;
        final ServerSocket listener = new ServerSocket(0, 1, lo);
        try {
            CountDownLatch accepting = new CountDownLatch(1);
            CountDownLatch acceptDone = new CountDownLatch(1);
            AtomicReference<String> outcome = new AtomicReference<>("none");
            Thread acceptor = new Thread(() -> {
                accepting.countDown();
                try {
                    Socket s = listener.accept();
                    s.close();
                    outcome.set("accepted");
                } catch (SocketException e) {
                    outcome.set("SocketException");
                } catch (IOException e) {
                    outcome.set(e.getClass().getSimpleName());
                }
                acceptDone.countDown();
            });
            acceptor.setDaemon(true);
            acceptor.start();
            check(accepting.await(T, TimeUnit.SECONDS), "the acceptor never started");
            Thread.sleep(300);
            acceptWasBlocked = acceptDone.getCount() == 1;
            listener.close();
            acceptWoke = acceptDone.await(T, TimeUnit.SECONDS);
            acceptOutcome = outcome.get();
        } finally {
            listener.close();
        }
        check(acceptWasBlocked,
                "nothing ever connects to this listener, so the accept must still be blocked "
                        + "when the close is issued: " + acceptOutcome);
        check(acceptWoke, "the blocked accept never woke up: " + acceptOutcome);
        check(acceptOutcome.equals("SocketException"),
                "close-during-accept outcome: " + acceptOutcome);
        System.out.println("CK RJdkNet asyncCloseAccept=" + acceptOutcome);
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

    /** "iae" when the body raised IllegalArgumentException, "se" for SocketException, else a token. */
    static String reject(ThrowingBody body) {
        try {
            body.run();
            return "accepted";
        } catch (IllegalArgumentException expected) {
            return "iae";
        } catch (SocketException expected) {
            return "se";
        } catch (Exception other) {
            return "wrong-type:" + other.getClass().getName();
        }
    }

    interface ThrowingBody {
        void run() throws Exception;
    }

    /**
     * A NEGATIVE SO_TIMEOUT is refused on every socket surface, and a zero one
     * is not.
     *
     * WHY THIS BLOCK EXISTS. `setSoTimeout(-1)` is how a miscomputed deadline
     * (`remaining = end - now`, gone negative) arrives at a socket. JDK 25
     * refuses it on all four surfaces, with the check placed AFTER the
     * closed-socket check:
     *
     *   java.net.Socket:1276           IllegalArgumentException("timeout can't be negative")
     *   java.net.ServerSocket:709      IllegalArgumentException("timeout < 0")
     *   sun.nio.ch.DatagramSocketAdaptor:234  IllegalArgumentException("timeout < 0")
     *                                  (this is what both java.net.DatagramSocket.setSoTimeout
     *                                   and DatagramChannel.socket() delegate to)
     *
     * CratonVM's `DatagramChannel` bridge (`native-io/src/lib.rs`,
     * `register_datagram_channel`, registered in every arm) instead did
     * `.max(0)` and then read `0` as "no timeout" — so a caller asking for a
     * bounded receive got an UNBOUNDED one, which is the single value that
     * cannot be told apart from a healthy configuration. That is the row this
     * asserts; the three JDK-served surfaces beside it are the controls that say
     * the rule is the JDK's and not this vector's invention.
     *
     * THE POSITIVE HALF IS NOT OPTIONAL. Without the `accepted` rows a VM that
     * threw on EVERY timeout would satisfy all four refusals — and `0` is the
     * value that must keep working, because `0` means "block forever" and every
     * default-constructed socket has it.
     */
    static void negativeSoTimeout() throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();

        try (java.nio.channels.DatagramChannel ch = java.nio.channels.DatagramChannel.open()) {
            // `DatagramChannel.socket()` is specified to return a
            // java.net.DatagramSocket. Published rather than assumed: CratonVM
            // has a native on this triple that used to answer the CHANNEL
            // itself, and if that ever wins here the failure arrives as a
            // ClassCastException on this line rather than on the timeout row
            // below, which would otherwise read as a timeout defect.
            DatagramSocket obtained = null;
            String outcome;
            try {
                obtained = ch.socket();
                outcome = "ok";
            } catch (Throwable t) {
                // A native answering the channel itself fails the assignment's
                // type, so catch Throwable: a ClassCastException here would
                // otherwise kill the vector with no PASS line and no clue.
                outcome = "threw:" + t.getClass().getName();
            }
            // THE CLASS NAME IS DELIBERATELY NOT PUBLISHED. This row used to
            // print `obtained.getClass().getName()`, which reads
            // `sun.nio.ch.DatagramSocketAdaptor` on HotSpot — a java.base
            // INTERNAL class that `DatagramChannel.socket()`'s contract never
            // names. A correct implementation is free to call its adaptor
            // something else, so the raw name diffed a VM's private spelling
            // rather than its behaviour. Publish instead the two facts the JDK
            // does specify, which are the same two the defect violates.
            //
            // AND THE CHECK BESIDE IT WAS VACUOUS. It asserted `obtained !=
            // null`, which cannot see this defect at all: javac emits NO
            // checkcast at a call whose DECLARED return type already satisfies
            // the assignment, so a native answering the channel lands in a
            // `DatagramSocket`-typed local intact and non-null. Only a real
            // `instanceof` catches it, and only through an Object-typed local —
            // `obtained instanceof DatagramSocket` on a DatagramSocket-typed
            // local is the same vacuous test in a different spelling.
            Object answer = obtained;
            boolean isDatagramSocket = answer instanceof DatagramSocket;
            boolean isTheChannel = (answer == ch);
            System.out.println("CK RJdkNet dcSocketAdaptor=" + outcome
                    + ",isDatagramSocket=" + isDatagramSocket
                    + ",isChannel=" + isTheChannel);
            check(isDatagramSocket,
                    "DatagramChannel.socket() must return a java.net.DatagramSocket, got "
                            + (answer == null ? outcome : answer.getClass().getName()));
            check(!isTheChannel,
                    "DatagramChannel.socket() must not answer the CHANNEL itself: a"
                            + " java.nio.channels.DatagramChannel is not a java.net.DatagramSocket");
            final DatagramSocket adaptor = obtained;
            String neg = reject(() -> adaptor.setSoTimeout(-1));
            check(neg.equals("iae"), "DatagramChannel.socket().setSoTimeout(-1): " + neg);
            // The value a caller actually gets wrong most often after -1.
            String negBig = reject(() -> adaptor.setSoTimeout(Integer.MIN_VALUE));
            check(negBig.equals("iae"),
                    "DatagramChannel.socket().setSoTimeout(MIN_VALUE): " + negBig);
            // ... and the two that must still be accepted.
            check(reject(() -> adaptor.setSoTimeout(0)).equals("accepted"),
                    "setSoTimeout(0) means no timeout and must be accepted");
            check(reject(() -> adaptor.setSoTimeout(1234)).equals("accepted"),
                    "a positive timeout must be accepted");
            System.out.println("CK RJdkNet dcSoTimeoutNegative=" + neg + "," + negBig);
        }

        try (DatagramSocket ds = new DatagramSocket(0, lo)) {
            check(reject(() -> ds.setSoTimeout(-1)).equals("iae"), "DatagramSocket.setSoTimeout(-1)");
            check(reject(() -> ds.setSoTimeout(0)).equals("accepted"), "DatagramSocket zero timeout");
            check(ds.getSoTimeout() == 0, "a refused setSoTimeout must not have taken effect");
        }

        try (Socket s = new Socket()) {
            check(reject(() -> s.setSoTimeout(-1)).equals("iae"), "Socket.setSoTimeout(-1)");
            s.setSoTimeout(77);
            check(reject(() -> s.setSoTimeout(-1)).equals("iae"), "Socket.setSoTimeout(-1) again");
            check(s.getSoTimeout() == 77, "a refused setSoTimeout must leave the old value");
        }

        try (ServerSocket ss = new ServerSocket(0, 1, lo)) {
            check(reject(() -> ss.setSoTimeout(-1)).equals("iae"), "ServerSocket.setSoTimeout(-1)");
        }

        // ORDER OF CHECKS. The closed-socket refusal comes FIRST upstream, so a
        // closed socket with a negative timeout is a SocketException, not an
        // IllegalArgumentException. Asserted only on the two surfaces that run
        // real JDK bytecode here: CratonVM's DatagramChannel bridge has no
        // closed check at all, which is stated in that native's comment rather
        // than asserted as though it were fixed.
        Socket closed = new Socket();
        closed.close();
        String closedOrder = reject(() -> closed.setSoTimeout(-1));
        check(closedOrder.equals("se"), "closed beats negative on Socket: " + closedOrder);
        ServerSocket closedServer = new ServerSocket(0, 1, lo);
        closedServer.close();
        String closedServerOrder = reject(() -> closedServer.setSoTimeout(-1));
        check(closedServerOrder.equals("se"),
                "closed beats negative on ServerSocket: " + closedServerOrder);
        System.out.println("CK RJdkNet soTimeoutClosedFirst=" + closedOrder + ","
                + closedServerOrder);
    }

    public static void main(String[] args) throws Exception {
        dns();
        loopbackTcp();
        soTimeoutAndAsyncClose();
        asyncCloseWriteAndAccept();
        loopbackUdp();
        negativeSoTimeout();
        System.out.println("CK RJdkNet checks=" + checks);
        System.out.println("PASS RJdkNet (" + checks + " checks)");
    }
}
