import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.BindException;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketAddress;
import java.net.SocketException;
import java.net.SocketTimeoutException;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Differential probe for the plain (non-channel) `java.net.ServerSocket` surface —
 * the one `docs/known-issues/tomcat/serversocket-bind-socketaddress-noop-localport-zero.md`
 * is about: `new ServerSocket()` followed by `bind(SocketAddress)`.
 *
 * Every line is `key=value` with values normalised so that a correct VM prints
 * BYTE-IDENTICAL output to HotSpot (ephemeral ports are reported as `>0`/`==bound`,
 * never as the literal number). Run it on HotSpot first, then on CratonVM, and
 * diff: any differing line is a defect.
 */
public class PlainServerSocketBindProbe {

    static void kv(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /** Normalise a port: real ephemeral ports vary run to run. */
    static String port(int p) {
        if (p > 0 && p < 65536) return "POSITIVE";
        return String.valueOf(p);
    }

    /** Exception shape: class simple name only (messages differ across JDKs). */
    static String ex(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    interface Section {
        void run() throws Exception;
    }

    /**
     * Run one section on its own daemon thread with a hard wall-clock bound. A
     * section that never returns (e.g. an `accept()` that ignores SO_TIMEOUT)
     * would otherwise take the whole probe with it and leave every later
     * scenario unmeasured — the exact shape this probe exists to catch.
     */
    static void section(String name, Section body) {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (Throwable e) {
                kv(name + ".UNEXPECTED-THROW", ex(e));
            }
        }, "section-" + name);
        t.setDaemon(true);
        t.start();
        try {
            t.join(30000);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
        }
        kv(name + ".HUNG", t.isAlive());
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        section("ctor", PlainServerSocketBindProbe::ctorForm);
        section("plain", PlainServerSocketBindProbe::plainBind);
        section("backlog", PlainServerSocketBindProbe::plainBindBacklog);
        section("bindNull", PlainServerSocketBindProbe::bindNull);
        section("state", PlainServerSocketBindProbe::isBoundIsClosed);
        section("accept", PlainServerSocketBindProbe::acceptEndToEnd);
        section("closeWakesAccept", PlainServerSocketBindProbe::closeWakesBlockedAccept);
        section("soTimeoutBeforeBind", PlainServerSocketBindProbe::soTimeoutBeforeBind);
        section("soTimeoutAfterBind", PlainServerSocketBindProbe::soTimeoutAfterBind);
        section("reuse", PlainServerSocketBindProbe::reuseAddressBeforeBind);
        section("recvBuf", PlainServerSocketBindProbe::recvBufferBeforeBind);
        section("doubleBind", PlainServerSocketBindProbe::doubleBind);
        section("bindAfterClose", PlainServerSocketBindProbe::bindAfterClose);
        section("acceptOnUnbound", PlainServerSocketBindProbe::acceptOnUnbound);
        section("postClose", PlainServerSocketBindProbe::postCloseAccessors);
        section("inetAddr", PlainServerSocketBindProbe::getInetAddressAfterPlainBind);
        System.out.println("PROBE-DONE");
        System.out.flush();
        // A section left hung owns a live socket and a parked thread. They are
        // daemons, so exit() is enough to drop them once the verdict is out —
        // and unlike halt() it does not need `java.lang.Shutdown.beforeHalt`,
        // which CratonVM does not implement in real-JDK mode.
        System.exit(0);
    }

    // 1. The constructor form always worked; keep it as the in-probe control.
    static void ctorForm() throws Exception {
        try (ServerSocket a = new ServerSocket(0)) {
            kv("ctor.localPort", port(a.getLocalPort()));
            kv("ctor.isBound", a.isBound());
            kv("ctor.localSocketAddress.null", a.getLocalSocketAddress() == null);
        }
    }

    // 2. The doc's minimal repro.
    static void plainBind() throws Exception {
        ServerSocket b = new ServerSocket();
        kv("plain.beforeBind.isBound", b.isBound());
        kv("plain.beforeBind.localPort", b.getLocalPort());
        b.bind(new InetSocketAddress("localhost", 0));
        int p = b.getLocalPort();
        kv("plain.localPort", port(p));
        SocketAddress sa = b.getLocalSocketAddress();
        kv("plain.localSocketAddress.null", sa == null);
        kv("plain.localSocketAddress.isISA", sa instanceof InetSocketAddress);
        if (sa instanceof InetSocketAddress isa) {
            kv("plain.localSocketAddress.portMatches", isa.getPort() == p);
            kv("plain.localSocketAddress.isLoopback", isa.getAddress() != null
                    && isa.getAddress().isLoopbackAddress());
        }
        kv("plain.isBound", b.isBound());
        kv("plain.isClosed", b.isClosed());
        b.close();
    }

    // 3. bind(SocketAddress, int backlog) — the second shadowed descriptor.
    static void plainBindBacklog() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("localhost", 0), 10);
        kv("backlog.localPort", port(b.getLocalPort()));
        kv("backlog.isBound", b.isBound());
        b.close();
    }

    // 4. bind(null) is legal: ephemeral port on the wildcard address.
    static void bindNull() throws Exception {
        ServerSocket b = new ServerSocket();
        String err = "none";
        try {
            b.bind(null);
        } catch (Throwable t) {
            err = ex(t);
        }
        kv("bindNull.error", err);
        kv("bindNull.localPort", port(b.getLocalPort()));
        kv("bindNull.isBound", b.isBound());
        b.close();
    }

    // 5. isBound / isClosed transitions.
    static void isBoundIsClosed() throws Exception {
        ServerSocket b = new ServerSocket();
        kv("state.new.isBound", b.isBound());
        kv("state.new.isClosed", b.isClosed());
        b.bind(new InetSocketAddress("localhost", 0));
        kv("state.bound.isBound", b.isBound());
        kv("state.bound.isClosed", b.isClosed());
        b.close();
        kv("state.closed.isBound", b.isBound());
        kv("state.closed.isClosed", b.isClosed());
    }

    // 6. bind() then accept() must actually serve a connection.
    static void acceptEndToEnd() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        final int p = b.getLocalPort();
        final AtomicReference<String> got = new AtomicReference<>("<none>");
        Thread server = new Thread(() -> {
            try (Socket s = b.accept()) {
                InputStream in = s.getInputStream();
                byte[] buf = new byte[5];
                int n = 0;
                while (n < 5) {
                    int r = in.read(buf, n, 5 - n);
                    if (r < 0) break;
                    n += r;
                }
                got.set(new String(buf, 0, n, "UTF-8"));
                OutputStream out = s.getOutputStream();
                out.write("PONG!".getBytes("UTF-8"));
                out.flush();
            } catch (Throwable t) {
                got.set("ERR:" + ex(t));
            }
        });
        server.start();
        String reply = "<none>";
        try (Socket c = new Socket("127.0.0.1", p)) {
            c.getOutputStream().write("PING!".getBytes("UTF-8"));
            c.getOutputStream().flush();
            byte[] buf = new byte[5];
            int n = 0;
            InputStream in = c.getInputStream();
            while (n < 5) {
                int r = in.read(buf, n, 5 - n);
                if (r < 0) break;
                n += r;
            }
            reply = new String(buf, 0, n, "UTF-8");
        }
        server.join(10000);
        kv("accept.serverReceived", got.get());
        kv("accept.clientReceived", reply);
        b.close();
    }

    // 7. close() must wake a thread blocked in accept() (okhttp MockWebServer teardown).
    static void closeWakesBlockedAccept() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        final AtomicReference<String> outcome = new AtomicReference<>("<blocked>");
        Thread t = new Thread(() -> {
            try {
                b.accept();
                outcome.set("accepted");
            } catch (Throwable e) {
                outcome.set(ex(e));
            }
        });
        t.setDaemon(true);
        t.start();
        Thread.sleep(300);
        b.close();
        t.join(5000);
        // HotSpot throws java.net.SocketException from the blocked accept.
        kv("closeWakesAccept.outcome", outcome.get());
        kv("closeWakesAccept.threadFinished", !t.isAlive());
    }

    // 8. setSoTimeout on the UNBOUND socket must survive the later bind().
    static void soTimeoutBeforeBind() throws Exception {
        ServerSocket b = new ServerSocket();
        b.setSoTimeout(700);
        kv("soTimeoutBeforeBind.readBackUnbound", b.getSoTimeout());
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        kv("soTimeoutBeforeBind.readBackBound", b.getSoTimeout());
        long start = System.nanoTime();
        String err = "none";
        try {
            b.accept();
        } catch (Throwable e) {
            err = ex(e);
        }
        long ms = (System.nanoTime() - start) / 1_000_000;
        kv("soTimeoutBeforeBind.acceptError", err);
        kv("soTimeoutBeforeBind.timedOutPromptly", ms >= 400 && ms < 5000);
        b.close();
    }

    // 9. setSoTimeout after bind (the ordering that already worked).
    static void soTimeoutAfterBind() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        b.setSoTimeout(700);
        kv("soTimeoutAfterBind.readBack", b.getSoTimeout());
        String err = "none";
        long start = System.nanoTime();
        try {
            b.accept();
        } catch (Throwable e) {
            err = ex(e);
        }
        long ms = (System.nanoTime() - start) / 1_000_000;
        kv("soTimeoutAfterBind.acceptError", err);
        kv("soTimeoutAfterBind.timedOutPromptly", ms >= 400 && ms < 5000);
        b.close();
    }

    // 10. SO_REUSEADDR set while unbound must reach the listener the bind creates —
    //     that ordering is the ONLY one where the option changes bind behaviour.
    static void reuseAddressBeforeBind() throws Exception {
        ServerSocket b = new ServerSocket();
        b.setReuseAddress(true);
        kv("reuseBeforeBind.readBackUnbound", b.getReuseAddress());
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        kv("reuseBeforeBind.readBackBound", b.getReuseAddress());
        b.close();

        ServerSocket c = new ServerSocket();
        c.setReuseAddress(false);
        kv("reuseOffBeforeBind.readBackUnbound", c.getReuseAddress());
        c.bind(new InetSocketAddress("127.0.0.1", 0));
        kv("reuseOffBeforeBind.readBackBound", c.getReuseAddress());
        c.close();
    }

    // 11. SO_RCVBUF set while unbound must likewise reach the listener.
    static void recvBufferBeforeBind() throws Exception {
        ServerSocket b = new ServerSocket();
        b.setReceiveBufferSize(64 * 1024);
        kv("recvBufBeforeBind.readBackUnboundPositive", b.getReceiveBufferSize() > 0);
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        // The kernel is free to round/double the request, so only assert it grew.
        kv("recvBufBeforeBind.readBackBoundAtLeast32k", b.getReceiveBufferSize() >= 32 * 1024);
        b.close();
    }

    // 12. Binding twice must fail with SocketException("Already bound").
    static void doubleBind() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        int first = b.getLocalPort();
        String err = "none";
        try {
            b.bind(new InetSocketAddress("127.0.0.1", 0));
        } catch (Throwable t) {
            err = ex(t);
        }
        kv("doubleBind.error", err);
        kv("doubleBind.portUnchanged", b.getLocalPort() == first);
        b.close();
    }

    // 13. Binding a closed socket must fail with SocketException("Socket is closed").
    static void bindAfterClose() throws Exception {
        ServerSocket b = new ServerSocket();
        b.close();
        String err = "none";
        try {
            b.bind(new InetSocketAddress("127.0.0.1", 0));
        } catch (Throwable t) {
            err = ex(t);
        }
        kv("bindAfterClose.error", err);
    }

    // 14. accept() on an unbound socket.
    static void acceptOnUnbound() throws Exception {
        ServerSocket b = new ServerSocket();
        String err = "none";
        try {
            b.accept();
        } catch (Throwable t) {
            err = ex(t);
        }
        kv("acceptOnUnbound.error", err);
        b.close();
    }

    // 15. Accessors after close(): HotSpot keeps `bound` true and still reports
    //     the port it had been bound to.
    static void postCloseAccessors() throws Exception {
        ServerSocket b = new ServerSocket();
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        int before = b.getLocalPort();
        b.close();
        kv("postClose.isBound", b.isBound());
        kv("postClose.isClosed", b.isClosed());
        kv("postClose.localPortSame", b.getLocalPort() == before);
        kv("postClose.localSocketAddress.null", b.getLocalSocketAddress() == null);
    }

    // 16. getInetAddress() after a plain bind (Narayana's TxControl reads it).
    static void getInetAddressAfterPlainBind() throws Exception {
        ServerSocket b = new ServerSocket();
        kv("inetAddr.beforeBind.null", b.getInetAddress() == null);
        b.bind(new InetSocketAddress("127.0.0.1", 0));
        kv("inetAddr.afterBind.null", b.getInetAddress() == null);
        kv("inetAddr.afterBind.hostAddress",
                b.getInetAddress() == null ? "<null>" : b.getInetAddress().getHostAddress());
        b.close();
    }
}
