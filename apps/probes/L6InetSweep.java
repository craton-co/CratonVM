import java.net.*;
import java.util.*;

/** L6 — `java.net.InetAddress` (18 rows), `Inet4Address` (10), `Inet6Address`
 *  (10) and `InetSocketAddress`.
 *
 *  The lane page flags this family as the opposite of `URI` and says why: its
 *  answers depend on the HOST's resolver, so a row that prints a resolved
 *  address is not reproducible and is worthless as a differential. **Nothing
 *  here performs a name lookup.** No `getHostName`, no `getCanonicalHostName`,
 *  no `getByName` of anything but a numeric literal, no `getAllByName`, no
 *  `isReachable`, no `getLocalHost`.
 *
 *  What is left is exactly the part a native can get wrong on its own:
 *  textual-form parsing (which is stricter than most implementations expect —
 *  `1.2.3` is a valid IPv4 literal and `01.2.3.4` is not, on a modern JDK),
 *  `getByAddress` round-trips and its length validation, the address-class
 *  predicates, IPv6 textual compression, the v4-mapped and v4-compatible
 *  embeddings, scope ids, and `equals`/`hashCode` over all of it.
 */
public class L6InetSweep {
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

    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder sb = new StringBuilder();
        for (byte b : a) sb.append(String.format("%02x", b));
        return sb.toString();
    }

    /** Every predicate of one address, so a fix to one that breaks another shows.
     *
     *  The row COUNT is FIXED whether the parse succeeded or not. A probe whose
     *  successful branch prints more lines than its failing one makes `diff`
     *  report the shift as ordinary differing rows for the rest of the file,
     *  and the operations page's rule — check the row count before reading the
     *  diff — then fires on every run for a reason that is not a crash. When
     *  the parse failed, every row carries the SAME refusal text, so the
     *  divergence is still visible and still one row per question.
     */
    static void preds(String tag, InetAddress a, String err) {
        String[] labels = {
            "class", "hostAddress", "toString", "address", "anyLocal", "loopback",
            "linkLocal", "siteLocal", "mcGlobal", "mcNodeLocal", "mcLinkLocal",
            "mcSiteLocal", "mcOrgLocal", "multicast", "hashCode",
            "v4compat", "scopeId", "scopedIface", "isInet4", "isInet6",
        };
        for (String label : labels) {
            if (a == null) { p(tag + " " + label, err); continue; }
            final InetAddress x = a;
            tv(tag + " " + label, () -> {
                switch (label) {
                    case "class": return x.getClass().getName();
                    case "hostAddress": return x.getHostAddress();
                    case "toString": return x.toString();
                    case "address": return hex(x.getAddress());
                    case "anyLocal": return x.isAnyLocalAddress();
                    case "loopback": return x.isLoopbackAddress();
                    case "linkLocal": return x.isLinkLocalAddress();
                    case "siteLocal": return x.isSiteLocalAddress();
                    case "mcGlobal": return x.isMCGlobal();
                    case "mcNodeLocal": return x.isMCNodeLocal();
                    case "mcLinkLocal": return x.isMCLinkLocal();
                    case "mcSiteLocal": return x.isMCSiteLocal();
                    case "mcOrgLocal": return x.isMCOrgLocal();
                    case "multicast": return x.isMulticastAddress();
                    case "hashCode": return x.hashCode();
                    case "isInet4": return x instanceof Inet4Address;
                    case "isInet6": return x instanceof Inet6Address;
                    case "v4compat":
                        return x instanceof Inet6Address
                            ? String.valueOf(((Inet6Address) x).isIPv4CompatibleAddress())
                            : "n/a";
                    case "scopeId":
                        return x instanceof Inet6Address
                            ? String.valueOf(((Inet6Address) x).getScopeId()) : "n/a";
                    case "scopedIface":
                        return x instanceof Inet6Address
                            ? String.valueOf(((Inet6Address) x).getScopedInterface()) : "n/a";
                    default: return "unhandled";
                }
            });
        }
    }

    static void lit(String tag, String spec) {
        InetAddress a = null;
        String err = null;
        try { a = InetAddress.getByName(spec); }
        catch (Throwable e) { err = "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()); }
        preds(tag, a, err);
    }

    public static void main(String[] args) throws Exception {
        // ---- the literals a modern JDK accepts, and the near-misses it does not
        lit("v4", "127.0.0.1");
        lit("v4zero", "0.0.0.0");
        lit("v4bcast", "255.255.255.255");
        lit("v4mc", "224.0.0.1");
        lit("v4mcGlobal", "224.0.1.1");
        lit("v4site10", "10.1.2.3");
        lit("v4site172", "172.16.0.1");
        lit("v4site192", "192.168.0.1");
        lit("v4link169", "169.254.1.1");
        lit("v4leadingZero", "01.2.3.4");
        lit("v4threePart", "1.2.3");
        lit("v4twoPart", "1.2");
        lit("v4onePart", "16909060");
        lit("v4hex", "0x7f.0.0.1");
        lit("v4trailingDot", "127.0.0.1.");
        lit("v4empty", "");
        lit("v4null-ish", "   ");
        lit("v4tooBig", "256.1.1.1");
        lit("v4neg", "-1.1.1.1");
        lit("v6loop", "::1");
        lit("v6any", "::");
        lit("v6full", "2001:0db8:0000:0000:0000:0000:0002:0001");
        lit("v6compressed", "2001:db8::2:1");
        lit("v6bracket", "[::1]");
        lit("v6mapped", "::ffff:127.0.0.1");
        lit("v6mappedHex", "::ffff:7f00:1");
        lit("v6compat", "::127.0.0.1");
        lit("v6linkLocal", "fe80::1");
        lit("v6siteLocal", "fec0::1");
        lit("v6uniqueLocal", "fc00::1");
        lit("v6mcNode", "ff01::1");
        lit("v6mcLink", "ff02::1");
        lit("v6mcSite", "ff05::1");
        lit("v6mcOrg", "ff08::1");
        lit("v6mcGlobal", "ff0e::1");
        lit("v6doubleCompress", "1::2::3");
        lit("v6tooMany", "1:2:3:4:5:6:7:8:9");
        lit("v6tooFew", "1:2:3:4:5:6:7");
        lit("v6badGroup", "1:2:3:4:5:6:7:zzzz");

        // ---- getByAddress: length validation, and the null/named forms
        tv("byAddress len0", () -> InetAddress.getByAddress(new byte[0]).toString());
        tv("byAddress len3", () -> InetAddress.getByAddress(new byte[3]).toString());
        tv("byAddress len4", () -> InetAddress.getByAddress(new byte[] {1, 2, 3, 4}).getHostAddress());
        tv("byAddress len5", () -> InetAddress.getByAddress(new byte[5]).toString());
        tv("byAddress len15", () -> InetAddress.getByAddress(new byte[15]).toString());
        tv("byAddress len16", () -> InetAddress.getByAddress(new byte[16]).getHostAddress());
        tv("byAddress len17", () -> InetAddress.getByAddress(new byte[17]).toString());
        tv("byAddress null", () -> InetAddress.getByAddress((byte[]) null).toString());
        tv("byAddress named v4", () -> InetAddress.getByAddress("h.example", new byte[] {8, 8, 4, 4}).toString());
        tv("byAddress named null host", () -> InetAddress.getByAddress(null, new byte[] {8, 8, 4, 4}).toString());
        tv("byAddress named class", () -> InetAddress.getByAddress("h", new byte[] {1, 2, 3, 4}).getClass().getName());
        tv("byAddress v4mapped-16 class", () -> {
            byte[] b = new byte[16];
            b[10] = (byte) 0xff; b[11] = (byte) 0xff;
            b[12] = 127; b[15] = 1;
            InetAddress a = InetAddress.getByAddress(b);
            return a.getClass().getName() + " " + a.getHostAddress();
        });
        tv("byAddress copies input", () -> {
            byte[] b = {1, 2, 3, 4};
            InetAddress a = InetAddress.getByAddress(b);
            b[0] = 9;
            return a.getHostAddress();
        });
        tv("getAddress returns copy", () -> {
            InetAddress a = InetAddress.getByAddress(new byte[] {1, 2, 3, 4});
            a.getAddress()[0] = 9;
            return a.getHostAddress();
        });

        // ---- Inet6Address.getByAddress with an explicit scope
        tv("v6 byAddress scope 0", () -> Inet6Address.getByAddress("h", new byte[16], 0).getHostAddress());
        tv("v6 byAddress scope 7", () -> Inet6Address.getByAddress("h", new byte[16], 7).getHostAddress());
        tv("v6 byAddress scope 7 scopeId", () -> Inet6Address.getByAddress("h", new byte[16], 7).getScopeId());
        tv("v6 byAddress wrong len", () -> Inet6Address.getByAddress("h", new byte[4], 0).getHostAddress());
        tv("v6 byAddress null addr", () -> Inet6Address.getByAddress("h", null, 0).getHostAddress());

        // ---- the loopback constants, which are fixed and need no resolver
        preds("loopbackConst", InetAddress.getLoopbackAddress(), null);
        tv("loopback stable", () -> InetAddress.getLoopbackAddress().equals(InetAddress.getLoopbackAddress()));

        // ---- equals / hashCode across the forms
        InetAddress v4 = InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        InetAddress v4named = InetAddress.getByAddress("other", new byte[] {127, 0, 0, 1});
        InetAddress v6 = InetAddress.getByName("::1");
        byte[] mapped = new byte[16];
        mapped[10] = (byte) 0xff; mapped[11] = (byte) 0xff; mapped[12] = 127; mapped[15] = 1;
        InetAddress v6m = InetAddress.getByAddress(mapped);
        p("eq v4 v4named (host ignored)", v4.equals(v4named));
        p("hash v4 v4named", v4.hashCode() == v4named.hashCode());
        p("eq v4 v6mapped", v4.equals(v6m));
        p("eq v6mapped v4", v6m.equals(v4));
        p("eq v4 v6", v4.equals(v6));
        p("eq v4 null", v4.equals(null));
        p("eq v4 string", v4.equals("127.0.0.1"));
        p("v6mapped class", v6m.getClass().getName());
        p("v6mapped hostAddress", v6m.getHostAddress());

        // ---- InetSocketAddress: port validation and the unresolved form
        tv("isa port -1", () -> new InetSocketAddress(-1).toString());
        tv("isa port 65536", () -> new InetSocketAddress(65536).toString());
        tv("isa port 0", () -> new InetSocketAddress(0).getPort());
        tv("isa addr+port", () -> new InetSocketAddress(v4, 80).toString());
        tv("isa null addr", () -> new InetSocketAddress((InetAddress) null, 80).toString());
        tv("isa unresolved", () -> {
            InetSocketAddress a = InetSocketAddress.createUnresolved("no.such.host.invalid", 80);
            return a.isUnresolved() + " " + a.getPort() + " " + a.getHostString()
                 + " " + (a.getAddress() == null) + " " + a.toString();
        });
        tv("isa unresolved bad port", () -> InetSocketAddress.createUnresolved("h", -1).toString());
        tv("isa unresolved null host", () -> InetSocketAddress.createUnresolved(null, 80).toString());
        tv("isa literal host", () -> {
            InetSocketAddress a = new InetSocketAddress("127.0.0.1", 8080);
            return a.isUnresolved() + " " + a.getHostString() + " " + a.getPort()
                 + " " + a.getAddress().getHostAddress();
        });
        tv("isa equals", () -> new InetSocketAddress(v4, 80).equals(new InetSocketAddress(v4, 80)));
        tv("isa hash", () -> new InetSocketAddress(v4, 80).hashCode() == new InetSocketAddress(v4, 80).hashCode());
        tv("isa unresolved equals", () -> InetSocketAddress.createUnresolved("h", 1)
                .equals(InetSocketAddress.createUnresolved("h", 1)));
        tv("isa unresolved != resolved", () -> InetSocketAddress.createUnresolved("127.0.0.1", 80)
                .equals(new InetSocketAddress("127.0.0.1", 80)));

        // ---- NetworkInterface: only the structural questions. Which interfaces
        // exist is a property of the host, so nothing here prints a name or an
        // address; only the shape of the refusals and the null contract.
        tv("nif byName absent", () -> String.valueOf(NetworkInterface.getByName("no-such-nif-0")));
        tv("nif byName null", () -> String.valueOf(NetworkInterface.getByName(null)));
        tv("nif byIndex 0", () -> String.valueOf(NetworkInterface.getByIndex(0)));
        tv("nif byIndex -1", () -> String.valueOf(NetworkInterface.getByIndex(-1)));
        tv("nif byInetAddress null", () -> String.valueOf(NetworkInterface.getByInetAddress(null)));
        tv("nif loopback present", () -> {
            NetworkInterface n = NetworkInterface.getByInetAddress(InetAddress.getLoopbackAddress());
            return n != null && n.isLoopback();
        });

        System.out.println("rows " + rows);
        System.out.println("DONE L6InetSweep");
    }
}
