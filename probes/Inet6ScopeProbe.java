import java.net.Inet4Address;
import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.Enumeration;

/**
 * Differential probe for Inet6Address scope-id handling.
 * Every line is "KEY=VALUE" so a diff against HotSpot is mechanical.
 */
public class Inet6ScopeProbe {

    static final byte[] LINK_LOCAL = {
        (byte) 0xfe, (byte) 0x80, '0', '0', '0', '0', '0', '0',
        '0', '0', '0', '0', '0', '0', '0', '1'
    };
    static final byte[] GLOBAL = {
        0x20, 0x01, 0x0d, (byte) 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1
    };
    static final byte[] V4MAPPED = {
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, (byte) 0xff, (byte) 0xff, 127, 0, 0, 1
    };

    static void p(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    static String hex(byte[] b) {
        if (b == null) return "null";
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // ---- A: scope-id survival on the three-arg factory ----------------
        Inet6Address a0 = Inet6Address.getByAddress(null, LINK_LOCAL, 0);
        Inet6Address a7 = Inet6Address.getByAddress(null, LINK_LOCAL, 7);
        Inet6Address aNeg = Inet6Address.getByAddress(null, LINK_LOCAL, -1);

        // toString BEFORE any getHostName call (HotSpot caches the reverse
        // lookup into holder.hostName, so call order is observable).
        p("A.a0.toString.fresh", a0.toString());
        p("A.a7.toString.fresh", a7.toString());
        p("A.aNeg.toString.fresh", aNeg.toString());

        p("A.a0.class", a0.getClass().getName());
        p("A.a0.isLinkLocal", a0.isLinkLocalAddress());
        p("A.a0.scopeId", a0.getScopeId());
        p("A.a0.hostAddress", a0.getHostAddress());
        p("A.a0.hostName", a0.getHostName());
        p("A.a0.canonicalHostName", a0.getCanonicalHostName());
        p("A.a0.toString.after", a0.toString());
        p("A.a0.addrLen", a0.getAddress().length);
        p("A.a0.addrHex", hex(a0.getAddress()));
        p("A.a0.hashCode", a0.hashCode());
        p("A.a0.scopedInterface", a0.getScopedInterface());

        p("A.a7.scopeId", a7.getScopeId());
        p("A.a7.hostAddress", a7.getHostAddress());
        p("A.a7.hostName", a7.getHostName());
        p("A.a7.toString.after", a7.toString());
        p("A.a7.addrHex", hex(a7.getAddress()));
        p("A.a7.hashCode", a7.hashCode());
        p("A.a0.equals.a7", a0.equals(a7));
        p("A.a7.equals.a0", a7.equals(a0));

        p("A.aNeg.scopeId", aNeg.getScopeId());
        p("A.aNeg.hostAddress", aNeg.getHostAddress());
        p("A.aNeg.toString.after", aNeg.toString());

        // ---- B: named three-arg factory -----------------------------------
        Inet6Address b = Inet6Address.getByAddress("myhost", LINK_LOCAL, 7);
        p("B.hostAddress", b.getHostAddress());
        p("B.hostName", b.getHostName());
        p("B.toString", b.toString());

        // ---- C: non-link-local with a scope -------------------------------
        Inet6Address c = Inet6Address.getByAddress(null, GLOBAL, 3);
        p("C.hostAddress", c.getHostAddress());
        p("C.scopeId", c.getScopeId());
        p("C.toString", c.toString());

        // ---- D: no-scope objects must NOT sprout a suffix ------------------
        InetAddress d1 = InetAddress.getByAddress(GLOBAL);
        p("D.getByAddress.class", d1.getClass().getName());
        p("D.getByAddress.hostAddress", d1.getHostAddress());
        p("D.getByAddress.toString.fresh", d1.toString());
        InetAddress d2 = InetAddress.getByName("::1");
        p("D.getByName.v6.class", d2.getClass().getName());
        p("D.getByName.v6.hostAddress", d2.getHostAddress());
        InetAddress d3 = InetAddress.getByName("127.0.0.1");
        p("D.getByName.v4.class", d3.getClass().getName());
        p("D.getByName.v4.hostAddress", d3.getHostAddress());
        p("D.getByName.v4.toString.fresh", d3.toString());
        InetAddress d4 = InetAddress.getByAddress(V4MAPPED);
        p("D.v4mapped.class", d4.getClass().getName());
        p("D.v4mapped.hostAddress", d4.getHostAddress());
        Inet6Address d5 = Inet6Address.getByAddress(null, V4MAPPED, 0);
        p("D.v4mapped6.class", d5.getClass().getName());
        p("D.v4mapped6.hostAddress", d5.getHostAddress());
        p("D.v4mapped6.addrLen", d5.getAddress().length);

        // ---- E: scope by NetworkInterface ---------------------------------
        // Pick by NAME, not "the first one that is up": getNetworkInterfaces()
        // order is unspecified, and CratonVM cannot reproduce HotSpot's (the
        // JDK's IPv6 pass reads /proc/net/if_inet6, a kernel hash-table walk).
        // Selecting positionally makes this section report the host's
        // interface churn instead of the VM's behaviour.
        NetworkInterface nif = NetworkInterface.getByName("eth0");
        if (nif == null) {
            for (Enumeration<NetworkInterface> e = NetworkInterface.getNetworkInterfaces();
                 e != null && e.hasMoreElements(); ) {
                NetworkInterface n = e.nextElement();
                if (!n.isLoopback() && n.isUp()) { nif = n; break; }
            }
        }
        if (nif == null) {
            p("E.nif", "none");
        } else {
            p("E.nif.name", nif.getName());
            try {
                Inet6Address e1 = Inet6Address.getByAddress(null, LINK_LOCAL, nif);
                p("E.byIf.hostAddress", e1.getHostAddress());
                p("E.byIf.scopeId", e1.getScopeId());
                p("E.byIf.scopedIfaceName",
                  e1.getScopedInterface() == null ? "null" : e1.getScopedInterface().getName());
                p("E.byIf.toString", e1.toString());
            } catch (Throwable t) {
                p("E.byIf.throw", t.getClass().getName() + ":" + t.getMessage());
            }
        }

        // ---- F: textual round-trip ----------------------------------------
        try {
            InetAddress f1 = InetAddress.getByName("fe80::1%1");
            p("F.getByName.scoped.class", f1.getClass().getName());
            p("F.getByName.scoped.hostAddress", f1.getHostAddress());
            p("F.getByName.scoped.scopeId",
              (f1 instanceof Inet6Address) ? ((Inet6Address) f1).getScopeId() : -1);
        } catch (Throwable t) {
            p("F.getByName.scoped.throw", t.getClass().getName());
        }

        // ---- F2: getAllByName keeps the scope too ---------------------------
        try {
            InetAddress[] all = InetAddress.getAllByName("fe80::1%1");
            p("F2.all.len", all.length);
            p("F2.all0.hostAddress", all[0].getHostAddress());
            p("F2.all0.scopeId",
              (all[0] instanceof Inet6Address) ? ((Inet6Address) all[0]).getScopeId() : -1);
        } catch (Throwable t) {
            p("F2.all.throw", t.getClass().getName());
        }

        // ---- G: the round-trip the doc calls the defect --------------------
        String txt = a7.getHostAddress();
        p("G.roundtrip.text", txt);
        p("G.roundtrip.hasScope", txt.contains("%"));

        // ---- H: Inet4Address regression guard ------------------------------
        Inet4Address h = (Inet4Address) InetAddress.getByAddress(new byte[]{10, 1, 2, 3});
        p("H.v4.hostAddress", h.getHostAddress());
        p("H.v4.toString.fresh", h.toString());
        p("H.v4.hostName", h.getHostName());
        p("H.v4.toString.after", h.toString());
        p("H.v4.hashCode", h.hashCode());

        System.out.println("PROBE-DONE");
    }
}
