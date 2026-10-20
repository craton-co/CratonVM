// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.MulticastSocket;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketAddress;
import java.net.SocketTimeoutException;
import java.net.StandardSocketOptions;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
import java.nio.channels.DatagramChannel;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * SBR wave 2, lane {@code nioch}: the socket / channel / selector
 * implementation classes ({@code sun.nio.ch.*ChannelImpl},
 * {@code EPollSelectorImpl}, {@code SelectionKeyImpl}, the asynchronous file
 * and server-socket channels, {@code java.net.DatagramSocket} /
 * {@code MulticastSocket}) that are native-won in the int1 census.
 *
 * <p>Each row is a small loopback scenario that returns only deterministic
 * observations (booleans, byte contents, exception classes, class names --
 * never a port or an identity hash). It is evaluated ONCE and printed (a
 * native that wins from the first call shows here), then re-evaluated inside a
 * hot loop and compared with that first value; {@code drift} counts
 * mismatches, so cold == warm is asserted rather than assumed. Diff the whole
 * output against HotSpot 25.
 *
 * <pre>
 *   java SbrW2Nioch [iters]            (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW2Nioch [iters]
 * </pre>
 */
public class SbrW2Nioch {
    interface S {
        String get() throws Exception;
    }

    static final List<String> LABELS = new ArrayList<>();
    static final List<S> ROWS = new ArrayList<>();

    static void row(String label, S s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    static String safe(S s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
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

    /** Strip the variable parts (ports, addresses) out of a channel's toString. */
    static String shape(Object o) {
        String s = String.valueOf(o).replaceAll("@[0-9a-f]+", "@H");
        return s.replaceAll("[0-9]+", "N").replaceAll("localhost|127\\.N\\.N\\.N|/N\\.N\\.N\\.N", "A");
    }

    static String cls(Object o) {
        return o == null ? "null" : o.getClass().getName();
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
        row("tcp.adaptor", () -> {
            try (ServerSocketChannel s = ServerSocketChannel.open()) {
                s.bind(new InetSocketAddress(LO, 0));
                try (SocketChannel c = SocketChannel.open(s.getLocalAddress()); SocketChannel a = s.accept()) {
                    Socket sk = c.socket();
                    sk.getOutputStream().write("adapt".getBytes(StandardCharsets.US_ASCII));
                    ByteBuffer in = ByteBuffer.allocate(5);
                    readFully(a, in);
                    return cls(sk) + " connected=" + sk.isConnected() + " bound=" + sk.isBound()
                            + " closed=" + sk.isClosed() + " got=" + str(in)
                            + " chanSame=" + (sk.getChannel() == c);
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
        row("sel.wakeup", () -> {
            try (Selector sel = Selector.open()) {
                sel.wakeup();
                long t0 = System.nanoTime();
                int n = sel.select(5000);
                long ms = (System.nanoTime() - t0) / 1_000_000;
                return "n=" + n + " fast=" + (ms < 2500);
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
                String r = cls(a) + " n=" + n + " got=" + str(in) + " fromEq=" + from.equals(b.getLocalAddress())
                        + " blk=" + a.isBlocking() + " conn=" + a.isConnected() + " adaptor=" + cls(ad)
                        + " adTo=" + ad.getSoTimeout() + " adBound=" + ad.isBound();
                return r;
            }
        });
        row("dc.close", () -> {
            DatagramChannel a = DatagramChannel.open();
            a.close();
            return "open=" + a.isOpen() + " send=" + ex(() -> String.valueOf(a.send(bb("x"), new InetSocketAddress(LO, 9))))
                    + " sockClosed=" + a.socket().isClosed();
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

        // ---- asynchronous channels -------------------------------------------------------
        row("afc.io", () -> {
            Path f = Files.createTempFile("sbrw2", ".bin");
            try {
                try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                        StandardOpenOption.WRITE)) {
                    int w = ch.write(bb("async-file"), 0).get(5, TimeUnit.SECONDS);
                    long size = ch.size();
                    ByteBuffer in = ByteBuffer.allocate(5);
                    int r = ch.read(in, 6).get(5, TimeUnit.SECONDS);
                    ch.force(true);
                    Object t = ch.truncate(4);
                    long size2 = ch.size();
                    FileLock l = ch.lock().get(5, TimeUnit.SECONDS);
                    String lk = "lockValid=" + l.isValid() + " shared=" + l.isShared() + " lcls=" + cls(l);
                    l.release();
                    lk += " afterRelease=" + l.isValid();
                    FileLock tl = ch.tryLock(0, 2, false);
                    lk += " try=" + (tl != null) + " tryValid=" + (tl != null && tl.isValid());
                    if (tl != null) tl.release();
                    int eofRead = ch.read(ByteBuffer.allocate(4), 100).get(5, TimeUnit.SECONDS);
                    return cls(ch) + " w=" + w + " size=" + size + " r=" + r + " got=" + str(in)
                            + " truncSame=" + (t == ch) + " size2=" + size2 + " " + lk + " eof=" + eofRead
                            + " open=" + ch.isOpen();
                }
            } finally {
                Files.deleteIfExists(f);
            }
        });
        row("afc.close", () -> {
            Path f = Files.createTempFile("sbrw2", ".bin");
            try {
                AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.WRITE);
                ch.close();
                ch.close();
                return "open=" + ch.isOpen() + " size=" + ex(() -> String.valueOf(ch.size()))
                        + " write=" + ex(() -> String.valueOf(ch.write(bb("x"), 0).get()))
                        + " readOnWriteOnly=" + ex(() -> "");
            } finally {
                Files.deleteIfExists(f);
            }
        });
        row("afc.handler", () -> {
            Path f = Files.createTempFile("sbrw2", ".bin");
            try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f, StandardOpenOption.READ,
                    StandardOpenOption.WRITE)) {
                final String[] res = new String[2];
                java.util.concurrent.CountDownLatch l = new java.util.concurrent.CountDownLatch(2);
                ch.write(bb("hnd"), 0, "W", new java.nio.channels.CompletionHandler<Integer, String>() {
                    public void completed(Integer v, String a) { res[0] = a + v; l.countDown(); }
                    public void failed(Throwable t, String a) { res[0] = a + "!" + cls(t); l.countDown(); }
                });
                ch.read(ByteBuffer.allocate(4), 1000, "R", new java.nio.channels.CompletionHandler<Integer, String>() {
                    public void completed(Integer v, String a) { res[1] = a + v; l.countDown(); }
                    public void failed(Throwable t, String a) { res[1] = a + "!" + cls(t); l.countDown(); }
                });
                boolean done = l.await(5, TimeUnit.SECONDS);
                return "done=" + done + " w=" + res[0] + " r=" + res[1];
            } finally {
                Files.deleteIfExists(f);
            }
        });
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
        row("assc.close", () -> {
            AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open();
            s.bind(new InetSocketAddress(LO, 0));
            s.close();
            return "open=" + s.isOpen() + " local=" + ex(() -> String.valueOf(s.getLocalAddress()))
                    + " accept=" + ex(() -> String.valueOf(s.accept().get(1, TimeUnit.SECONDS)));
        });

        // ---- FileChannel locks (FileLockImpl) ---------------------------------------------
        row("fc.lock", () -> {
            Path f = Files.createTempFile("sbrw2", ".bin");
            try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
                FileLock l = ch.lock();
                String r = cls(l) + " valid=" + l.isValid() + " shared=" + l.isShared() + " chan=" + (l.channel() == ch)
                        + " overlap=" + ex(() -> String.valueOf(ch.tryLock()));
                l.release();
                l.release();
                return r + " after=" + l.isValid() + " again=" + (ch.tryLock() != null);
            } finally {
                Files.deleteIfExists(f);
            }
        });

        // ---- interrupts -----------------------------------------------------------------
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

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 150;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
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
