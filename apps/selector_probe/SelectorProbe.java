// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Wave 3, Task C probe: the JDK NIO selector loop.
//
// Driven by `vm/tests/wave3_c_selector.rs`, which requires:
//
//   server.port=NNN        -- non-blocking bind on an OS-chosen port worked
//   server.accepted=true   -- select() reported OP_ACCEPT and accept() returned
//                             a non-null SocketChannel
//   server.recv=42         -- the accepted socket read the byte the client wrote
//   OK                     -- the whole happy path completed without throwing
//
// Shape and why each line can fail:
//
//   * The port printed is read back from `ServerSocketChannel.getLocalAddress()`
//     and is the ONLY address the client connects to. A VM whose local-address
//     accessor lies (the historical "wildcard reported as loopback" family)
//     sends the client somewhere nothing is listening, no OP_ACCEPT ever fires,
//     and the probe dies on the bounded wait below.
//   * `server.accepted=` prints the result of `accepted != null`, not a
//     literal; a `select()` that never reports OP_ACCEPT, or an `accept()` that
//     returns null after it does, prints `false` or throws.
//   * `server.recv=` prints the byte actually read out of the ByteBuffer.
//
// Everything binds on the loopback address with an ephemeral port, so the probe
// needs no external host and cannot collide with a fixed port. Every blocking
// wait is deadline-bounded (DEADLINE_MS) and every timeout throws a named
// AssertionError, so a defect shows up as a non-zero exit with a diagnostic
// rather than as a hang that the driving test can only report as a timeout.

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.Iterator;
import java.util.Set;
import java.util.concurrent.atomic.AtomicReference;

public class SelectorProbe {

    /** Upper bound on each blocking phase. Generous, but never unbounded. */
    private static final long DEADLINE_MS = 15_000L;

    /** The byte the client writes and the server must read back. */
    private static final int PAYLOAD = 42;

    public static void main(String[] args) throws Exception {
        final AtomicReference<Throwable> clientError = new AtomicReference<>();
        Thread client = null;

        try (Selector selector = Selector.open();
                ServerSocketChannel server = ServerSocketChannel.open()) {

            server.configureBlocking(false);
            server.bind(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 1);
            server.register(selector, SelectionKey.OP_ACCEPT);

            java.net.SocketAddress local = server.getLocalAddress();
            if (!(local instanceof InetSocketAddress)) {
                throw new AssertionError("getLocalAddress() returned " + local
                        + ", expected an InetSocketAddress");
            }
            final InetSocketAddress bound = (InetSocketAddress) local;
            int port = bound.getPort();
            if (port <= 0 || port > 65535) {
                throw new AssertionError("bind(port 0) left an unusable local port: " + port);
            }
            System.out.println("server.port=" + port);

            // The client only ever knows what the accessor above reported.
            final InetSocketAddress target =
                    new InetSocketAddress(InetAddress.getLoopbackAddress(), port);
            client = new Thread(new Runnable() {
                @Override
                public void run() {
                    try (SocketChannel ch = SocketChannel.open()) {
                        ch.socket().connect(target, (int) DEADLINE_MS);
                        ByteBuffer out = ByteBuffer.allocate(1);
                        out.put((byte) PAYLOAD).flip();
                        while (out.hasRemaining()) {
                            ch.write(out);
                        }
                    } catch (Throwable t) {
                        clientError.set(t);
                    }
                }
            }, "selector-probe-client");
            client.setDaemon(true);
            client.start();

            // --- phase 1: wait for OP_ACCEPT -------------------------------
            SocketChannel accepted = null;
            long deadline = System.nanoTime() + DEADLINE_MS * 1_000_000L;
            while (accepted == null && System.nanoTime() < deadline) {
                failIfClientDied(clientError);
                selector.select(500);
                Set<SelectionKey> keys = selector.selectedKeys();
                for (Iterator<SelectionKey> it = keys.iterator(); it.hasNext();) {
                    SelectionKey key = it.next();
                    it.remove();
                    if (key.isValid() && key.isAcceptable()) {
                        accepted = ((ServerSocketChannel) key.channel()).accept();
                    }
                }
            }
            failIfClientDied(clientError);
            System.out.println("server.accepted=" + (accepted != null));
            if (accepted == null) {
                throw new AssertionError("no OP_ACCEPT within " + DEADLINE_MS
                        + "ms on 127.0.0.1:" + port);
            }

            // --- phase 2: read the byte ------------------------------------
            try (SocketChannel conn = accepted) {
                conn.configureBlocking(false);
                conn.register(selector, SelectionKey.OP_READ);
                ByteBuffer in = ByteBuffer.allocate(1);
                deadline = System.nanoTime() + DEADLINE_MS * 1_000_000L;
                while (in.position() == 0 && System.nanoTime() < deadline) {
                    failIfClientDied(clientError);
                    selector.select(500);
                    Set<SelectionKey> keys = selector.selectedKeys();
                    for (Iterator<SelectionKey> it = keys.iterator(); it.hasNext();) {
                        SelectionKey key = it.next();
                        it.remove();
                        if (key.isValid() && key.isReadable()) {
                            int n = conn.read(in);
                            if (n < 0) {
                                throw new AssertionError(
                                        "peer closed before sending the payload byte");
                            }
                        }
                    }
                }
                if (in.position() == 0) {
                    throw new AssertionError("no readable byte within " + DEADLINE_MS + "ms");
                }
                System.out.println("server.recv=" + (in.get(0) & 0xFF));
            }

            client.join(DEADLINE_MS);
            failIfClientDied(clientError);
            if (client.isAlive()) {
                throw new AssertionError("client thread still alive after " + DEADLINE_MS + "ms");
            }
        } finally {
            if (client != null) {
                client.interrupt();
            }
        }

        System.out.println("OK");
    }

    private static void failIfClientDied(AtomicReference<Throwable> box) throws IOException {
        Throwable t = box.get();
        if (t != null) {
            throw new AssertionError("client thread failed: " + t, t);
        }
    }
}
