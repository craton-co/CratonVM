// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.Iterator;

/**
 * SBR wave 5, lane {@code nioreal}: loopback socket-channel cost, the hot path
 * the retirement moves from native carriers to JDK bytecode.
 *
 * <pre>
 *   java SbrW5NiorealPerf [rounds] [mib] [pings]
 * </pre>
 *
 * Four figures per round, each after its own warm-up pass:
 * <ul>
 *   <li>{@code block.tput}: MiB/s, one thread writing {@code mib} MiB through a
 *       64 KiB heap buffer to a blocking peer that reads and discards;</li>
 *   <li>{@code block.rtt}: microseconds per 1-byte ping-pong over two blocking
 *       channels (a helper thread echoes);</li>
 *   <li>{@code sel.tput}: the same transfer read by a non-blocking channel
 *       under a {@link Selector};</li>
 *   <li>{@code sel.rtt}: the ping-pong with the echo side driven by a
 *       selector loop.</li>
 * </ul>
 */
public class SbrW5NiorealPerf {
    static final InetAddress LO = InetAddress.getLoopbackAddress();

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int mib = args.length > 1 ? Integer.parseInt(args[1]) : 64;
        int pings = args.length > 2 ? Integer.parseInt(args[2]) : 5000;
        // Warm-up, not printed.
        blockTput(Math.max(1, mib / 8));
        blockRtt(pings / 5);
        selTput(Math.max(1, mib / 8));
        selRtt(pings / 5);
        for (int r = 0; r < rounds; r++) {
            double bt = blockTput(mib);
            double br = blockRtt(pings);
            double st = selTput(mib);
            double sr = selRtt(pings);
            System.out.printf("round %d block.tput=%.1f MiB/s block.rtt=%.1f us sel.tput=%.1f MiB/s sel.rtt=%.1f us%n",
                    r, bt, br, st, sr);
        }
    }

    static SocketChannel[] pair(ServerSocketChannel s) throws Exception {
        SocketChannel c = SocketChannel.open(s.getLocalAddress());
        SocketChannel a = s.accept();
        return new SocketChannel[] {c, a};
    }

    static void writeAll(SocketChannel c, long total) throws Exception {
        ByteBuffer b = ByteBuffer.allocate(64 * 1024);
        long left = total;
        while (left > 0) {
            b.clear();
            if (left < b.capacity()) b.limit((int) left);
            left -= b.remaining();
            while (b.hasRemaining()) c.write(b);
        }
    }

    static double blockTput(int mib) throws Exception {
        long total = (long) mib << 20;
        try (ServerSocketChannel s = ServerSocketChannel.open()) {
            s.bind(new InetSocketAddress(LO, 0));
            SocketChannel[] p = pair(s);
            try (SocketChannel c = p[0]; SocketChannel a = p[1]) {
                Thread w = new Thread(() -> {
                    try {
                        writeAll(c, total);
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                });
                long t0 = System.nanoTime();
                w.start();
                ByteBuffer in = ByteBuffer.allocate(64 * 1024);
                long got = 0;
                while (got < total) {
                    in.clear();
                    int n = a.read(in);
                    if (n < 0) break;
                    got += n;
                }
                w.join();
                double s2 = (System.nanoTime() - t0) / 1e9;
                if (got != total) throw new IllegalStateException("short " + got);
                return mib / s2;
            }
        }
    }

    static double blockRtt(int pings) throws Exception {
        try (ServerSocketChannel s = ServerSocketChannel.open()) {
            s.bind(new InetSocketAddress(LO, 0));
            SocketChannel[] p = pair(s);
            try (SocketChannel c = p[0]; SocketChannel a = p[1]) {
                Thread echo = new Thread(() -> {
                    ByteBuffer b = ByteBuffer.allocate(1);
                    try {
                        for (int i = 0; i < pings; i++) {
                            b.clear();
                            while (b.hasRemaining()) if (a.read(b) < 0) return;
                            b.flip();
                            while (b.hasRemaining()) a.write(b);
                        }
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                });
                echo.start();
                ByteBuffer b = ByteBuffer.allocate(1);
                long t0 = System.nanoTime();
                for (int i = 0; i < pings; i++) {
                    b.clear();
                    b.put((byte) i).flip();
                    while (b.hasRemaining()) c.write(b);
                    b.clear();
                    while (b.hasRemaining()) c.read(b);
                }
                double us = (System.nanoTime() - t0) / 1e3 / pings;
                echo.join();
                return us;
            }
        }
    }

    static double selTput(int mib) throws Exception {
        long total = (long) mib << 20;
        try (ServerSocketChannel s = ServerSocketChannel.open(); Selector sel = Selector.open()) {
            s.bind(new InetSocketAddress(LO, 0));
            SocketChannel[] p = pair(s);
            try (SocketChannel c = p[0]; SocketChannel a = p[1]) {
                a.configureBlocking(false);
                a.register(sel, SelectionKey.OP_READ);
                Thread w = new Thread(() -> {
                    try {
                        writeAll(c, total);
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                });
                long t0 = System.nanoTime();
                w.start();
                ByteBuffer in = ByteBuffer.allocate(64 * 1024);
                long got = 0;
                while (got < total) {
                    sel.select(1000);
                    Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                    while (it.hasNext()) {
                        it.next();
                        it.remove();
                        int n;
                        do {
                            in.clear();
                            n = a.read(in);
                            if (n > 0) got += n;
                        } while (n > 0);
                    }
                }
                w.join();
                double s2 = (System.nanoTime() - t0) / 1e9;
                return mib / s2;
            }
        }
    }

    static double selRtt(int pings) throws Exception {
        try (ServerSocketChannel s = ServerSocketChannel.open()) {
            s.bind(new InetSocketAddress(LO, 0));
            SocketChannel[] p = pair(s);
            try (SocketChannel c = p[0]; SocketChannel a = p[1]) {
                Thread echo = new Thread(() -> {
                    try (Selector sel = Selector.open()) {
                        a.configureBlocking(false);
                        a.register(sel, SelectionKey.OP_READ);
                        ByteBuffer b = ByteBuffer.allocate(16);
                        int seen = 0;
                        while (seen < pings) {
                            sel.select(1000);
                            Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                            while (it.hasNext()) {
                                it.next();
                                it.remove();
                                b.clear();
                                int n = a.read(b);
                                if (n < 0) return;
                                seen += n;
                                b.flip();
                                while (b.hasRemaining()) a.write(b);
                            }
                        }
                    } catch (Exception e) {
                        throw new RuntimeException(e);
                    }
                });
                echo.start();
                ByteBuffer b = ByteBuffer.allocate(1);
                long t0 = System.nanoTime();
                for (int i = 0; i < pings; i++) {
                    b.clear();
                    b.put((byte) i).flip();
                    while (b.hasRemaining()) c.write(b);
                    b.clear();
                    while (b.hasRemaining()) c.read(b);
                }
                double us = (System.nanoTime() - t0) / 1e3 / pings;
                echo.join();
                return us;
            }
        }
    }
}
