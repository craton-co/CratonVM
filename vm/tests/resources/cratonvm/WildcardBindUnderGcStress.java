package cratonvm;

import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.Locale;

/// Regression fixture for the wildcard-`InetAddress` contract that
/// `new ServerSocket(0)` depends on, exercised on a COLD VM under GC stress.
///
/// Two cross-call GC-safety defects (both fixed 2026-08-04) made this fail:
///
///  1. `native-builtins`' `InetSocketAddress`/`InetAddress` construction path
///     held freshly-allocated objects as bare `ObjectRef` locals across
///     allocating calls. On a cold VM the first such call also initialises the
///     `java.net.InetAddress` hierarchy — a lot of Java, and therefore a
///     reliable moving young collection — so the wildcard address was written
///     into a vacated from-space copy. `getAddress()` then answered **null**
///     while `isUnresolved()` still answered **false**, which is precisely the
///     pair that lets `ServerSocket.bind` walk past its own unresolved-address
///     guard and NPE inside `sun.nio.ch.Net.bind` (`addr.isLinkLocalAddress()`).
///
///  2. `Class.getEnumConstants()` copied out of `$VALUES` through a reference
///     captured BEFORE it allocated the destination array, so after a
///     relocation it copied nulls. `Enum.valueOf` scans that array, so
///     `jdk.internal.util.OperatingSystem.<clinit>` failed with
///     `No enum constant WINDOWS` inside `sun.nio.ch.Net.<clinit>` —
///     `new ServerSocket(0)` died with `ExceptionInInitializerError`.
///
/// EVERYTHING here must run on a cold VM: the ORDER of the checks is
/// load-bearing. Resolving any `InetAddress` first warms the hierarchy and
/// hides defect 1 completely (verified — a `getByAddress` call before the
/// first `new InetSocketAddress(0)` made the pre-fix binary pass).
///
/// Passes on HotSpot.
public class WildcardBindUnderGcStress {

    private static void check(boolean ok, String what) {
        if (!ok) {
            throw new AssertionError("FAILED: " + what);
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- 1. The cold wildcard. Nothing above this line may touch
        //         java.net, or the defect this pins is warmed away.
        InetSocketAddress cold = new InetSocketAddress(0);
        InetAddress wildcard = cold.getAddress();
        check(wildcard != null, "new InetSocketAddress(0).getAddress() is null on a cold VM");
        check(wildcard.isAnyLocalAddress(), "the cold wildcard is not the any-local address");
        check(!wildcard.isLinkLocalAddress(),
                "the cold wildcard reports link-local (Net.bind's first test)");
        // PAIRED property: the JDK defines isUnresolved() as addr == null.
        // These two disagreeing is the actual defect signature — a lone null
        // getAddress() would have been caught by bind's own guard.
        check(cold.isUnresolved() == (cold.getAddress() == null),
                "isUnresolved()/getAddress() disagree: "
                        + cold.isUnresolved() + " vs " + cold.getAddress());

        // Same address through the constructor `ServerSocket(int,int,InetAddress)`
        // actually uses for a null bindAddr.
        InetSocketAddress viaNull = new InetSocketAddress((InetAddress) null, 0);
        check(viaNull.getAddress() != null,
                "new InetSocketAddress((InetAddress) null, 0).getAddress() is null");
        check(viaNull.isUnresolved() == (viaNull.getAddress() == null),
                "isUnresolved()/getAddress() disagree for the null-InetAddress ctor");

        // ---- 2. `Enum.valueOf` after a relocation. `OperatingSystem.<clinit>`
        //         is the JDK witness reached from `Net.<clinit>` below; this
        //         is the same shape with an enum we own, so a failure here
        //         names the mechanism rather than a JDK-internal symptom.
        check(Mode.valueOf("windows".toUpperCase(Locale.ROOT)) == Mode.WINDOWS,
                "Enum.valueOf lost a constant after a collection");
        check(Mode.class.getEnumConstants().length == Mode.values().length,
                "getEnumConstants() and values() disagree on length");
        for (Mode m : Mode.class.getEnumConstants()) {
            check(m != null, "getEnumConstants() copied a null out of $VALUES");
        }

        // ---- 3. The doc's headline: this is the line that threw.
        try (ServerSocket server = new ServerSocket(0)) {
            check(server.isBound(), "new ServerSocket(0) is not bound");
            check(server.getLocalPort() > 0, "new ServerSocket(0) got no ephemeral port");
            InetAddress bound = server.getInetAddress();
            check(bound != null, "a bound ServerSocket reports a null InetAddress");
            check(bound.isAnyLocalAddress(),
                    "new ServerSocket(0) did not bind the wildcard: " + bound);

            // A bind that does not actually listen is not a bind.
            int port = server.getLocalPort();
            try (Socket client = new Socket()) {
                client.connect(new InetSocketAddress("127.0.0.1", port), 10_000);
                try (Socket accepted = server.accept()) {
                    accepted.getOutputStream().write(0x41);
                    accepted.getOutputStream().flush();
                    check(client.getInputStream().read() == 0x41,
                            "round trip over the wildcard-bound socket failed");
                }
            }
        }

        // ---- 4. Unbound socket + bind(null) — "ephemeral port on the
        //         wildcard address", the other route into the same code.
        try (ServerSocket server = new ServerSocket()) {
            check(!server.isBound(), "a fresh ServerSocket() claims to be bound");
            check(server.getLocalPort() == -1,
                    "an unbound ServerSocket must report getLocalPort() == -1");
            server.bind(null);
            check(server.isBound(), "bind(null) did not bind");
            check(server.getLocalPort() > 0, "bind(null) got no ephemeral port");
            check(server.getInetAddress() != null, "bind(null) left a null InetAddress");
        }

        // ---- 5. The JDK's ERROR behaviour, which a lenient re-implementation
        //         silently loses.
        try {
            new InetSocketAddress(-1);
            throw new AssertionError("FAILED: new InetSocketAddress(-1) did not throw");
        } catch (IllegalArgumentException expected) {
            // as HotSpot
        }
        try {
            new InetSocketAddress(65536);
            throw new AssertionError("FAILED: new InetSocketAddress(65536) did not throw");
        } catch (IllegalArgumentException expected) {
            // as HotSpot
        }
        try {
            new InetSocketAddress((InetAddress) null, -1);
            throw new AssertionError(
                    "FAILED: new InetSocketAddress((InetAddress) null, -1) did not throw");
        } catch (IllegalArgumentException expected) {
            // as HotSpot
        }

        System.out.println("WILDCARD_BIND_UNDER_GC_STRESS_OK");
    }

    enum Mode {
        LINUX, MACOS, WINDOWS, AIX
    }
}
