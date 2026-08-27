import java.net.*;
import java.util.*;

/** `java.net.InetAddress` and its two implementation types, aimed by the survey's
 *  own prior: `native-builtins/src/inet_address.rs` opens by saying
 *
 *      `java.net.Inet4AddressImpl` -- lookupAllHostAddr, getHostByAddr, ...
 *      `java.net.Inet6AddressImpl` -- SAME SURFACE, IPv6-flavoured.
 *
 *  Every defect this survey has found sat in a family whose registrar carried a
 *  stated justification that had drifted from its code, and "same surface" is
 *  that claim in three words. So this asks the two impls the questions where a
 *  v4 and a v6 address must answer DIFFERENTLY -- address length, the
 *  any-local and loopback constants, IPv4-mapped collapse, scope ids, the
 *  textual forms, and which concrete class comes back.
 *
 *  DETERMINISM: no DNS. Every address is a literal or the loopback constant.
 *  A numeric address's `getHostName` is never printed -- it can trigger a
 *  reverse lookup whose answer belongs to the resolver, not to the VM. The one
 *  name looked up is `localhost`, and only the SHAPE of the answer is printed.
 */
public class InetFamilySweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }
    static String hex(byte[] b) {
        if (b == null) return "null";
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }

    /** Everything an address can be asked that does not consult a resolver. */
    static void shape(String tag, InetAddress a) {
        p(tag + " class", a.getClass().getName());
        p(tag + " hostAddress", a.getHostAddress());
        p(tag + " address bytes", hex(a.getAddress()));
        p(tag + " address length", a.getAddress().length);
        p(tag + " isAnyLocalAddress", a.isAnyLocalAddress());
        p(tag + " isLoopbackAddress", a.isLoopbackAddress());
        p(tag + " isLinkLocalAddress", a.isLinkLocalAddress());
        p(tag + " isSiteLocalAddress", a.isSiteLocalAddress());
        p(tag + " isMulticastAddress", a.isMulticastAddress());
        p(tag + " isMCGlobal", a.isMCGlobal());
        p(tag + " isMCNodeLocal", a.isMCNodeLocal());
        p(tag + " isMCLinkLocal", a.isMCLinkLocal());
        p(tag + " isMCSiteLocal", a.isMCSiteLocal());
        p(tag + " isMCOrgLocal", a.isMCOrgLocal());
        p(tag + " toString", a.toString());
        p(tag + " equals self", a.equals(a));
        p(tag + " hashCode stable", a.hashCode() == a.hashCode());
        if (a instanceof Inet6Address) {
            Inet6Address s = (Inet6Address) a;
            p(tag + " v6 scopeId", s.getScopeId());
            p(tag + " v6 scopedInterface", s.getScopedInterface());
            p(tag + " v6 isIPv4CompatibleAddress", s.isIPv4CompatibleAddress());
        }
    }

    static void literals() throws Exception {
        // ---- v4 literals, one per predicate the two impls must split on ----
        String[] v4 = {
            "127.0.0.1",        // loopback
            "0.0.0.0",          // any-local
            "10.1.2.3",         // site-local
            "169.254.7.7",      // link-local
            "224.0.0.1",        // multicast, link-local scope
            "224.0.1.1",        // multicast, global scope
            "239.192.0.1",      // multicast, org-local
            "239.255.0.1",      // multicast, site-local
            "1.2.3.4",          // plain unicast
            "255.255.255.255",  // broadcast
        };
        for (String s : v4) shape("[v4 " + s + "]", InetAddress.getByName(s));

        // ---- v6 literals ---------------------------------------------------
        String[] v6 = {
            "::1",              // loopback
            "::",               // any-local
            "fe80::1",          // link-local
            "fec0::1",          // site-local (deprecated, still classified)
            "ff01::1",          // multicast node-local
            "ff02::1",          // multicast link-local
            "ff05::1",          // multicast site-local
            "ff08::1",          // multicast org-local
            "ff0e::1",          // multicast global
            "2001:db8::1",      // plain unicast
            "::ffff:1.2.3.4",   // IPv4-MAPPED: must collapse to an Inet4Address
            "::1.2.3.4",        // IPv4-COMPATIBLE: stays v6
        };
        for (String s : v6) shape("[v6 " + s + "]", InetAddress.getByName(s));
    }

    static void byAddress() throws Exception {
        byte[] four = {1, 2, 3, 4};
        byte[] sixteen = new byte[16];
        sixteen[15] = 1;                              // ::1
        byte[] mapped = new byte[16];
        mapped[10] = (byte) 0xff; mapped[11] = (byte) 0xff;
        mapped[12] = 1; mapped[13] = 2; mapped[14] = 3; mapped[15] = 4;

        p("getByAddress 4 class", InetAddress.getByAddress(four).getClass().getName());
        p("getByAddress 4 text", InetAddress.getByAddress(four).getHostAddress());
        p("getByAddress 16 class", InetAddress.getByAddress(sixteen).getClass().getName());
        p("getByAddress 16 text", InetAddress.getByAddress(sixteen).getHostAddress());
        // A 16-byte v4-mapped array is the one case getByAddress must COLLAPSE.
        p("getByAddress mapped class", InetAddress.getByAddress(mapped).getClass().getName());
        p("getByAddress mapped text", InetAddress.getByAddress(mapped).getHostAddress());
        // A literal name supplied by the caller is NOT a resolver answer, so it
        // is safe to print: getHostName must give back exactly what was passed.
        p("getByAddress named 4", InetAddress.getByAddress("h", four).getHostName());
        t("getByAddress 5 bytes", () -> InetAddress.getByAddress(new byte[5]));
        t("getByAddress 0 bytes", () -> InetAddress.getByAddress(new byte[0]));
        t("getByAddress null", () -> InetAddress.getByAddress((byte[]) null));
        p("Inet6 getByAddress scoped null iface",
          Inet6Address.getByAddress("h", sixteen, (NetworkInterface) null).getHostAddress());
        p("Inet6 getByAddress scope 0",
          Inet6Address.getByAddress("h", sixteen, 0).getHostAddress());
        p("Inet6 getByAddress scope 7",
          Inet6Address.getByAddress("h", sixteen, 7).getHostAddress());
        p("Inet6 getByAddress scope 7 scopeId",
          Inet6Address.getByAddress("h", sixteen, 7).getScopeId());
        t("Inet6 getByAddress 4 bytes", () -> Inet6Address.getByAddress("h", four, 0));

        // getCanonicalHostName vs getHostName, asked as a PROPERTY so the
        // resolver cannot get into the diff.
        //
        // `getByAddress(name, addr)` stores the caller's literal name, so
        // `getHostName()` must give it back verbatim. `getCanonicalHostName()`
        // must IGNORE that name and do its own reverse lookup -- and whatever
        // the resolver answers, it is not the string "h": either a real PTR
        // name, or the numeric literal when the lookup fails. So on HotSpot
        // this row is FALSE for every resolver outcome, and it is TRUE exactly
        // on a VM that routes both methods to one shared body.
        //
        // `net_phase_e.rs` registers getHostName and getCanonicalHostName on
        // BOTH Inet4Address and Inet6Address, and both bodies call
        // `inet_addr_host_name_value` -- which is what this row is aimed at.
        // 192.0.2.1 is RFC 5737 TEST-NET-1, reserved for documentation.
        byte[] testnet = {(byte) 192, 0, 2, 1};
        InetAddress named4 = InetAddress.getByAddress("h", testnet);
        p("v4 getHostName is the supplied name", named4.getHostName().equals("h"));
        p("v4 canonical is the supplied name", named4.getCanonicalHostName().equals("h"));
        byte[] doc6 = new byte[16];
        doc6[0] = 0x20; doc6[1] = 0x01; doc6[2] = 0x0d; doc6[3] = (byte) 0xb8;
        doc6[15] = 1;                                  // 2001:db8::1, RFC 3849
        InetAddress named6 = InetAddress.getByAddress("h6", doc6);
        p("v6 getHostName is the supplied name", named6.getHostName().equals("h6"));
        p("v6 canonical is the supplied name", named6.getCanonicalHostName().equals("h6"));
    }

    static void loopbackAndAny() throws Exception {
        InetAddress lb = InetAddress.getLoopbackAddress();
        shape("[loopbackAddress]", lb);
        // anyLocalAddress()/loopbackAddress() are the clearest place the
        // "same surface" claim is false: 0.0.0.0 vs :: and 127.0.0.1 vs ::1.
        p("wildcard v4 isAnyLocal", new InetSocketAddress(0).getAddress().isAnyLocalAddress());
        p("wildcard v4 class", new InetSocketAddress(0).getAddress().getClass().getName());
        p("unresolved socket addr", InetSocketAddress.createUnresolved("h.example", 80).toString());
        p("unresolved isUnresolved", InetSocketAddress.createUnresolved("h.example", 80).isUnresolved());
        InetSocketAddress isa = new InetSocketAddress(InetAddress.getByName("1.2.3.4"), 80);
        p("socket addr toString", isa.toString());
        p("socket addr port", isa.getPort());
        p("socket addr equals copy",
          isa.equals(new InetSocketAddress(InetAddress.getByName("1.2.3.4"), 80)));
        InetSocketAddress v6isa = new InetSocketAddress(InetAddress.getByName("::1"), 80);
        p("v6 socket addr toString", v6isa.toString());
    }

    static void equality() throws Exception {
        InetAddress a4 = InetAddress.getByName("127.0.0.1");
        InetAddress a6 = InetAddress.getByName("::1");
        InetAddress mapped = InetAddress.getByName("::ffff:127.0.0.1");
        p("v4 equals v6 loopback", a4.equals(a6));
        // A v4-mapped literal collapses to Inet4Address, so this must be TRUE --
        // and it is the row that fails if only one of the two impls collapses.
        p("v4 equals v4-mapped", a4.equals(mapped));
        p("v4-mapped class", mapped.getClass().getName());
        p("v4 hash == v4-mapped hash", a4.hashCode() == mapped.hashCode());
        Set<InetAddress> set = new HashSet<>();
        set.add(a4); set.add(a6); set.add(mapped);
        p("set of {v4, v6, mapped} size", set.size());
    }

    static void refusals() {
        t("getByName 1.2.3.4.5", () -> InetAddress.getByName("1.2.3.4.5"));
        t("getByName 256.1.1.1", () -> InetAddress.getByName("256.1.1.1"));
        t("getByName ::::", () -> InetAddress.getByName("::::"));
        t("getByName [::1]", () -> InetAddress.getByName("[::1]"));
        t("getByName [::1 unclosed", () -> InetAddress.getByName("[::1"));
        // An EMPTY or null host is documented to give the loopback address.
        p("getByName empty", loopbackOrThrow(""));
        p("getByName null", loopbackOrThrow(null));
    }
    static String loopbackOrThrow(String h) {
        try { return InetAddress.getByName(h).getHostAddress(); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    /** localhost is the ONE name looked up, and only its shape is printed. */
    static void localhostShape() {
        try {
            InetAddress[] all = InetAddress.getAllByName("localhost");
            p("localhost count >= 1", all.length >= 1);
            boolean allLoopback = true, anyV4 = false;
            for (InetAddress a : all) {
                if (!a.isLoopbackAddress()) allLoopback = false;
                if (a instanceof Inet4Address) anyV4 = true;
            }
            p("localhost all loopback", allLoopback);
            p("localhost has a v4", anyV4);
            p("localhost getByName is loopback",
              InetAddress.getByName("localhost").isLoopbackAddress());
        } catch (Throwable e) {
            p("localhost", "THREW " + e.getClass().getName());
        }
    }

    public static void main(String[] a) throws Exception {
        literals();
        byAddress();
        loopbackAndAny();
        equality();
        refusals();
        localhostShape();
        System.out.println("DONE InetFamilySweep");
    }
}
