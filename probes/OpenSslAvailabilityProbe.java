import io.netty.handler.ssl.OpenSsl;

/**
 * `OpenSsl.isAvailable()`, and the reason when it is false.
 *
 * The netty OPENSSL classes read as clean PASSES when it is false — JUnit
 * simply never generates the OPENSSL parameterisations, so
 * `ParameterizedSslHandlerTest` runs 7 of its 63 tests and reports success.
 * Print this before trusting any number out of those classes.
 *
 * Two halves are needed on this host and only the first is obvious:
 *   1. `netty-tcnative-boringssl-static-<ver>-<os>.jar` on the classpath — it
 *      links BoringSSL statically, so the host's OpenSSL version (3.0.13 here)
 *      stops mattering to the 3.2.0-requiring dynamic artifact;
 *   2. the DYNAMIC `netty-tcnative-<ver>-<os>.jar` REMOVED. With both present
 *      netty finds the dynamic one and `isAvailable()` stays false whatever
 *      their order.
 */
public final class OpenSslAvailabilityProbe {
    public static void main(String[] args) {
        System.out.println("OpenSsl.isAvailable = " + OpenSsl.isAvailable());
        Throwable cause = OpenSsl.unavailabilityCause();
        System.out.println("OpenSsl.unavailabilityCause = " + cause);
        for (Throwable t = cause; t != null && t.getCause() != t; t = t.getCause()) {
            System.out.println("  caused by: " + t);
            if (t.getCause() == null) {
                break;
            }
        }
        if (OpenSsl.isAvailable()) {
            System.out.println("OpenSsl.versionString = " + OpenSsl.versionString());
            System.out.println("OpenSsl.supportsKeyManagerFactory = " + OpenSsl.supportsKeyManagerFactory());
        }
    }
}
