package cratonvm;

import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.SocketException;
import java.net.SocketTimeoutException;

/**
 * The plain (non-channel) {@code java.net.ServerSocket} contract, asserted
 * against the behaviour of the real JDK.
 *
 * Two families, both regressions this fixture exists to hold:
 *
 * <ol>
 * <li><b>SO_TIMEOUT must bound {@code accept()}</b>, whether it was set before
 * or after {@code bind()}. Under real sockets the native {@code Net.accept}
 * ignored the non-blocking mode {@code NioSocketImpl.timedAccept} puts the fd
 * in, so the timeout never fired and {@code accept()} hung forever; under
 * synthetic sockets the timeout was keyed by listener id, which does not exist
 * before {@code bind()}, so the set-then-bind ordering dropped it.</li>
 * <li><b>The lifecycle accessors must survive {@code close()}</b>:
 * {@code isBound()} stays true, {@code getLocalPort()} keeps reporting the
 * port, {@code isClosed()} becomes true. They were answered from a side table
 * that {@code close()} cleared, so they reverted or never changed.</li>
 * </ol>
 *
 * Every assertion here also passes on HotSpot — that is the point.
 */
public final class PlainServerSocketContract {

    /** Long enough not to be flaky on a loaded box, short enough to fail fast. */
    private static final int ACCEPT_TIMEOUT_MILLIS = 700;

    private PlainServerSocketContract() {
    }

    public static void main(String[] args) throws Exception {
        acceptTimesOutWhenSetBeforeBind();
        acceptTimesOutWhenSetAfterBind();
        lifecycleAccessorsSurviveClose();
        unboundAccessors();
        bindNullPicksAnEphemeralPort();
        rebindAndBindAfterCloseAreRefused();
        System.out.println("PLAIN_SERVER_SOCKET_CONTRACT_OK");
    }

    private static void acceptTimesOutWhenSetBeforeBind() throws Exception {
        try (ServerSocket ss = new ServerSocket()) {
            ss.setSoTimeout(ACCEPT_TIMEOUT_MILLIS);
            check(ss.getSoTimeout() == ACCEPT_TIMEOUT_MILLIS,
                    "getSoTimeout on an unbound socket read back " + ss.getSoTimeout());
            ss.bind(new InetSocketAddress("127.0.0.1", 0));
            check(ss.getSoTimeout() == ACCEPT_TIMEOUT_MILLIS,
                    "bind() lost the SO_TIMEOUT set before it: " + ss.getSoTimeout());
            expectAcceptTimeout(ss, "setSoTimeout before bind");
        }
    }

    private static void acceptTimesOutWhenSetAfterBind() throws Exception {
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(new InetSocketAddress("127.0.0.1", 0));
            ss.setSoTimeout(ACCEPT_TIMEOUT_MILLIS);
            expectAcceptTimeout(ss, "setSoTimeout after bind");
        }
    }

    /**
     * The failure mode being guarded is a HANG, so bound the wait: run the
     * accept on a helper thread and fail if it is still parked well past the
     * timeout it was given.
     */
    private static void expectAcceptTimeout(ServerSocket ss, String ordering) throws Exception {
        final String[] outcome = new String[] {"<still blocked>"};
        Thread accept = new Thread(() -> {
            long start = System.nanoTime();
            try {
                ss.accept();
                outcome[0] = "accept() returned a connection nobody made";
            } catch (SocketTimeoutException expected) {
                long millis = (System.nanoTime() - start) / 1_000_000L;
                outcome[0] = millis >= (ACCEPT_TIMEOUT_MILLIS / 2) ? null
                        : "accept() timed out after only " + millis + " ms";
            } catch (Throwable other) {
                outcome[0] = "accept() threw " + other.getClass().getName() + " instead of "
                        + "SocketTimeoutException";
            }
        }, "accept-" + ordering);
        accept.setDaemon(true);
        accept.start();
        accept.join(ACCEPT_TIMEOUT_MILLIS * 10L);
        check(!accept.isAlive(), ordering + ": accept() ignored SO_TIMEOUT and is still blocked");
        check(outcome[0] == null, ordering + ": " + outcome[0]);
    }

    private static void lifecycleAccessorsSurviveClose() throws Exception {
        ServerSocket ss = new ServerSocket();
        ss.bind(new InetSocketAddress("127.0.0.1", 0));
        int port = ss.getLocalPort();
        check(port > 0, "bind() left getLocalPort() at " + port);
        check(ss.isBound(), "isBound() false after a successful bind()");
        check(!ss.isClosed(), "isClosed() true on an open socket");
        check(ss.getLocalSocketAddress() != null, "getLocalSocketAddress() null while bound");
        check(ss.getInetAddress() != null, "getInetAddress() null while bound");

        ss.close();
        check(ss.isClosed(), "isClosed() still false after close()");
        check(ss.isBound(), "isBound() reverted to false after close()");
        check(ss.getLocalPort() == port,
                "getLocalPort() after close() is " + ss.getLocalPort() + ", expected " + port);
        check(ss.getLocalSocketAddress() != null, "getLocalSocketAddress() null after close()");
    }

    private static void unboundAccessors() throws Exception {
        try (ServerSocket ss = new ServerSocket()) {
            check(!ss.isBound(), "a fresh ServerSocket reports isBound()");
            check(!ss.isClosed(), "a fresh ServerSocket reports isClosed()");
            check(ss.getLocalPort() == -1,
                    "getLocalPort() on an unbound socket is " + ss.getLocalPort() + ", expected -1");
            check(ss.getLocalSocketAddress() == null,
                    "getLocalSocketAddress() on an unbound socket is not null");
            check(ss.getInetAddress() == null,
                    "getInetAddress() on an unbound socket is not null");
        }
    }

    private static void bindNullPicksAnEphemeralPort() throws Exception {
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(null);
            check(ss.isBound(), "bind(null) did not bind");
            check(ss.getLocalPort() > 0,
                    "bind(null) left getLocalPort() at " + ss.getLocalPort());
        }
    }

    private static void rebindAndBindAfterCloseAreRefused() throws Exception {
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(new InetSocketAddress("127.0.0.1", 0));
            int port = ss.getLocalPort();
            expectSocketException(() -> ss.bind(new InetSocketAddress("127.0.0.1", 0)),
                    "binding an already-bound ServerSocket");
            check(ss.getLocalPort() == port, "a refused re-bind still moved getLocalPort()");
        }
        ServerSocket closed = new ServerSocket();
        closed.close();
        expectSocketException(() -> closed.bind(new InetSocketAddress("127.0.0.1", 0)),
                "binding a closed ServerSocket");
    }

    private interface Body {
        void run() throws Exception;
    }

    private static void expectSocketException(Body body, String what) throws Exception {
        try {
            body.run();
        } catch (SocketException expected) {
            return;
        }
        throw new AssertionError(what + " should have thrown SocketException");
    }

    private static void check(boolean condition, String message) {
        if (!condition) {
            throw new AssertionError(message);
        }
    }
}
