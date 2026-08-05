// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketAddress;
import java.nio.channels.DatagramChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/// Matrix probe for the wildcard-`InetAddress` contract that
/// `new ServerSocket(0)` depends on.
///
/// `ServerSocket(int,int,InetAddress)` builds `new InetSocketAddress(bindAddr,
/// port)` with a NULL `bindAddr`; the JDK constructor substitutes
/// `InetAddress.anyLocalAddress()`. `ServerSocket.bind` then hands
/// `epoint.getAddress()` to `SocketImpl.bind`, and `sun.nio.ch.Net.bind`
/// immediately calls `addr.isLinkLocalAddress()` on it — so a null anywhere in
/// that chain is an NPE inside `Net.bind`, not a diagnosable error.
///
/// Every line is normalised so a correct VM prints output byte-identical to
/// HotSpot. Ports and other run-varying values are reduced to a property
/// (`>0`, `==0`, `nonnull`) rather than printed raw. PAIRED properties are
/// asserted (e.g. `isUnresolved()` must be exactly `getAddress()==null`) so a
/// self-consistent-but-wrong implementation still shows up.
public final class ServerSocketNullInetAddressProbe {

    private static void p(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /// Normalised description of an InetAddress: never the raw hostname, which
    /// varies per machine.
    private static String addr(InetAddress a) {
        if (a == null) {
            return "null";
        }
        StringBuilder sb = new StringBuilder();
        sb.append(a.getClass().getSimpleName());
        sb.append('/').append(a.getHostAddress());
        sb.append(" any=").append(a.isAnyLocalAddress());
        sb.append(" loop=").append(a.isLoopbackAddress());
        sb.append(" ll=").append(a.isLinkLocalAddress());
        sb.append(" sl=").append(a.isSiteLocalAddress());
        sb.append(" mc=").append(a.isMulticastAddress());
        sb.append(" len=").append(a.getAddress().length);
        sb.append(" str=").append(a.toString());
        return sb.toString();
    }

    /// Normalised description of a SocketAddress.
    private static String isa(SocketAddress sa) {
        if (sa == null) {
            return "null";
        }
        if (!(sa instanceof InetSocketAddress i)) {
            return "notInetSocketAddress:" + sa.getClass().getName();
        }
        StringBuilder sb = new StringBuilder();
        sb.append("addr=").append(addr(i.getAddress()));
        sb.append(" host=").append(i.getHostString());
        sb.append(" unresolved=").append(i.isUnresolved());
        sb.append(" portPositive=").append(i.getPort() > 0);
        // PAIRED property: the JDK defines isUnresolved() as addr == null.
        sb.append(" pairOk=").append(i.isUnresolved() == (i.getAddress() == null));
        // Rendered form, with the run-varying port replaced so the line stays
        // comparable. HotSpot: `host/ip:port`, or `host/<unresolved>:port`.
        sb.append(" str=").append(i.toString().replace(":" + i.getPort(), ":<port>"));
        return sb.toString();
    }

    private static String thrown(Throwable t) {
        if (t == null) {
            return "none";
        }
        Throwable c = t;
        // NPEs from a missing wildcard address are the defect this probe
        // exists for: report the type only, since helpful-NPE message text
        // is implementation detail.
        return c.getClass().getName();
    }

    // ---- 1. InetAddress statics ------------------------------------------

    private static void section1() throws Exception {
        InetAddress any;
        try {
            // The exact call ServerSocket depends on, reached the only way it
            // is reachable from public API without reflection.
            any = new InetSocketAddress(0).getAddress();
        } catch (Throwable t) {
            any = null;
            p("s1.wildcardViaIsa.throws", thrown(t));
        }
        p("s1.wildcardViaIsa", addr(any));

        p("s1.getByName_null", addr(InetAddress.getByName(null)));
        p("s1.getByName_0000", addr(InetAddress.getByName("0.0.0.0")));
        p("s1.getByAddress_zero", addr(InetAddress.getByAddress(new byte[] {0, 0, 0, 0})));
        p("s1.loopbackAddress", addr(InetAddress.getLoopbackAddress()));
        p("s1.getByName_127", addr(InetAddress.getByName("127.0.0.1")));
        p("s1.getByName_linklocal", addr(InetAddress.getByName("169.254.1.1")));

        // Identity: the wildcard the JDK hands out is a cached singleton, and
        // is an Inet4Address when IPv6 is unavailable, Inet6Address otherwise.
        InetAddress a1 = new InetSocketAddress(0).getAddress();
        InetAddress a2 = new InetSocketAddress(0).getAddress();
        p("s1.wildcardStable", a1 != null && a1.equals(a2));
        p("s1.wildcardEqualsGetByNameNull", a1 != null && a1.equals(InetAddress.getByName(null)));
    }

    // ---- 2. InetSocketAddress constructors -------------------------------

    private static void section2() {
        p("s2.isa_int0", isa(new InetSocketAddress(0)));
        p("s2.isa_int1234", isa(new InetSocketAddress(1234)));
        p("s2.isa_nullAddr0", isa(new InetSocketAddress((InetAddress) null, 0)));
        p("s2.isa_nullAddr1234", isa(new InetSocketAddress((InetAddress) null, 1234)));
        p("s2.isa_string", isa(new InetSocketAddress("127.0.0.1", 1234)));
        p("s2.isa_unresolved", isa(InetSocketAddress.createUnresolved("nonexistent.invalid", 1234)));

        // The JDK's ERROR behaviour, which a lenient re-implementation loses.
        try {
            new InetSocketAddress(-1);
            p("s2.isa_negPort", "noThrow");
        } catch (Throwable t) {
            p("s2.isa_negPort", thrown(t));
        }
        try {
            new InetSocketAddress(65536);
            p("s2.isa_bigPort", "noThrow");
        } catch (Throwable t) {
            p("s2.isa_bigPort", thrown(t));
        }
        try {
            new InetSocketAddress((InetAddress) null, -1);
            p("s2.isa_nullAddrNegPort", "noThrow");
        } catch (Throwable t) {
            p("s2.isa_nullAddrNegPort", thrown(t));
        }
    }

    // ---- 3. ServerSocket -------------------------------------------------

    private static void section3() throws Exception {
        // The doc's headline: `new ServerSocket(0)` NPE'd inside Net.bind.
        try (ServerSocket ss = new ServerSocket(0)) {
            p("s3.ctor0.bound", ss.isBound());
            p("s3.ctor0.portPositive", ss.getLocalPort() > 0);
            p("s3.ctor0.inetAddress", addr(ss.getInetAddress()));
            p("s3.ctor0.localSocketAddress", isa(ss.getLocalSocketAddress()));
        } catch (Throwable t) {
            p("s3.ctor0", "THREW " + thrown(t));
        }

        // Same shape with an explicit backlog and an explicit null bindAddr —
        // the three-arg constructor is what the one-arg one delegates to.
        try (ServerSocket ss = new ServerSocket(0, 50, null)) {
            p("s3.ctor3null.bound", ss.isBound());
            p("s3.ctor3null.portPositive", ss.getLocalPort() > 0);
            p("s3.ctor3null.inetAddress", addr(ss.getInetAddress()));
        } catch (Throwable t) {
            p("s3.ctor3null", "THREW " + thrown(t));
        }

        // Unbound + bind(null): "ephemeral port on the wildcard address".
        try (ServerSocket ss = new ServerSocket()) {
            p("s3.unbound.bound", ss.isBound());
            p("s3.unbound.localPort", ss.getLocalPort());
            p("s3.unbound.inetAddress", addr(ss.getInetAddress()));
            p("s3.unbound.localSocketAddress", isa(ss.getLocalSocketAddress()));
            ss.bind(null);
            p("s3.bindNull.bound", ss.isBound());
            p("s3.bindNull.portPositive", ss.getLocalPort() > 0);
            p("s3.bindNull.inetAddress", addr(ss.getInetAddress()));
        } catch (Throwable t) {
            p("s3.bindNull", "THREW " + thrown(t));
        }

        // bind(new InetSocketAddress(0)) — the wildcard, explicitly.
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(new InetSocketAddress(0));
            p("s3.bindWildcard.portPositive", ss.getLocalPort() > 0);
            p("s3.bindWildcard.inetAddress", addr(ss.getInetAddress()));
        } catch (Throwable t) {
            p("s3.bindWildcard", "THREW " + thrown(t));
        }

        // bind(new InetSocketAddress((InetAddress) null, 0)) — same wildcard,
        // reached through the constructor ServerSocket itself uses.
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(new InetSocketAddress((InetAddress) null, 0));
            p("s3.bindNullAddr.portPositive", ss.getLocalPort() > 0);
            p("s3.bindNullAddr.inetAddress", addr(ss.getInetAddress()));
        } catch (Throwable t) {
            p("s3.bindNullAddr", "THREW " + thrown(t));
        }

        // An unresolved address must be REJECTED, not silently bound.
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(InetSocketAddress.createUnresolved("nonexistent.invalid", 0));
            p("s3.bindUnresolved", "noThrow");
        } catch (Throwable t) {
            p("s3.bindUnresolved", thrown(t));
        }

        // A round trip through the wildcard-bound socket: a bind that does not
        // actually listen is not a bind.
        try (ServerSocket ss = new ServerSocket(0)) {
            int port = ss.getLocalPort();
            try (Socket client = new Socket()) {
                client.connect(new InetSocketAddress("127.0.0.1", port), 5000);
                try (Socket accepted = ss.accept()) {
                    accepted.getOutputStream().write(0x41);
                    accepted.getOutputStream().flush();
                    int b = client.getInputStream().read();
                    p("s3.roundTrip", b == 0x41);
                }
            }
        } catch (Throwable t) {
            p("s3.roundTrip", "THREW " + thrown(t));
        }
    }

    // ---- 4. Channels: the same null flows through Net.bind ----------------

    private static void section4() throws Exception {
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(null);
            p("s4.sscBindNull.localAddress", isa(ssc.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.sscBindNull", "THREW " + thrown(t));
        }
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress(0));
            p("s4.sscBindWildcard.localAddress", isa(ssc.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.sscBindWildcard", "THREW " + thrown(t));
        }
        try (SocketChannel sc = SocketChannel.open()) {
            sc.bind(null);
            p("s4.scBindNull.localAddress", isa(sc.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.scBindNull", "THREW " + thrown(t));
        }
        try (DatagramChannel dc = DatagramChannel.open()) {
            dc.bind(null);
            p("s4.dcBindNull.localAddress", isa(dc.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.dcBindNull", "THREW " + thrown(t));
        }
        try (DatagramSocket ds = new DatagramSocket()) {
            p("s4.dgramPortPositive", ds.getLocalPort() > 0);
            p("s4.dgramLocalAddress", addr(ds.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.dgram", "THREW " + thrown(t));
        }
        // Socket bound to the wildcard before connect.
        try (Socket s = new Socket()) {
            s.bind(null);
            p("s4.socketBindNull.bound", s.isBound());
            p("s4.socketBindNull.localAddress", addr(s.getLocalAddress()));
        } catch (Throwable t) {
            p("s4.socketBindNull", "THREW " + thrown(t));
        }
    }

    // ---- 5. Wildcard identity as Net.bind sees it ------------------------

    private static void section5() throws Exception {
        // Net.bind's very first act on the address is isLinkLocalAddress().
        // Whatever the wildcard is, that call must not throw and must be false.
        InetAddress wildcard = new InetSocketAddress(0).getAddress();
        if (wildcard == null) {
            p("s5.wildcard", "NULL");
            return;
        }
        p("s5.linkLocal", wildcard.isLinkLocalAddress());
        p("s5.anyLocal", wildcard.isAnyLocalAddress());
        p("s5.isInet4", wildcard instanceof Inet4Address);
        p("s5.hostAddress", wildcard.getHostAddress());
        p("s5.addressBytesAllZero", allZero(wildcard.getAddress()));
        p("s5.equalsSelf", wildcard.equals(wildcard));
        p("s5.hashStable", wildcard.hashCode() == wildcard.hashCode());
    }

    private static boolean allZero(byte[] b) {
        for (byte x : b) {
            if (x != 0) {
                return false;
            }
        }
        return true;
    }

    public static void main(String[] args) throws Exception {
        section1();
        section2();
        section3();
        section4();
        section5();
        System.out.println("SERVERSOCKET_NULL_INETADDRESS_PROBE_DONE");
    }
}
