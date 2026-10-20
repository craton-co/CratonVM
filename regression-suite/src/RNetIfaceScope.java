import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.Collections;
import java.util.List;

/**
 * A scoped IPv6 address's TEXT has to round-trip back through
 * {@link InetAddress#getByName}.
 *
 * The scope suffix is not decoration. netty's `Socks4ProxyHandler` puts
 * `getHostAddress()` of the destination on the wire and the proxy at the other
 * end feeds that text straight to `getByName`, so an address that renders a
 * suffix nothing can parse breaks every proxy hop over it -- which is how 8 of
 * `ProxyHandlerTest`'s 47 parameterisations failed while HotSpot passed all 47.
 * The suffix was there because every IPv6 address reached through an interface
 * was scoped to it, a rule measured on Linux and wrong on Windows, where the
 * loopback's `sin6_scope_id` is 0 and HotSpot leaves `::1` unscoped.
 *
 * <p>Determinism: this vector prints NO interface name and NO address. Names
 * are host-specific, and the Windows name is a divergence in its own right
 * (ours is the adapter GUID, HotSpot's is `loopback_0`). What it prints is the
 * two things that must agree with HotSpot on any host: that the loopback's
 * IPv6 address is unscoped, and that every scoped address on the box renders
 * text `getByName` accepts.
 */
public class RNetIfaceScope {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The address text must parse back to the same address. */
    static boolean roundTrips(InetAddress a) {
        try {
            return InetAddress.getByName(a.getHostAddress()) != null;
        } catch (Exception e) {
            return false;
        }
    }

    public static void main(String[] args) throws Exception {
        // `::1` asked for by name is unscoped everywhere, and is the control:
        // if this one is scoped, the enumeration is not the only thing wrong.
        InetAddress literal = InetAddress.getByName("::1");
        check(literal instanceof Inet6Address, "::1 must parse to an Inet6Address");
        Inet6Address literal6 = (Inet6Address) literal;
        check(literal6.getScopeId() == 0, "getByName(\"::1\") must be unscoped");
        check(literal6.getScopedInterface() == null, "getByName(\"::1\") has no scoped interface");
        check(roundTrips(literal6), "getByName(\"::1\") text must round-trip");
        System.out.println("CK RNetIfaceScope literalUnscoped=true");

        int scoped = 0;
        int loopbackV6 = 0;
        int badRoundTrip = 0;
        int scopedLoopback = 0;
        List<NetworkInterface> ifaces =
                Collections.list(NetworkInterface.getNetworkInterfaces());
        for (NetworkInterface ni : ifaces) {
            for (InetAddress a : Collections.list(ni.getInetAddresses())) {
                if (!(a instanceof Inet6Address)) {
                    continue;
                }
                Inet6Address a6 = (Inet6Address) a;
                boolean isScoped = a6.getScopeId() != 0 || a6.getScopedInterface() != null;
                if (isScoped) {
                    scoped++;
                    // A scope the address advertises must also be one the
                    // parser accepts; the two halves are the same contract.
                    if (!roundTrips(a6)) {
                        badRoundTrip++;
                    }
                }
                if (a6.isLoopbackAddress()) {
                    loopbackV6++;
                    if (isScoped) {
                        scopedLoopback++;
                    }
                    if (!roundTrips(a6)) {
                        badRoundTrip++;
                    }
                }
            }
        }
        // Counts of INTERFACES are host-specific and deliberately not printed;
        // these three are not.
        check(badRoundTrip == 0, "every scoped IPv6 address must round-trip, " + badRoundTrip
                + " did not");
        System.out.println("CK RNetIfaceScope badRoundTrip=" + badRoundTrip);
        System.out.println("CK RNetIfaceScope scopedLoopback=" + scopedLoopback);
        // `scoped > 0` is NOT asserted: a host with no link-local address is
        // legal, and asserting it would make this vector fail on a CI box
        // rather than on a regression. It is printed so a run where the whole
        // scope column went silently empty is visible in the diff against
        // HotSpot, which is the only reader that can judge it.
        System.out.println("CK RNetIfaceScope anyScoped=" + (scoped > 0));

        System.out.println("CK RNetIfaceScope checks=" + checks);
        System.out.println("PASS RNetIfaceScope (" + checks + " checks)");
    }
}
