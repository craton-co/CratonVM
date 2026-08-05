// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketAddress;
import java.net.UnknownHostException;
import java.nio.channels.SocketChannel;

/// Matrix probe for the `hostName` an `InetAddress` mirror remembers.
///
/// HotSpot's `InetAddress.toString()` is
/// `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`, and
/// `holder().hostName` is **null** whenever the address was built from a
/// numeric literal or from raw bytes — there was no name to remember. So a
/// literal-derived address prints `/127.0.0.1`, not `127.0.0.1/127.0.0.1`.
///
/// The distinction is NOT "host equals ip": the wildcard
/// `InetAddress.anyLocalAddress()` genuinely carries `hostName = "0.0.0.0"`
/// (`Inet4AddressImpl.anyLocalAddress` sets it), so it prints
/// `0.0.0.0/0.0.0.0`. Anything keying off `host == ip` breaks that row.
///
/// This probe pins WHICH constructions carry a name and which do not, and — the
/// part that actually constrains the fix — what the OTHER observers
/// (`getHostName()`, `getHostString()`) answer in each case, because those are
/// what a change to the stored `hostName` would move.
///
/// **`getHostName()` MUTATES the address.** On a holder with a null `hostName`
/// it performs a reverse lookup and CACHES the result back into the holder, so
/// any later observation of the same object — including an `InetSocketAddress`
/// that merely wraps it — reports a name that was not there before. An earlier
/// revision of this probe called `getHostName()` inside its per-address
/// description and then read the accepted socket's remote address, which
/// duly reported `kubernetes.docker.internal/127.0.0.1` on a machine with
/// Docker installed: a pure artifact of the probe's own observation order.
/// So `describe()` NEVER calls `getHostName()`; it lives in its own section on
/// its own freshly built addresses, and reverse DNS is machine-dependent, so
/// even there it is reported only as a RELATION (`==hostAddress`) rather than
/// as text. `hostNamePresent` is derived from `toString()`, the one observer
/// that exposes the raw holder field without a lookup.
public final class InetAddressHostNameProbe {

    private static void p(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /// True iff `holder.hostName` is non-null, read WITHOUT triggering a
    /// reverse lookup: `toString()` prints it verbatim and prints nothing at
    /// all when it is null.
    private static boolean hostNamePresent(InetAddress a) {
        return !a.toString().startsWith("/");
    }

    /// Everything about one address that does not vary per machine.
    ///
    /// Deliberately does NOT call `getHostName()` — see the class comment: that
    /// call writes a reverse-lookup result into the holder and would make every
    /// later row that shares this object report a name it never had.
    private static void describe(String key, InetAddress a) {
        if (a == null) {
            p(key, "null");
            return;
        }
        String ip = a.getHostAddress();
        StringBuilder sb = new StringBuilder();
        sb.append("class=").append(a.getClass().getSimpleName());
        sb.append(" ip=").append(ip);
        sb.append(" hostNamePresent=").append(hostNamePresent(a));
        // toString() with the (stable) IP elided so the shape is what is compared.
        sb.append(" strShape=").append(a.toString().replace(ip, "<ip>"));
        sb.append(" anyLocal=").append(a.isAnyLocalAddress());
        sb.append(" loopback=").append(a.isLoopbackAddress());
        p(key, sb.toString());
    }

    /// The `InetSocketAddress` observers that read the address's hostName —
    /// these are what a change to the stored name would move.
    private static void describeIsa(String key, InetSocketAddress i) {
        if (i == null) {
            p(key, "null");
            return;
        }
        InetAddress a = i.getAddress();
        String ip = a == null ? "<none>" : a.getHostAddress();
        StringBuilder sb = new StringBuilder();
        sb.append("hostString=").append(i.getHostString().replace(ip, "<ip>"));
        sb.append(" hostStringIsIp=").append(i.getHostString().equals(ip));
        sb.append(" unresolved=").append(i.isUnresolved());
        sb.append(" strShape=").append(
                i.toString().replace(ip, "<ip>").replace(":" + i.getPort(), ":<port>"));
        p(key, sb.toString());
    }

    // ---- 1. Which constructions carry a hostName -------------------------

    private static void section1() throws Exception {
        // NO name: numeric literals and raw bytes.
        describe("s1.getByName_v4literal", InetAddress.getByName("127.0.0.1"));
        describe("s1.getByName_wildcardLiteral", InetAddress.getByName("0.0.0.0"));
        describe("s1.getByName_linkLocalLiteral", InetAddress.getByName("169.254.1.1"));
        describe("s1.getByName_v6literal", InetAddress.getByName("::1"));
        describe("s1.getByAddress_bytes", InetAddress.getByAddress(new byte[] {1, 2, 3, 4}));
        describe("s1.getByAddress_zeroBytes", InetAddress.getByAddress(new byte[] {0, 0, 0, 0}));
        describe("s1.getByAddress_v6bytes",
                InetAddress.getByAddress(new byte[] {0, 0, 0, 0, 0, 0, 0, 0,
                                                     0, 0, 0, 0, 0, 0, 0, 1}));

        // HAS a name.
        describe("s1.getByName_localhost", InetAddress.getByName("localhost"));
        describe("s1.getLoopbackAddress", InetAddress.getLoopbackAddress());
        describe("s1.getByName_null", InetAddress.getByName(null));
        describe("s1.getByAddress_named",
                InetAddress.getByAddress("myhost.example", new byte[] {1, 2, 3, 4}));
        // The wildcard: host == ip, yet the name IS present. This is the row
        // that rules out any "host equals ip" heuristic.
        describe("s1.wildcardViaIsa", new InetSocketAddress(0).getAddress());

        // getAllByName's first element for a literal must match getByName.
        InetAddress[] all = InetAddress.getAllByName("127.0.0.1");
        p("s1.getAllByName_literalCount", all.length);
        describe("s1.getAllByName_literalFirst", all[0]);
    }

    // ---- 2. What the change would move ----------------------------------

    private static void section2() throws Exception {
        describeIsa("s2.isa_fromLiteralString", new InetSocketAddress("127.0.0.1", 8080));
        describeIsa("s2.isa_fromName", new InetSocketAddress("localhost", 8080));
        describeIsa("s2.isa_fromLiteralAddr",
                new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 8080));
        describeIsa("s2.isa_fromBytesAddr",
                new InetSocketAddress(InetAddress.getByAddress(new byte[] {1, 2, 3, 4}), 8080));
        describeIsa("s2.isa_wildcard", new InetSocketAddress(0));
        describeIsa("s2.isa_nullAddr", new InetSocketAddress((InetAddress) null, 8080));
        describeIsa("s2.isa_unresolved",
                InetSocketAddress.createUnresolved("nonexistent.invalid", 8080));
    }

    // ---- 3. equals/hashCode must NOT depend on the hostName --------------

    private static void section3() throws Exception {
        InetAddress literal = InetAddress.getByName("127.0.0.1");
        InetAddress named = InetAddress.getByName("localhost");
        InetAddress bytes = InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        InetAddress namedBytes =
                InetAddress.getByAddress("whatever.example", new byte[] {127, 0, 0, 1});
        // The JDK compares the ADDRESS only, so all four are equal even though
        // their hostNames differ. A fix that made equality name-sensitive would
        // break every host-based ACL.
        p("s3.literalEqualsNamed", literal.equals(named));
        p("s3.literalEqualsBytes", literal.equals(bytes));
        p("s3.literalEqualsNamedBytes", literal.equals(namedBytes));
        p("s3.hashesAgree",
                literal.hashCode() == named.hashCode()
                        && literal.hashCode() == bytes.hashCode()
                        && literal.hashCode() == namedBytes.hashCode());

        InetSocketAddress a = new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 9);
        InetSocketAddress b = new InetSocketAddress(InetAddress.getByName("localhost"), 9);
        p("s3.isaEqualsAcrossNames", a.equals(b));
        p("s3.isaHashesAgree", a.hashCode() == b.hashCode());
    }

    // ---- 4. The live socket surfaces that report peer/local addresses ----

    private static void section4() throws Exception {
        try (ServerSocket server = new ServerSocket(0)) {
            int port = server.getLocalPort();
            describe("s4.serverInetAddress", server.getInetAddress());
            describeIsa("s4.serverLocalSocketAddress",
                    (InetSocketAddress) server.getLocalSocketAddress());
            try (Socket client = new Socket()) {
                client.connect(new InetSocketAddress("127.0.0.1", port), 10_000);
                try (Socket accepted = server.accept()) {
                    describe("s4.acceptedInetAddress", accepted.getInetAddress());
                    describeIsa("s4.acceptedRemote",
                            (InetSocketAddress) accepted.getRemoteSocketAddress());
                    describe("s4.clientLocalAddress", client.getLocalAddress());
                    describeIsa("s4.clientRemote",
                            (InetSocketAddress) client.getRemoteSocketAddress());
                }
            }
        } catch (Throwable t) {
            p("s4.sockets", "THREW " + t.getClass().getName());
        }
        try (ServerSocket server = new ServerSocket(0);
             SocketChannel ch = SocketChannel.open()) {
            ch.connect(new InetSocketAddress("127.0.0.1", server.getLocalPort()));
            try (Socket accepted = server.accept()) {
                SocketAddress remote = ch.getRemoteAddress();
                describeIsa("s4.channelRemote", (InetSocketAddress) remote);
                describeIsa("s4.channelLocal", (InetSocketAddress) ch.getLocalAddress());
            }
        } catch (Throwable t) {
            p("s4.channel", "THREW " + t.getClass().getName());
        }
        try (DatagramSocket ds = new DatagramSocket()) {
            describe("s4.datagramLocalAddress", ds.getLocalAddress());
        } catch (Throwable t) {
            p("s4.datagram", "THREW " + t.getClass().getName());
        }
    }

    // ---- 5. The JDK's error behaviour ------------------------------------

    private static void section5() {
        // Catch Throwable, not the EXPECTED type: a probe whose catch clause
        // names only the answer it expects stops measuring and starts
        // asserting — CratonVM threw IllegalArgumentException here, which
        // escaped a `catch (UnknownHostException)` and aborted the run two
        // sections early, hiding everything after it.
        try {
            InetAddress.getByAddress(new byte[] {1, 2, 3});
            p("s5.badLength", "noThrow");
        } catch (Throwable t) {
            p("s5.badLength", t.getClass().getName());
        }
        try {
            InetAddress.getByName("nonexistent.invalid");
            p("s5.unknownHost", "noThrow");
        } catch (Throwable t) {
            p("s5.unknownHost", t.getClass().getName());
        }
        // A named address is NOT resolved — the bytes are taken verbatim, so a
        // bogus name is fine and is remembered.
        try {
            InetAddress named =
                    InetAddress.getByAddress("nonexistent.invalid", new byte[] {1, 2, 3, 4});
            p("s5.namedBogusHostKept",
                    named.toString().equals("nonexistent.invalid/1.2.3.4"));
        } catch (Throwable t) {
            p("s5.namedBogusHostKept", "THREW " + t.getClass().getName());
        }
        p("s5.isInet4", InetAddress.getLoopbackAddress() instanceof Inet4Address);
    }

    // ---- 6. getHostName(): the mutating observer, on its own objects ------

    /// Every address here is built fresh and used exactly once, because
    /// `getHostName()` writes its answer back into the holder.
    ///
    /// Only relations are reported. Whether a reverse lookup SUCCEEDS is a
    /// property of the machine's resolver (on this box `127.0.0.1` resolves to
    /// a Docker-installed name), so `getHostNameIsIp` is not a contract — the
    /// contract is the pair of invariants after it: a name that was already
    /// present is returned verbatim and never replaced, and the call never
    /// returns null.
    private static void section6() throws Exception {
        InetAddress lit = InetAddress.getByName("127.0.0.1");
        p("s6.literal.nameAbsentBefore", !hostNamePresent(lit));
        p("s6.literal.getHostNameNonNull", lit.getHostName() != null);
        p("s6.literal.namePresentAfter", hostNamePresent(lit));

        InetAddress named = InetAddress.getByAddress("myhost.example", new byte[] {1, 2, 3, 4});
        p("s6.named.getHostNameIsTheGivenName", named.getHostName().equals("myhost.example"));
        p("s6.named.unchangedAfter", named.toString().equals("myhost.example/1.2.3.4"));

        InetAddress wildcard = new InetSocketAddress(0).getAddress();
        p("s6.wildcard.getHostNameIsIp",
                wildcard.getHostName().equals(wildcard.getHostAddress()));
        p("s6.wildcard.unchangedAfter", wildcard.toString().equals("0.0.0.0/0.0.0.0"));

        // An address with no name and no reverse mapping must fall back to the
        // literal rather than null or empty.
        InetAddress bytes = InetAddress.getByAddress(new byte[] {(byte) 203, 0, 113, 7});
        String hn = bytes.getHostName();
        p("s6.bytes.getHostNameNonEmpty", hn != null && !hn.isEmpty());

        InetAddress loop = InetAddress.getLoopbackAddress();
        p("s6.loopback.getHostNameIsNotIp", !loop.getHostName().equals(loop.getHostAddress()));
    }

    public static void main(String[] args) throws Exception {
        section1();
        section2();
        section3();
        section4();
        section5();
        section6();
        System.out.println("INETADDRESS_HOSTNAME_PROBE_DONE");
    }
}
