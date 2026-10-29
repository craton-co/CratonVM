package cratonvm;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;

/// Pins which `InetAddress` constructions remember a `hostName` and which do
/// not, because `InetAddress.toString()` is
/// `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()` —
/// so a literal-derived address prints `/127.0.0.1` and a named one prints
/// `localhost/127.0.0.1`.
///
/// **The distinction is NOT "host equals ip".** `InetAddress.getByName("0.0.0.0")`
/// has no name, while the wildcard singleton behind `new InetSocketAddress(0)`
/// genuinely carries `hostName = "0.0.0.0"` — same IP, opposite answer. Both
/// rows are asserted below precisely so a future "simplification" that keys off
/// `host == ip` fails here instead of in an app.
///
/// Everything asserted was recorded from HotSpot 25.0.3+9 first. Reverse DNS is
/// machine-dependent, so `getHostName()` on a NAMELESS address is asserted only
/// to be non-empty, never to equal any particular text.
///
/// Passes on HotSpot.
public class InetAddressHostNameContract {

    private static void eq(String actual, String expected, String what) {
        if (!expected.equals(actual)) {
            throw new AssertionError(
                    "FAILED " + what + ": expected <" + expected + "> got <" + actual + ">");
        }
    }

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError("FAILED: " + what);
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- no hostName: numeric literals and raw bytes ----
        eq(InetAddress.getByName("127.0.0.1").toString(), "/127.0.0.1",
                "getByName(v4 literal)");
        eq(InetAddress.getByName("0.0.0.0").toString(), "/0.0.0.0",
                "getByName(wildcard literal)");
        eq(InetAddress.getByName("169.254.1.1").toString(), "/169.254.1.1",
                "getByName(link-local literal)");
        eq(InetAddress.getByAddress(new byte[] {1, 2, 3, 4}).toString(), "/1.2.3.4",
                "getByAddress(byte[])");
        eq(InetAddress.getByAddress(new byte[] {0, 0, 0, 0}).toString(), "/0.0.0.0",
                "getByAddress(zero byte[])");
        eq(InetAddress.getAllByName("127.0.0.1")[0].toString(), "/127.0.0.1",
                "getAllByName(v4 literal)[0]");

        // ---- hostName present ----
        eq(InetAddress.getByName("localhost").toString(), "localhost/127.0.0.1",
                "getByName(localhost)");
        eq(InetAddress.getLoopbackAddress().toString(), "localhost/127.0.0.1",
                "getLoopbackAddress()");
        eq(InetAddress.getByName(null).toString(), "localhost/127.0.0.1",
                "getByName(null)");
        eq(InetAddress.getByAddress("myhost.example", new byte[] {1, 2, 3, 4}).toString(),
                "myhost.example/1.2.3.4", "getByAddress(name, byte[])");

        // ---- the anti-heuristic rows: host EQUALS ip, yet the name is kept ----
        eq(new InetSocketAddress(0).getAddress().toString(), "0.0.0.0/0.0.0.0",
                "the wildcard singleton keeps its name");
        eq(new InetSocketAddress((InetAddress) null, 0).getAddress().toString(),
                "0.0.0.0/0.0.0.0", "the null-InetAddress ctor yields the named wildcard");

        // ---- InetSocketAddress rendering follows the address ----
        InetSocketAddress fromLiteral = new InetSocketAddress("127.0.0.1", 8080);
        eq(fromLiteral.toString(), "/127.0.0.1:8080", "ISA from a literal");
        eq(fromLiteral.getHostString(), "127.0.0.1",
                "getHostString() falls back to the IP when the address has no name");
        InetSocketAddress fromName = new InetSocketAddress("localhost", 8080);
        eq(fromName.toString(), "localhost/127.0.0.1:8080", "ISA from a name");
        eq(fromName.getHostString(), "localhost", "getHostString() keeps a real name");
        eq(new InetSocketAddress(0).toString(), "0.0.0.0/0.0.0.0:0", "ISA wildcard");
        eq(InetSocketAddress.createUnresolved("nonexistent.invalid", 80).toString(),
                "nonexistent.invalid/<unresolved>:80", "unresolved ISA");

        // ---- a bound ServerSocket reports the NAMED wildcard singleton ----
        try (ServerSocket server = new ServerSocket(0)) {
            eq(server.getInetAddress().toString(), "0.0.0.0/0.0.0.0",
                    "bound ServerSocket.getInetAddress()");
            InetSocketAddress local = (InetSocketAddress) server.getLocalSocketAddress();
            eq(local.getAddress().toString(), "0.0.0.0/0.0.0.0",
                    "bound ServerSocket.getLocalSocketAddress().getAddress()");
        }

        // ---- equals/hashCode ignore the hostName entirely ----
        InetAddress literal = InetAddress.getByName("127.0.0.1");
        InetAddress named = InetAddress.getByName("localhost");
        InetAddress bytes = InetAddress.getByAddress(new byte[] {127, 0, 0, 1});
        InetAddress namedBytes =
                InetAddress.getByAddress("whatever.example", new byte[] {127, 0, 0, 1});
        check(literal.equals(named), "equals() must ignore the hostName (literal vs named)");
        check(literal.equals(bytes), "equals() must ignore the hostName (literal vs bytes)");
        check(literal.equals(namedBytes), "equals() must ignore the hostName (literal vs named bytes)");
        check(literal.hashCode() == named.hashCode()
                        && literal.hashCode() == bytes.hashCode()
                        && literal.hashCode() == namedBytes.hashCode(),
                "hashCode() must ignore the hostName");
        check(new InetSocketAddress(literal, 9).equals(new InetSocketAddress(named, 9)),
                "InetSocketAddress.equals() must ignore the hostName");

        // ---- getHostName(): a name that exists is returned verbatim; a
        //      missing one falls back to something non-empty. The fallback text
        //      is machine-dependent (HotSpot performs a reverse lookup), so only
        //      the invariant is asserted.
        eq(InetAddress.getByAddress("myhost.example", new byte[] {1, 2, 3, 4}).getHostName(),
                "myhost.example", "getHostName() returns a stored name verbatim");
        eq(InetAddress.getLoopbackAddress().getHostName(), "localhost",
                "getHostName() on the loopback singleton");
        String namelessHostName =
                InetAddress.getByAddress(new byte[] {(byte) 203, 0, 113, 7}).getHostName();
        check(namelessHostName != null && !namelessHostName.isEmpty(),
                "getHostName() on a nameless address must not be null or empty");
        eq(new InetSocketAddress(0).getAddress().getHostName(), "0.0.0.0",
                "getHostName() on the wildcard singleton");

        // ---- the address bytes must be unaffected by any of this ----
        check(InetAddress.getByName("127.0.0.1").getHostAddress().equals("127.0.0.1"),
                "getHostAddress() on a literal");
        check(InetAddress.getByAddress(new byte[] {1, 2, 3, 4}).getAddress().length == 4,
                "getAddress() length");

        System.out.println("INETADDRESS_HOSTNAME_CONTRACT_OK");
    }
}
