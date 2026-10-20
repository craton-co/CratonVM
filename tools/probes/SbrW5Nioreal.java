// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.MulticastSocket;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketAddress;
import java.net.SocketTimeoutException;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousCloseException;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.ClosedByInterruptException;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.DatagramChannel;
import java.nio.channels.MembershipKey;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.List;
import java.util.TreeSet;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * SBR wave 5, lane {@code nioreal}: the socket / channel / selector / datagram
 * family retired over the real {@code sun.nio.ch} native layer
 * ({@code RETIRED_SHADOW_SBR_NIOREAL_TRIPLES}).
 *
 * <p>Wave 2's {@code SbrW2Nioch} rows (minus the async FILE channel, the FS
 * lane's) plus the shapes a real channel/selector must get right that a
 * native carrier could fake: a multi-client selector echo, non-blocking and
 * connected datagram channels, multicast join/leave, and the blocked-thread
 * rows (interrupt, asynchronous close, selector wakeup/interrupt).
 *
 * <p>Each row returns deterministic observations only (no port, no identity
 * hash, no timing but a coarse "fast" flag). It is evaluated ONCE and printed,
 * then re-evaluated in a hot loop and compared with the first value; {@code
 * drift} counts mismatches. Diff the whole output against HotSpot 25.
 *
 * <pre>
 *   java SbrW5Nioreal [iters]              (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW5Nioreal [iters]
 * </pre>
 */
public class SbrW5Nioreal {
    interface S {
        String get() throws Exception;
    }

    static final List<String> LABELS = new ArrayList<>();
    static final List<S> ROWS = new ArrayList<>();

    static void row(String label, S s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static final String TIMEOUT = "!TIMEOUT (row did not finish in 20 s)";

    /**
     * Evaluate a row on a daemon thread with a 20 s deadline, so one row that
     * hangs on one VM (a timed read that never times out, say) is reported as
     * such instead of killing the whole probe.
     */
    static String safe(S s) {
        final String[] out = {TIMEOUT};
        Thread t = new Thread(() -> {
            String v;
            try {
                v = s.get();
            } catch (Throwable e) {
                v = "!" + e.getClass().getName() + ": " + e.getMessage();
            }
            out[0] = v;
        });
        t.setDaemon(true);
        t.start();
        try {
            t.join(20_000);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
        return t.isAlive() ? TIMEOUT : out[0];
    }

    static String ex(S s) {
        try {
            return "ok:" + shape(s.get());
        } catch (Throwable t) {
            return "ex:" + t.getClass().getName();
        }
    }

    static final InetAddress LO = InetAddress.getLoopbackAddress();

    static ByteBuffer bb(String s) {
        return ByteBuffer.wrap(s.getBytes(StandardCharsets.US_ASCII));
    }

    static String str(ByteBuffer b) {
        b.flip();
        byte[] a = new byte[b.remaining()];
        b.get(a);
        return new String(a, StandardCharsets.US_ASCII);
    }

    static int readFully(SocketChannel ch, ByteBuffer dst) throws Exception {
        int total = 0;
        while (dst.hasRemaining()) {
            int n = ch.read(dst);
            if (n < 0) return total == 0 ? -1 : total;
            total += n;
        }
        return total;
    }

    /** Strip the variable parts (ports, addresses) out of a toString. */
    static String shape(Object o) {
        String s = String.valueOf(o).replaceAll("@[0-9a-f]+", "@H");
        return s.replaceAll("[0-9]+", "N").replaceAll("localhost|127\\.N\\.N\\.N|/N\\.N\\.N\\.N", "A");
    }

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String exName(Throwable t) {
        if (t instanceof ClosedByInterruptException) return "ClosedByInterruptException";
        if (t instanceof AsynchronousCloseException) return "AsynchronousCloseException";
        if (t instanceof ClosedChannelException) return "ClosedChannelException";
        return t.getClass().getName();
    }

    /** Run {@code body} on a helper thread; {@code act} runs once it is parked. */
    static String parked(ThrowingRunnable body, ThrowingRunnable act, boolean interrupt) throws Exception {
        final String[] out = {"never"};
        final boolean[] flag = {false};
        final CountDownLatch entered = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            entered.countDown();
            try {
                body.run();
                out[0] = "no-exception";
            } catch (Throwable e) {
                out[0] = exName(e);
            }
            flag[0] = Thread.currentThread().isInterrupted();
        });
        t.setDaemon(true);
        t.start();
        entered.await(5, TimeUnit.SECONDS);
        Thread.sleep(40);
        if (interrupt) t.interrupt();
        if (act != null) act.run();
        t.join(10_000);
        return out[0] + " woke=" + !t.isAlive() + " flag=" + flag[0];
    }

    interface ThrowingRunnable {
        void run() throws Exception;
    }

    public static void main(String[] args) throws Exception {
        // ---- SocketChannel lifecycle, no peer -------------------------------------------
        row("sc.fresh", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                return cls(c) + " open=" + c.isOpen() + " blocking=" + c.isBlocking()
                        + " connected=" + c.isConnected() + " pending=" + c.isConnectionPending()
                        + " registered=" + c.isRegistered() + " local=" + c.getLocalAddress()
                        + " remote=" + c.getRemoteAddress() + " str=" + shape(c)
                        + " provider=" + cls(c.provider());
            }
        });
        row("sc.configureBlocking", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                Object r = c.configureBlocking(false);
                return "same=" + (r == c) + " blocking=" + c.isBlocking()
                        + " back=" + c.configureBlocking(true).isBlocking();
            }
        });
        row("sc.close", () -> {
            SocketChannel c = SocketChannel.open();
            c.close();
            c.close();
            return "open=" + c.isOpen() + " connect=" + ex(() -> String.valueOf(c.connect(new InetSocketAddress(LO, 1))))
                    + " read=" + ex(() -> String.valueOf(c.read(ByteBuffer.allocate(1))))
                    + " write=" + ex(() -> String.valueOf(c.write(bb("x"))))
                    + " cfg=" + ex(() -> String.valueOf(c.configureBlocking(false)))
                    + " local=" + ex(() -> String.valueOf(c.getLocalAddress()))
                    + " str=" + shape(c);
        });
        row("sc.unconnected", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                return "read=" + ex(() -> String.valueOf(c.read(ByteBuffer.allocate(1))))
                        + " write=" + ex(() -> String.valueOf(c.write(bb("x"))))
                        + " finish=" + ex(() -> String.valueOf(c.finishConnect()))
                        + " shutIn=" + ex(() -> String.valueOf(c.shutdownInput()));
            }
        });
        row("sc.refused", () -> {
            int port;
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                port = ((InetSocketAddress) s.getLocalAddress()).getPort();
            }
            SocketChannel c = SocketChannel.open();
            String r = ex(() -> String.valueOf(c.connect(new InetSocketAddress(LO, port))));
            return r + " openAfter=" + c.isOpen();
        });
        row("sc.options", () -> {
            try (SocketChannel c = SocketChannel.open()) {
                c.setOption(StandardSocketOptions.TCP_NODELAY, true);
                c.setOption(StandardSocketOptions.SO_KEEPALIVE, true);
                c.setOption(StandardSocketOptions.SO_REUSEADDR, true);
                return "nodelay=" + c.getOption(StandardSocketOptions.TCP_NODELAY)
                        + " keepalive=" + c.getOption(StandardSocketOptions.SO_KEEPALIVE)
                        + " reuse=" + c.getOption(StandardSocketOptions.SO_REUSEADDR)
                        + " n=" + c.supportedOptions().contains(StandardSocketOptions.SO_LINGER);
            }
        });

        // ---- ServerSocketChannel -------------------------------------------------------
        row("ssc.bind", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                String before = "local0=" + s.getLocalAddress() + " open=" + s.isOpen() + " blk=" + s.isBlocking();
                Object r = s.bind(new InetSocketAddress(LO, 0));
                InetSocketAddress a = (InetSocketAddress) s.getLocalAddress();
                s.configureBlocking(false);
                return cls(s) + " " + before + " same=" + (r == s) + " addr=" + a.getAddress()
                        + " port>0=" + (a.getPort() > 0) + " nbAccept=" + s.accept()
                        + " rebind=" + ex(() -> String.valueOf(s.bind(new InetSocketAddress(LO, 0))))
                        + " str=" + shape(s) + " sock=" + cls(s.socket())
                        + " sockBound=" + s.socket().isBound();
            }
        });
        row("ssc.close", () -> {
            ServerSocketChannel s = ServerSocketChannel.open();
            s.bind(new InetSocketAddress(LO, 0));
            s.close();
            return "open=" + s.isOpen() + " accept=" + ex(() -> String.valueOf(s.accept()))
                    + " local=" + ex(() -> String.valueOf(s.getLocalAddress()))
                    + " str=" + shape(s);
        });

        // ---- blocking loopback round trip ----------------------------------------------
        row("tcp.roundtrip", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                SocketAddress sa = s.getLocalAddress();
                try (SocketChannel c = SocketChannel.open(sa); SocketChannel a = s.accept()) {
                    int w = c.write(bb("hello, nio"));
                    ByteBuffer in = ByteBuffer.allocate(10);
                    int r = readFully(a, in);
                    String got = str(in);
                    a.write(bb("pong"));
                    ByteBuffer back = ByteBuffer.allocate(4);
                    readFully(c, back);
                    c.shutdownOutput();
                    int eof = a.read(ByteBuffer.allocate(4));
                    return "w=" + w + " r=" + r + " got=" + got + " back=" + str(back) + " eof=" + eof
                            + " conn=" + c.isConnected() + "/" + a.isConnected()
                            + " remoteEq=" + sa.equals(c.getRemoteAddress())
                            + " acceptedBlk=" + a.isBlocking() + " cls=" + cls(a)
                            + " peerPortEq=" + (((InetSocketAddress) a.getRemoteAddress()).getPort()
                                    == ((InetSocketAddress) c.getLocalAddress()).getPort());
                }
            }
        });
        row("tcp.gather", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    long w = c.write(new ByteBuffer[] {bb("ab"), bb("cde"), bb("f")});
                    ByteBuffer x = ByteBuffer.allocate(3), y = ByteBuffer.allocate(3);
                    long r = 0;
                    while (y.hasRemaining()) r += a.read(new ByteBuffer[] {x, y});
                    return "w=" + w + " r=" + r + " x=" + str(x) + " y=" + str(y);
                }
            }
        });
        row("tcp.direct", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    ByteBuffer out = ByteBuffer.allocateDirect(64 * 1024);
                    for (int i = 0; out.hasRemaining(); i++) out.put((byte) i);
                    out.flip();
                    Thread w = new Thread(() -> {
                        try {
                            while (out.hasRemaining()) c.write(out);
                        } catch (Exception e) {
                            // reported by the byte count below
                        }
                    });
                    w.start();
                    ByteBuffer in = ByteBuffer.allocateDirect(64 * 1024);
                    readFully(a, in);
                    w.join();
                    in.flip();
                    long sum = 0;
                    boolean ok = true;
                    for (int i = 0; in.hasRemaining(); i++) {
                        byte b = in.get();
                        sum += b & 0xff;
                        if (b != (byte) i) ok = false;
                    }
                    return "sum=" + sum + " inOrder=" + ok;
                }
            }
        });
        row("tcp.adaptor", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    Socket sk = c.socket();
                    sk.getOutputStream().write("adapt".getBytes(StandardCharsets.US_ASCII));
                    ByteBuffer in = ByteBuffer.allocate(5);
                    readFully(a, in);
                    Socket ask = a.socket();
                    ask.setSoTimeout(30);
                    String to = ex(() -> String.valueOf(ask.getInputStream().read()));
                    return cls(sk) + " connected=" + sk.isConnected() + " bound=" + sk.isBound()
                            + " closed=" + sk.isClosed() + " got=" + str(in)
                            + " chanSame=" + (sk.getChannel() == c) + " timedRead=" + to;
                }
            }
        });
        row("tcp.closePeer", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                SocketChannel c = SocketChannel.open(s.getLocalAddress());
                SocketChannel a = s.accept();
                c.close();
                int eof = a.read(ByteBuffer.allocate(4));
                a.close();
                return "eof=" + eof + " cOpen=" + c.isOpen() + " aOpen=" + a.isOpen()
                        + " cConn=" + c.isConnected();
            }
        });

        // ---- selector ------------------------------------------------------------------
        row("sel.fresh", () -> {
            Selector sel = Selector.open();
            String r = cls(sel) + " open=" + sel.isOpen() + " keys=" + sel.keys().size()
                    + " selected=" + sel.selectedKeys().size() + " now=" + sel.selectNow()
                    + " provider=" + cls(sel.provider());
            sel.close();
            sel.close();
            return r + " closedOpen=" + sel.isOpen() + " keysAfter=" + ex(() -> String.valueOf(sel.keys()))
                    + " selAfter=" + ex(() -> String.valueOf(sel.selectNow()));
        });
        row("sel.accept.read", () -> {
            try (Selector sel = Selector.open(); ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                s.configureBlocking(false);
                SelectionKey sk = s.register(sel, SelectionKey.OP_ACCEPT, "srv");
                StringBuilder b = new StringBuilder();
                b.append(cls(sk)).append(" valid=").append(sk.isValid()).append(" ops=").append(sk.interestOps())
                        .append(" att=").append(sk.attachment()).append(" regd=").append(s.isRegistered())
                        .append(" keyFor=").append(s.keyFor(sel) == sk).append(" chan=").append(sk.channel() == s)
                        .append(" sel=").append(sk.selector() == sel);
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress())) {
                    int n = sel.select(5000);
                    b.append(" n=").append(n).append(" acc=").append(sk.isAcceptable())
                            .append(" ready=").append(sk.readyOps()).append(" inSel=").append(sel.selectedKeys().contains(sk));
                    sel.selectedKeys().clear();
                    SocketChannel a = s.accept();
                    a.configureBlocking(false);
                    SelectionKey rk = a.register(sel, SelectionKey.OP_READ);
                    b.append(" keys=").append(sel.keys().size()).append(" none=").append(sel.selectNow());
                    c.write(bb("sel!"));
                    int m = sel.select(5000);
                    ByteBuffer in = ByteBuffer.allocate(8);
                    int r = a.read(in);
                    b.append(" m=").append(m).append(" readable=").append(rk.isReadable())
                            .append(" r=").append(r).append(" got=").append(str(in))
                            .append(" again=").append(a.read(ByteBuffer.allocate(8)));
                    rk.interestOps(SelectionKey.OP_READ | SelectionKey.OP_WRITE);
                    b.append(" ops2=").append(rk.interestOps());
                    rk.cancel();
                    b.append(" validAfter=").append(rk.isValid())
                            .append(" opsAfter=").append(ex(() -> String.valueOf(rk.interestOps())));
                    sel.selectNow();
                    b.append(" keysAfter=").append(sel.keys().size()).append(" aRegd=").append(a.isRegistered());
                    a.close();
                }
                return b.toString();
            }
        });
        row("sel.connect", () -> {
            try (Selector sel = Selector.open(); ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open()) {
                    c.configureBlocking(false);
                    boolean now = c.connect(s.getLocalAddress());
                    SelectionKey k = c.register(sel, SelectionKey.OP_CONNECT);
                    boolean done = now;
                    for (int i = 0; i < 50 && !done; i++) {
                        sel.select(200);
                        if (k.isConnectable()) done = c.finishConnect();
                    }
                    try (SocketChannel a = s.accept()) {
                        return "done=" + done + " conn=" + c.isConnected() + " pending=" + c.isConnectionPending()
                                + " aNull=" + (a == null) + " blocking=" + ex(() -> String.valueOf(c.configureBlocking(true)));
                    }
                }
            }
        });
        row("sel.echo3", () -> {
            try (Selector sel = Selector.open(); ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                s.configureBlocking(false);
                s.register(sel, SelectionKey.OP_ACCEPT);
                SocketChannel[] cs = new SocketChannel[3];
                for (int i = 0; i < 3; i++) {
                    cs[i] = SocketChannel.open(s.getLocalAddress());
                    cs[i].write(bb("c" + i));
                }
                int echoed = 0, accepted = 0, rounds = 0;
                while (echoed < 3 && rounds++ < 100) {
                    sel.select(200);
                    Iterator<SelectionKey> it = sel.selectedKeys().iterator();
                    while (it.hasNext()) {
                        SelectionKey k = it.next();
                        it.remove();
                        if (k.isAcceptable()) {
                            SocketChannel a = s.accept();
                            if (a != null) {
                                a.configureBlocking(false);
                                a.register(sel, SelectionKey.OP_READ, ByteBuffer.allocate(2));
                                accepted++;
                            }
                        } else if (k.isReadable()) {
                            SocketChannel a = (SocketChannel) k.channel();
                            ByteBuffer buf = (ByteBuffer) k.attachment();
                            a.read(buf);
                            if (!buf.hasRemaining()) {
                                buf.flip();
                                a.write(buf);
                                echoed++;
                            }
                        }
                    }
                }
                TreeSet<String> back = new TreeSet<>();
                for (SocketChannel c : cs) {
                    ByteBuffer in = ByteBuffer.allocate(2);
                    readFully(c, in);
                    back.add(str(in));
                    c.close();
                }
                for (SelectionKey k : sel.keys()) if (k.channel() != s) k.channel().close();
                return "accepted=" + accepted + " echoed=" + echoed + " back=" + back;
            }
        });
        row("sel.wakeup", () -> {
            try (Selector sel = Selector.open()) {
                sel.wakeup();
                long t0 = System.nanoTime();
                int n = sel.select(5000);
                long ms = (System.nanoTime() - t0) / 1_000_000;
                return "n=" + n + " fast=" + (ms < 2500);
            }
        });
        row("sel.blockedWakeup", () -> {
            try (Selector sel = Selector.open()) {
                return parked(() -> sel.select(), () -> sel.wakeup(), false) + " open=" + sel.isOpen();
            }
        });
        row("sel.blockedInterrupt", () -> {
            try (Selector sel = Selector.open()) {
                return parked(() -> sel.select(), null, true) + " open=" + sel.isOpen();
            }
        });
        row("sel.closeChannelCancels", () -> {
            try (Selector sel = Selector.open()) {
                SocketChannel c = SocketChannel.open();
                c.configureBlocking(false);
                SelectionKey k = c.register(sel, SelectionKey.OP_READ);
                c.close();
                sel.selectNow();
                return "valid=" + k.isValid() + " keys=" + sel.keys().size() + " open=" + c.isOpen();
            }
        });
        row("sel.blockingRegister", () -> {
            try (Selector sel = Selector.open(); SocketChannel c = SocketChannel.open()) {
                return ex(() -> String.valueOf(c.register(sel, SelectionKey.OP_READ)));
            }
        });

        // ---- blocked threads: interrupt, asynchronous close -------------------------------
        row("intr.read", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    Thread.currentThread().interrupt();
                    String r = ex(() -> String.valueOf(c.read(ByteBuffer.allocate(4))));
                    boolean flag = Thread.interrupted();
                    return r + " flag=" + flag + " open=" + c.isOpen();
                }
            }
        });
        row("intr.blockedRead", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    return parked(() -> c.read(ByteBuffer.allocate(4)), null, true) + " open=" + c.isOpen();
                }
            }
        });
        row("close.blockedRead", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    return parked(() -> c.read(ByteBuffer.allocate(4)), () -> c.close(), false);
                }
            }
        });
        row("close.blockedAccept", () -> {
            ServerSocketChannel s = ServerSocketChannel.open();
            s.bind(new InetSocketAddress(LO, 0));
            return parked(() -> s.accept(), () -> s.close(), false);
        });

        // ---- java.net.Socket / ServerSocket (NioSocketImpl) -----------------------------
        row("sock.roundtrip", () -> {
            try (ServerSocket ss = new ServerSocket(0, 50, LO);
                    Socket c = new Socket(LO, ss.getLocalPort());
                    Socket a = ss.accept()) {
                c.getOutputStream().write(new byte[] {1, 2, 3});
                byte[] buf = new byte[3];
                int n = 0;
                while (n < 3) n += a.getInputStream().read(buf, n, 3 - n);
                c.shutdownOutput();
                int eof = a.getInputStream().read();
                return "n=" + n + " b=" + buf[0] + buf[1] + buf[2] + " eof=" + eof + " chan=" + c.getChannel()
                        + " bound=" + ss.isBound() + " conn=" + c.isConnected();
            }
        });
        row("sock.timeout", () -> {
            try (ServerSocket ss = new ServerSocket(0, 50, LO)) {
                ss.setSoTimeout(50);
                return "to=" + ss.getSoTimeout() + " accept=" + ex(() -> String.valueOf(ss.accept()));
            }
        });

        // ---- DatagramSocket / DatagramChannel / MulticastSocket --------------------------
        row("dgs.basic", () -> {
            try (DatagramSocket d = new DatagramSocket(0, LO)) {
                return "bound=" + d.isBound() + " port>0=" + (d.getLocalPort() > 0) + " conn=" + d.isConnected()
                        + " port=" + d.getPort() + " to=" + d.getSoTimeout() + " closed=" + d.isClosed()
                        + " chan=" + d.getChannel() + " addr=" + d.getLocalAddress();
            }
        });
        row("dgs.unbound", () -> {
            try (DatagramSocket d = new DatagramSocket(null)) {
                String r = "bound=" + d.isBound() + " port=" + d.getLocalPort();
                d.bind(new InetSocketAddress(LO, 0));
                return r + " after=" + d.isBound();
            }
        });
        row("dgs.default", () -> {
            DatagramSocket d = new DatagramSocket();
            String r = "bound=" + d.isBound() + " port>0=" + (d.getLocalPort() > 0) + " any=" + d.getLocalAddress().isAnyLocalAddress();
            d.close();
            d.close();
            return r + " closed=" + d.isClosed() + " port=" + d.getLocalPort()
                    + " send=" + ex(() -> { d.send(new DatagramPacket(new byte[1], 1, LO, 9)); return ""; });
        });
        row("dgs.sendReceive", () -> {
            try (DatagramSocket a = new DatagramSocket(0, LO); DatagramSocket b = new DatagramSocket(0, LO)) {
                a.setSoTimeout(5000);
                b.send(new DatagramPacket("dgram".getBytes(StandardCharsets.US_ASCII), 5, LO, a.getLocalPort()));
                DatagramPacket p = new DatagramPacket(new byte[16], 16);
                a.receive(p);
                return "len=" + p.getLength() + " data=" + new String(p.getData(), 0, p.getLength(), StandardCharsets.US_ASCII)
                        + " fromPortEq=" + (p.getPort() == b.getLocalPort()) + " addr=" + p.getAddress();
            }
        });
        row("dgs.connect", () -> {
            try (DatagramSocket a = new DatagramSocket(0, LO); DatagramSocket b = new DatagramSocket(0, LO)) {
                a.connect(LO, b.getLocalPort());
                String r = "conn=" + a.isConnected() + " portEq=" + (a.getPort() == b.getLocalPort())
                        + " inet=" + a.getInetAddress();
                a.send(new DatagramPacket(new byte[] {7}, 1));
                b.setSoTimeout(5000);
                DatagramPacket p = new DatagramPacket(new byte[4], 4);
                b.receive(p);
                a.disconnect();
                return r + " got=" + p.getData()[0] + " after=" + a.isConnected() + " port=" + a.getPort();
            }
        });
        row("dgs.timeout", () -> {
            try (DatagramSocket a = new DatagramSocket(0, LO)) {
                a.setSoTimeout(30);
                String r = "to=" + a.getSoTimeout();
                try {
                    a.receive(new DatagramPacket(new byte[4], 4));
                    return r + " received?";
                } catch (SocketTimeoutException e) {
                    return r + " timeout " + cls(e);
                }
            }
        });
        row("dc.basic", () -> {
            try (DatagramChannel a = DatagramChannel.open(); DatagramChannel b = DatagramChannel.open()) {
                a.bind(new InetSocketAddress(LO, 0));
                b.bind(new InetSocketAddress(LO, 0));
                int n = b.send(bb("chan"), a.getLocalAddress());
                ByteBuffer in = ByteBuffer.allocate(8);
                SocketAddress from = a.receive(in);
                DatagramSocket ad = a.socket();
                ad.setSoTimeout(40);
                return cls(a) + " n=" + n + " got=" + str(in) + " fromEq=" + from.equals(b.getLocalAddress())
                        + " blk=" + a.isBlocking() + " conn=" + a.isConnected() + " adaptor=" + cls(ad)
                        + " adTo=" + ad.getSoTimeout() + " adBound=" + ad.isBound();
            }
        });
        row("dc.nonblocking", () -> {
            try (Selector sel = Selector.open(); DatagramChannel a = DatagramChannel.open();
                    DatagramChannel b = DatagramChannel.open()) {
                a.bind(new InetSocketAddress(LO, 0));
                a.configureBlocking(false);
                SocketAddress none = a.receive(ByteBuffer.allocate(4));
                SelectionKey k = a.register(sel, SelectionKey.OP_READ);
                int idle = sel.selectNow();
                b.send(bb("nb"), a.getLocalAddress());
                int n = sel.select(5000);
                ByteBuffer in = ByteBuffer.allocate(4);
                SocketAddress from = a.receive(in);
                return "none=" + none + " idle=" + idle + " n=" + n + " readable=" + k.isReadable()
                        + " got=" + str(in) + " from=" + (from != null);
            }
        });
        row("dc.connected", () -> {
            try (DatagramChannel a = DatagramChannel.open(); DatagramChannel b = DatagramChannel.open()) {
                a.bind(new InetSocketAddress(LO, 0));
                b.bind(new InetSocketAddress(LO, 0));
                a.connect(b.getLocalAddress());
                b.connect(a.getLocalAddress());
                int w = a.write(bb("xyz"));
                ByteBuffer in = ByteBuffer.allocate(8);
                int r = b.read(in);
                long wv = b.write(new ByteBuffer[] {bb("p"), bb("q")});
                ByteBuffer x = ByteBuffer.allocate(1), y = ByteBuffer.allocate(4);
                long rv = a.read(new ByteBuffer[] {x, y});
                String s = "w=" + w + " r=" + r + " got=" + str(in) + " wv=" + wv + " rv=" + rv
                        + " x=" + str(x) + " y=" + str(y) + " conn=" + a.isConnected()
                        + " remoteEq=" + b.getLocalAddress().equals(a.getRemoteAddress());
                a.disconnect();
                return s + " after=" + a.isConnected() + " remote=" + a.getRemoteAddress()
                        + " stillBound=" + (a.getLocalAddress() != null);
            }
        });
        row("dc.close", () -> {
            DatagramChannel a = DatagramChannel.open();
            a.close();
            return "open=" + a.isOpen() + " send=" + ex(() -> String.valueOf(a.send(bb("x"), new InetSocketAddress(LO, 9))))
                    + " sockClosed=" + a.socket().isClosed();
        });
        row("dc.blockedReceive", () -> {
            DatagramChannel a = DatagramChannel.open();
            a.bind(new InetSocketAddress(LO, 0));
            return parked(() -> a.receive(ByteBuffer.allocate(4)), () -> a.close(), false);
        });
        row("mcs.basic", () -> {
            MulticastSocket m = new MulticastSocket();
            String r = "closed=" + m.isClosed() + " bound=" + m.isBound();
            @SuppressWarnings("deprecation")
            boolean lb = m.getLoopbackMode();
            m.setOption(StandardSocketOptions.IP_MULTICAST_LOOP, false);
            r += " lbDisabledBefore=" + lb + " loopOpt=" + m.getOption(StandardSocketOptions.IP_MULTICAST_LOOP)
                    + " ttl=" + m.getTimeToLive();
            m.close();
            return r + " closedAfter=" + m.isClosed();
        });
        row("mcs.join", () -> {
            InetAddress group = InetAddress.getByName("230.0.0.1");
            try (MulticastSocket m = new MulticastSocket()) {
                @SuppressWarnings("deprecation")
                String j = ex(() -> { m.joinGroup(group); return "joined"; });
                @SuppressWarnings("deprecation")
                String l = ex(() -> { m.leaveGroup(group); return "left"; });
                @SuppressWarnings("deprecation")
                String l2 = ex(() -> { m.leaveGroup(group); return "left-again"; });
                return "join=" + j + " leave=" + l + " leave2=" + l2;
            }
        });
        row("dc.membership", () -> {
            NetworkInterface lo = NetworkInterface.getByInetAddress(InetAddress.getByName("127.0.0.1"));
            if (lo == null) return "no-loopback-interface";
            try (DatagramChannel d = DatagramChannel.open(java.net.StandardProtocolFamily.INET)) {
                d.setOption(StandardSocketOptions.SO_REUSEADDR, true);
                d.bind(new InetSocketAddress(0));
                MembershipKey k = d.join(InetAddress.getByName("230.0.0.2"), lo);
                String r = "valid=" + k.isValid() + " group=" + k.group() + " same=" + (d.join(k.group(), lo) == k);
                k.drop();
                return r + " afterDrop=" + k.isValid();
            }
        });

        // ---- asynchronous server socket (held native this wave) --------------------------
        row("assc.accept", () -> {
            try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
                String before = "local0=" + s.getLocalAddress() + " open=" + s.isOpen();
                Object r = s.bind(new InetSocketAddress(LO, 0));
                Future<AsynchronousSocketChannel> fut = s.accept();
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress());
                        AsynchronousSocketChannel a = fut.get(5, TimeUnit.SECONDS)) {
                    c.write(bb("aio"));
                    ByteBuffer in = ByteBuffer.allocate(3);
                    int n = 0;
                    while (in.hasRemaining()) n += a.read(in).get(5, TimeUnit.SECONDS);
                    return cls(s) + " " + before + " same=" + (r == s) + " n=" + n + " got=" + str(in)
                            + " acls=" + cls(a) + " str=" + shape(s);
                }
            }
        });

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                if (first[i] == TIMEOUT) continue; // would only time out again
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
