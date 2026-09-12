import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/** L6 — `java.net.DatagramSocket` (35 rows), `MulticastSocket` (13),
 *  `DatagramPacket`, `Socket`/`ServerSocket` options, and the `Proxy` /
 *  `SocketPermission` value types.
 *
 *  Every socket here is bound to the LOOPBACK interface on an EPHEMERAL port
 *  and no packet crosses a real network. Nothing prints a port number, an
 *  address chosen by the kernel, or a timing — the datagram rows send to the
 *  socket's own bound address and assert the CONTENT that comes back, plus the
 *  validation refusals around it, which is what a native gets wrong.
 *
 *  The rows that matter are the argument contracts, because that is what a
 *  shim in front of a real socket layer reimplements by hand:
 *
 *    * `DatagramPacket`'s offset/length validation, on construction and on
 *      every setter, including the `off + len` overflow shape;
 *    * `setSoTimeout` / `setSendBufferSize` / `setReceiveBufferSize` and
 *      `setTrafficClass` on their negative and out-of-range arguments;
 *    * the operations that must refuse on a CLOSED socket, and the ones that
 *      must stay legal;
 *    * `connect` to a port outside 0..65535 and to a null address;
 *    * the `SocketOption` surface (`getOption`/`setOption`/`supportedOptions`).
 *
 *  `MulticastSocket` is asked only what needs no group membership on a real
 *  interface: the deprecated TTL accessors' validation and the refusals.
 */
public class L6SocketSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static final InetAddress LOOP = InetAddress.getLoopbackAddress();

    public static void main(String[] args) throws Exception {
        // ---- DatagramPacket construction and setter validation
        byte[] buf = new byte[32];
        tv("packet ctor len>buf", () -> new DatagramPacket(buf, 33).getLength());
        tv("packet ctor len<0", () -> new DatagramPacket(buf, -1).getLength());
        tv("packet ctor off<0", () -> new DatagramPacket(buf, -1, 4).getLength());
        tv("packet ctor off+len>buf", () -> new DatagramPacket(buf, 30, 4).getLength());
        tv("packet ctor off+len overflow", () -> new DatagramPacket(buf, 1, Integer.MAX_VALUE).getLength());
        tv("packet ctor null buf", () -> new DatagramPacket(null, 4).getLength());
        tv("packet ctor len 0", () -> new DatagramPacket(buf, 0).getLength());
        tv("packet ctor exact", () -> new DatagramPacket(buf, 8, 24).getOffset() + "/" + new DatagramPacket(buf, 8, 24).getLength());
        tv("packet setLength > buf", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setLength(33);
            return q.getLength();
        });
        tv("packet setLength negative", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setLength(-1);
            return q.getLength();
        });
        tv("packet setLength past offset", () -> {
            DatagramPacket q = new DatagramPacket(buf, 30, 2);
            q.setLength(4);
            return q.getLength();
        });
        tv("packet setData null", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setData(null);
            return "no throw";
        });
        tv("packet setData shrinks length", () -> {
            DatagramPacket q = new DatagramPacket(buf, 32);
            q.setData(new byte[4]);
            return q.getLength() + "/" + q.getOffset();
        });
        tv("packet setData off/len bad", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setData(new byte[4], 2, 8);
            return q.getLength();
        });
        tv("packet setPort out of range", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setPort(65536);
            return q.getPort();
        });
        tv("packet setPort negative", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setPort(-1);
            return q.getPort();
        });
        tv("packet setSocketAddress unresolved", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setSocketAddress(InetSocketAddress.createUnresolved("h.invalid", 9));
            return "no throw";
        });
        tv("packet setSocketAddress null", () -> {
            DatagramPacket q = new DatagramPacket(buf, 8);
            q.setSocketAddress(null);
            return "no throw";
        });
        tv("packet default address null", () -> String.valueOf(new DatagramPacket(buf, 8).getAddress()));
        tv("packet default port", () -> new DatagramPacket(buf, 8).getPort());
        tv("packet getData identity", () -> new DatagramPacket(buf, 8).getData() == buf);

        // ---- DatagramSocket bound to loopback: options, and their contracts
        tv("datagram options", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                StringBuilder sb = new StringBuilder();
                sb.append("bound=").append(s.isBound());
                sb.append(" connected=").append(s.isConnected());
                sb.append(" closed=").append(s.isClosed());
                sb.append(" localAddrLoopback=").append(s.getLocalAddress().isLoopbackAddress());
                sb.append(" portInRange=").append(s.getLocalPort() > 0 && s.getLocalPort() < 65536);
                sb.append(" soTimeout=").append(s.getSoTimeout());
                sb.append(" broadcast=").append(s.getBroadcast());
                sb.append(" reuseAddr=").append(s.getReuseAddress());
                sb.append(" trafficClass=").append(s.getTrafficClass());
                sb.append(" remoteAddr=").append(s.getInetAddress());
                sb.append(" remotePort=").append(s.getPort());
                sb.append(" sendBuf>0=").append(s.getSendBufferSize() > 0);
                sb.append(" recvBuf>0=").append(s.getReceiveBufferSize() > 0);
                return sb.toString();
            }
        });
        tv("datagram setSoTimeout -1", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setSoTimeout(-1); return "accepted"; } });
        tv("datagram setSoTimeout 0", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setSoTimeout(0); return s.getSoTimeout(); } });
        tv("datagram setSendBufferSize 0", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setSendBufferSize(0); return "accepted"; } });
        tv("datagram setSendBufferSize -1", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setSendBufferSize(-1); return "accepted"; } });
        tv("datagram setReceiveBufferSize 0", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setReceiveBufferSize(0); return "accepted"; } });
        tv("datagram setTrafficClass -1", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setTrafficClass(-1); return "accepted"; } });
        tv("datagram setTrafficClass 256", () -> { try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setTrafficClass(256); return "accepted"; } });
        tv("datagram setTrafficClass 8 roundtrip", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.setTrafficClass(8); return s.getTrafficClass(); }
        });
        tv("datagram connect port 65536", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.connect(LOOP, 65536); return "accepted"; }
        });
        tv("datagram connect port -1", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.connect(LOOP, -1); return "accepted"; }
        });
        tv("datagram connect null address", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.connect((InetAddress) null, 9); return "accepted"; }
        });
        tv("datagram connect unresolved SocketAddress", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.connect(InetSocketAddress.createUnresolved("h.invalid", 9));
                return "accepted";
            }
        });
        tv("datagram connect then state", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.connect(LOOP, 9);
                String r = "connected=" + s.isConnected()
                         + " remoteLoopback=" + s.getInetAddress().isLoopbackAddress()
                         + " remotePort=" + s.getPort();
                s.disconnect();
                return r + " afterDisconnect=" + s.isConnected() + "/" + s.getPort()
                         + "/" + s.getInetAddress();
            }
        });
        tv("datagram send to wrong peer while connected", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.connect(LOOP, 9);
                DatagramPacket q = new DatagramPacket(new byte[1], 1, LOOP, 10);
                s.send(q);
                return "accepted";
            }
        });
        tv("datagram send null packet", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { s.send(null); return "accepted"; }
        });
        tv("datagram bind twice", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.bind(new InetSocketAddress(LOOP, 0));
                return "accepted";
            }
        });
        tv("datagram unbound then bind", () -> {
            try (DatagramSocket s = new DatagramSocket(null)) {
                String before = "bound=" + s.isBound() + " localPort=" + s.getLocalPort();
                s.bind(new InetSocketAddress(LOOP, 0));
                return before + " after bound=" + s.isBound() + " portValid=" + (s.getLocalPort() > 0);
            }
        });

        // ---- the closed-socket contract: which calls throw and which do not
        tv("datagram closed surface", () -> {
            DatagramSocket s = new DatagramSocket(0, LOOP);
            int before = s.getLocalPort();
            s.close();
            StringBuilder sb = new StringBuilder();
            sb.append("isClosed=").append(s.isClosed());
            sb.append(" isBound=").append(s.isBound());
            // The port itself is the kernel's choice and differs every run.
            // What the contract fixes is whether close() KEEPS it.
            sb.append(" localPortKept=").append(s.getLocalPort() == before);
            sb.append(" localAddress=").append(s.getLocalAddress());
            sb.append(" localSocketAddress=").append(s.getLocalSocketAddress());
            try { s.getSoTimeout(); sb.append(" getSoTimeout=ok"); }
            catch (Throwable t) { sb.append(" getSoTimeout=").append(t.getClass().getName()); }
            try { s.setSoTimeout(1); sb.append(" setSoTimeout=ok"); }
            catch (Throwable t) { sb.append(" setSoTimeout=").append(t.getClass().getName()); }
            try { s.getReceiveBufferSize(); sb.append(" getRecvBuf=ok"); }
            catch (Throwable t) { sb.append(" getRecvBuf=").append(t.getClass().getName()); }
            try { s.send(new DatagramPacket(new byte[1], 1, LOOP, 9)); sb.append(" send=ok"); }
            catch (Throwable t) { sb.append(" send=").append(t.getClass().getName()); }
            try { s.receive(new DatagramPacket(new byte[1], 1)); sb.append(" receive=ok"); }
            catch (Throwable t) { sb.append(" receive=").append(t.getClass().getName()); }
            s.close();
            return sb.append(" doubleClose=ok").toString();
        });

        // ---- a real loopback round-trip, asserting the CONTENT only
        tv("datagram loopback roundtrip", () -> {
            try (DatagramSocket rx = new DatagramSocket(0, LOOP);
                 DatagramSocket tx = new DatagramSocket(0, LOOP)) {
                rx.setSoTimeout(5000);
                byte[] payload = "l6-datagram-payload".getBytes(StandardCharsets.UTF_8);
                tx.send(new DatagramPacket(payload, payload.length, LOOP, rx.getLocalPort()));
                byte[] in = new byte[64];
                DatagramPacket q = new DatagramPacket(in, in.length);
                rx.receive(q);
                return "len=" + q.getLength() + " off=" + q.getOffset()
                     + " data=" + new String(q.getData(), q.getOffset(), q.getLength(), StandardCharsets.UTF_8)
                     + " fromLoopback=" + q.getAddress().isLoopbackAddress()
                     + " fromPortMatches=" + (q.getPort() == tx.getLocalPort());
            }
        });
        tv("datagram receive truncates to buffer", () -> {
            try (DatagramSocket rx = new DatagramSocket(0, LOOP);
                 DatagramSocket tx = new DatagramSocket(0, LOOP)) {
                rx.setSoTimeout(5000);
                byte[] payload = "0123456789abcdef".getBytes(StandardCharsets.UTF_8);
                tx.send(new DatagramPacket(payload, payload.length, LOOP, rx.getLocalPort()));
                DatagramPacket q = new DatagramPacket(new byte[4], 4);
                rx.receive(q);
                return "len=" + q.getLength()
                     + " data=" + new String(q.getData(), q.getOffset(), q.getLength(), StandardCharsets.UTF_8);
            }
        });
        tv("datagram receive honours offset", () -> {
            try (DatagramSocket rx = new DatagramSocket(0, LOOP);
                 DatagramSocket tx = new DatagramSocket(0, LOOP)) {
                rx.setSoTimeout(5000);
                byte[] payload = "abcd".getBytes(StandardCharsets.UTF_8);
                tx.send(new DatagramPacket(payload, payload.length, LOOP, rx.getLocalPort()));
                byte[] in = new byte[16];
                Arrays.fill(in, (byte) '.');
                DatagramPacket q = new DatagramPacket(in, 8, 8);
                rx.receive(q);
                return "off=" + q.getOffset() + " len=" + q.getLength()
                     + " buf=" + new String(in, StandardCharsets.UTF_8);
            }
        });
        tv("datagram zero-length send/receive", () -> {
            try (DatagramSocket rx = new DatagramSocket(0, LOOP);
                 DatagramSocket tx = new DatagramSocket(0, LOOP)) {
                rx.setSoTimeout(5000);
                tx.send(new DatagramPacket(new byte[0], 0, LOOP, rx.getLocalPort()));
                DatagramPacket q = new DatagramPacket(new byte[8], 8);
                rx.receive(q);
                return "len=" + q.getLength();
            }
        });
        tv("datagram receive timeout throws", () -> {
            try (DatagramSocket rx = new DatagramSocket(0, LOOP)) {
                rx.setSoTimeout(120);
                rx.receive(new DatagramPacket(new byte[4], 4));
                return "no throw";
            } catch (Throwable t) { return t.getClass().getName(); }
        });

        // ---- the SocketOption surface
        tv("datagram supportedOptions has SO_RCVBUF", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                return s.supportedOptions().contains(java.net.StandardSocketOptions.SO_RCVBUF);
            }
        });
        tv("datagram getOption SO_REUSEADDR", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                return String.valueOf(s.getOption(java.net.StandardSocketOptions.SO_REUSEADDR));
            }
        });
        tv("datagram setOption SO_SNDBUF roundtrip", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.setOption(java.net.StandardSocketOptions.SO_SNDBUF, 32768);
                return s.getOption(java.net.StandardSocketOptions.SO_SNDBUF) > 0;
            }
        });
        tv("datagram getOption null", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) { return String.valueOf(s.getOption(null)); }
        });
        tv("datagram setOption unsupported", () -> {
            try (DatagramSocket s = new DatagramSocket(0, LOOP)) {
                s.setOption(java.net.StandardSocketOptions.TCP_NODELAY, Boolean.TRUE);
                return "accepted";
            }
        });

        // ---- MulticastSocket: only what needs no live group membership
        tv("multicast ttl surface", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) {
                return "ttl=" + s.getTimeToLive() + " loop=" + s.getOption(java.net.StandardSocketOptions.IP_MULTICAST_LOOP);
            }
        });
        tv("multicast setTimeToLive -1", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) { s.setTimeToLive(-1); return "accepted"; }
        });
        tv("multicast setTimeToLive 256", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) { s.setTimeToLive(256); return "accepted"; }
        });
        tv("multicast setTimeToLive 4 roundtrip", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) { s.setTimeToLive(4); return s.getTimeToLive(); }
        });
        tv("multicast joinGroup non-multicast address", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) {
                s.joinGroup(new InetSocketAddress(LOOP, 0), null);
                return "accepted";
            }
        });
        tv("multicast joinGroup null", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) { s.joinGroup(null, null); return "accepted"; }
        });
        tv("multicast is a DatagramSocket", () -> {
            try (MulticastSocket s = new MulticastSocket(0)) { return s instanceof DatagramSocket; }
        });

        // ---- Socket / ServerSocket, unconnected and unbound
        tv("unconnected socket surface", () -> {
            try (Socket s = new Socket()) {
                return "connected=" + s.isConnected() + " bound=" + s.isBound() + " closed=" + s.isClosed()
                     + " inputShutdown=" + s.isInputShutdown() + " outputShutdown=" + s.isOutputShutdown()
                     + " localPort=" + s.getLocalPort() + " port=" + s.getPort()
                     + " addr=" + s.getInetAddress() + " localAddr=" + s.getLocalAddress();
            }
        });
        tv("socket getInputStream before connect", () -> {
            try (Socket s = new Socket()) { return String.valueOf(s.getInputStream()); }
        });
        tv("socket connect port 65536", () -> {
            try (Socket s = new Socket()) { s.connect(new InetSocketAddress(LOOP, 65536), 100); return "accepted"; }
        });
        tv("socket connect null endpoint", () -> {
            try (Socket s = new Socket()) { s.connect(null, 100); return "accepted"; }
        });
        tv("socket connect negative timeout", () -> {
            try (Socket s = new Socket()) { s.connect(new InetSocketAddress(LOOP, 9), -1); return "accepted"; }
        });
        tv("socket connect unresolved", () -> {
            try (Socket s = new Socket()) {
                s.connect(InetSocketAddress.createUnresolved("no.such.host.invalid", 9), 100);
                return "accepted";
            }
        });
        tv("socket setSoTimeout -1", () -> {
            try (Socket s = new Socket()) { s.setSoTimeout(-1); return "accepted"; }
        });
        tv("socket setSoLinger negative", () -> {
            try (Socket s = new Socket()) { s.setSoLinger(true, -1); return "accepted"; }
        });
        tv("socket setSendBufferSize 0", () -> {
            try (Socket s = new Socket()) { s.setSendBufferSize(0); return "accepted"; }
        });
        tv("serversocket bind and accept-timeout", () -> {
            try (ServerSocket s = new ServerSocket(0, 1, LOOP)) {
                s.setSoTimeout(120);
                String r = "bound=" + s.isBound() + " closed=" + s.isClosed()
                         + " loopback=" + s.getInetAddress().isLoopbackAddress()
                         + " portValid=" + (s.getLocalPort() > 0);
                try { s.accept(); return r + " accept=no-throw"; }
                catch (Throwable t) { return r + " accept=" + t.getClass().getName(); }
            }
        });
        tv("serversocket connected client sees peer", () -> {
            try (ServerSocket ss = new ServerSocket(0, 1, LOOP)) {
                ss.setSoTimeout(5000);
                try (Socket c = new Socket()) {
                    c.connect(new InetSocketAddress(LOOP, ss.getLocalPort()), 5000);
                    try (Socket a = ss.accept()) {
                        a.getOutputStream().write("hello-l6".getBytes(StandardCharsets.UTF_8));
                        a.getOutputStream().flush();
                        byte[] in = new byte[8];
                        int n = c.getInputStream().read(in);
                        return "read=" + n + " data=" + new String(in, 0, Math.max(n, 0), StandardCharsets.UTF_8)
                             + " clientConnected=" + c.isConnected()
                             + " peerLoopback=" + c.getInetAddress().isLoopbackAddress();
                    }
                }
            }
        });
        tv("serversocket bind port 65536", () -> {
            try (ServerSocket s = new ServerSocket()) {
                s.bind(new InetSocketAddress(LOOP, 65536));
                return "accepted";
            }
        });
        tv("serversocket closed accept", () -> {
            ServerSocket s = new ServerSocket(0, 1, LOOP);
            s.close();
            try { s.accept(); return "no throw"; }
            catch (Throwable t) { return t.getClass().getName(); }
        });

        // ---- Proxy and the address value types
        p("Proxy.NO_PROXY type", Proxy.NO_PROXY.type());
        p("Proxy.NO_PROXY address", String.valueOf(Proxy.NO_PROXY.address()));
        p("Proxy.NO_PROXY toString", Proxy.NO_PROXY.toString());
        tv("Proxy DIRECT with address refused", () -> new Proxy(Proxy.Type.DIRECT, new InetSocketAddress(LOOP, 1)).toString());
        tv("Proxy HTTP without address refused", () -> new Proxy(Proxy.Type.HTTP, null).toString());
        tv("Proxy HTTP toString", () -> new Proxy(Proxy.Type.HTTP, InetSocketAddress.createUnresolved("p.invalid", 8080)).toString());
        tv("Proxy equals", () -> new Proxy(Proxy.Type.HTTP, InetSocketAddress.createUnresolved("p", 1))
                .equals(new Proxy(Proxy.Type.HTTP, InetSocketAddress.createUnresolved("p", 1))));

        // ---- the URI/URL-adjacent exception family carried by this lane
        tv("MalformedURLException msg", () -> new MalformedURLException("m").getMessage());
        tv("MalformedURLException is IOException", () -> new MalformedURLException("m") instanceof IOException);
        tv("UnknownHostException msg", () -> new UnknownHostException("m").getMessage());
        tv("UnknownHostException is IOException", () -> new UnknownHostException("m") instanceof IOException);
        tv("SocketTimeoutException is InterruptedIOException",
           () -> new SocketTimeoutException("m") instanceof InterruptedIOException);
        tv("ConnectException is SocketException", () -> new ConnectException("m") instanceof SocketException);
        tv("BindException is SocketException", () -> new BindException("m") instanceof SocketException);
        tv("NoRouteToHostException is SocketException", () -> new NoRouteToHostException("m") instanceof SocketException);
        tv("PortUnreachableException is SocketException", () -> new PortUnreachableException("m") instanceof SocketException);
        tv("ProtocolException is IOException", () -> new ProtocolException("m") instanceof IOException);
        tv("URISyntaxException getIndex default", () -> new URISyntaxException("in", "why").getIndex());
        tv("URISyntaxException message shape", () -> new URISyntaxException("in", "why", 2).getMessage());
        tv("URISyntaxException negative index", () -> new URISyntaxException("in", "why", -2).getIndex());
        tv("URISyntaxException null input", () -> new URISyntaxException(null, "why").getMessage());

        System.out.println("rows " + rows);
        System.out.println("DONE L6SocketSweep");
    }
}
