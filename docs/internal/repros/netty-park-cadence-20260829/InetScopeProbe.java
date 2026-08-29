import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.NetworkInterface;
import java.util.Collections;

/**
 * What a scoped IPv6 loopback address stringifies to, and whether that string
 * round-trips back through InetAddress.getByName.
 *
 * netty's NetUtil.LOCALHOST comes out of NetworkInterface enumeration, and
 * Socks4ProxyHandler puts getHostAddress() of the destination on the wire; the
 * proxy server then feeds that text back to getByName. So a scope suffix that
 * does not round-trip breaks every proxy chain hop.
 */
public final class InetScopeProbe {

    static void show(String label, InetAddress a) {
        if (a == null) {
            System.out.printf("%-46s <null>%n", label);
            return;
        }
        StringBuilder extra = new StringBuilder();
        if (a instanceof Inet6Address) {
            Inet6Address a6 = (Inet6Address) a;
            NetworkInterface sif = a6.getScopedInterface();
            extra.append("  scopeId=").append(a6.getScopeId())
                 .append(" scopedIface=").append(sif == null ? "null" : sif.getName());
        }
        String host = a.getHostAddress();
        String trip;
        try {
            InetAddress back = InetAddress.getByName(host);
            trip = "OK -> " + back.getHostAddress();
        } catch (Exception e) {
            trip = "FAILED: " + e.getClass().getSimpleName() + ": " + e.getMessage();
        }
        System.out.printf("%-46s %s%s%n%-46s   getByName(getHostAddress()) = %s%n",
                          label, host, extra, "", trip);
    }

    public static void main(String[] args) throws Exception {
        show("InetAddress.getLoopbackAddress()", InetAddress.getLoopbackAddress());
        show("InetAddress.getByName(\"::1\")", InetAddress.getByName("::1"));
        show("InetAddress.getByName(\"127.0.0.1\")", InetAddress.getByName("127.0.0.1"));

        for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
            if (!ni.isLoopback()) {
                continue;
            }
            System.out.printf("loopback iface: name=%s display=%s index=%d%n",
                              ni.getName(), ni.getDisplayName(), ni.getIndex());
            for (InetAddress a : Collections.list(ni.getInetAddresses())) {
                show("  " + ni.getName() + " address", a);
            }
        }

        try {
            Class<?> netUtil = Class.forName("io.netty.util.NetUtil");
            InetAddress lh = (InetAddress) netUtil.getField("LOCALHOST").get(null);
            show("io.netty.util.NetUtil.LOCALHOST", lh);
            InetSocketAddress sa = new InetSocketAddress(lh, 4242);
            System.out.printf("%-46s getHostString()=%s toString()=%s%n",
                              "  as InetSocketAddress", sa.getHostString(), sa);
        } catch (Throwable t) {
            System.out.println("NetUtil not on the classpath: " + t);
        }
    }
}
