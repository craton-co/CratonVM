// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.List;

/**
 * Minimal repro for
 * `known-issues/netty/blocking-connect-accept-stalls-near-128-connections-20260905.md`,
 * cut down to answer the two questions that page leaves open.
 *
 * <h2>Question 1 — which call blocks?</h2>
 *
 * The original probe printed progress every 32 connections, which localises the
 * stall to a 32-connection window and no further. This one prints immediately
 * BEFORE each of the three calls, and flushes, so the last line names the exact
 * call that never returned: `connect`, `accept`, or `register`.
 *
 * <h2>Question 2 — does the selector matter?</h2>
 *
 * `register` is skipped entirely when argv[1] is `noreg`. That splits the two
 * surviving hypotheses cleanly:
 *
 * <pre>
 *   stalls with noreg   -> plain blocking connect/accept, selector irrelevant
 *   passes with noreg   -> the selector registration path, which try_clone()s a
 *                          duplicate handle per registration on Windows
 * </pre>
 *
 * Deliberately no traffic, no timing and no echo thread: everything that is not
 * connection establishment has been removed, so a stall here cannot be blamed
 * on readiness reporting.
 *
 * <pre>
 *   java WinConnectStallProbe 256          # with selector registration
 *   java WinConnectStallProbe 256 noreg    # without
 * </pre>
 */
public class WinConnectStallProbe {

    public static void main(String[] args) throws Exception {
        // Split modes, so the client and the server can run on DIFFERENT VMs.
        // Same-process connect+accept cannot say which half is stuck; pointing
        // a CratonVM client at a HotSpot server (and the reverse) can.
        //   server <n>            bind, print PORT=<p>, accept n, hold them
        //   client <port> <n>     connect n times to <port>
        if (args.length > 0 && "server".equals(args[0])) {
            int want = Integer.parseInt(args[1]);
            serverOnly(want);
            return;
        }
        if (args.length > 0 && "client".equals(args[0])) {
            int port = Integer.parseInt(args[1]);
            int want = Integer.parseInt(args[2]);
            clientOnly(port, want);
            return;
        }
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 256;
        boolean register = args.length < 2 || !"noreg".equals(args[1]);

        InetAddress lo = InetAddress.getLoopbackAddress();
        List<SocketChannel> clients = new ArrayList<>();
        List<SocketChannel> served = new ArrayList<>();

        try (ServerSocketChannel server = ServerSocketChannel.open();
                Selector sel = Selector.open()) {
            server.bind(new InetSocketAddress(lo, 0), 4096);
            int port = ((InetSocketAddress) server.getLocalAddress()).getPort();
            System.out.println("n=" + n + " register=" + register + " port=" + port);

            for (int i = 0; i < n; i++) {
                say(i, "connect");
                SocketChannel c = SocketChannel.open(new InetSocketAddress(lo, port));
                clients.add(c);

                say(i, "accept");
                SocketChannel s = server.accept();
                served.add(s);

                if (register) {
                    say(i, "configureBlocking(false)");
                    s.configureBlocking(false);
                    say(i, "register");
                    s.register(sel, SelectionKey.OP_READ);
                }
            }
            System.out.println("OK established=" + clients.size()
                    + " accepted=" + served.size());
        } finally {
            for (SocketChannel c : clients) {
                try {
                    c.close();
                } catch (Exception ignored) {
                    // teardown
                }
            }
            for (SocketChannel s : served) {
                try {
                    s.close();
                } catch (Exception ignored) {
                    // teardown
                }
            }
        }
    }

    /** Accept `n` connections and hold them open, announcing the bound port. */
    static void serverOnly(int n) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        List<SocketChannel> held = new ArrayList<>();
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(new InetSocketAddress(lo, 0), 4096);
            int port = ((InetSocketAddress) server.getLocalAddress()).getPort();
            System.out.println("PORT=" + port);
            System.out.flush();
            for (int i = 0; i < n; i++) {
                say(i, "accept");
                held.add(server.accept());
            }
            System.out.println("SERVER_OK accepted=" + held.size());
        } finally {
            for (SocketChannel s : held) {
                try {
                    s.close();
                } catch (Exception ignored) {
                    // teardown
                }
            }
        }
    }

    /** Connect `n` times to an already-listening port in another process. */
    static void clientOnly(int port, int n) throws Exception {
        InetAddress lo = InetAddress.getLoopbackAddress();
        List<SocketChannel> held = new ArrayList<>();
        try {
            for (int i = 0; i < n; i++) {
                say(i, "connect");
                held.add(SocketChannel.open(new InetSocketAddress(lo, port)));
            }
            System.out.println("CLIENT_OK connected=" + held.size());
        } finally {
            for (SocketChannel c : held) {
                try {
                    c.close();
                } catch (Exception ignored) {
                    // teardown
                }
            }
        }
    }

    /**
     * Announce the call about to be made, and FLUSH.
     *
     * Without the flush the buffered tail is lost when the process is killed on
     * timeout, and the last line you see is whatever happened to fit — which is
     * how a 32-connection window becomes the resolution limit.
     */
    static void say(int i, String what) {
        System.err.println("[" + i + "] " + what);
        System.err.flush();
    }
}
