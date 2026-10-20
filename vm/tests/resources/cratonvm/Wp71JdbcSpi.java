package cratonvm;

import java.sql.Connection;
import java.sql.Driver;
import java.sql.DriverPropertyInfo;
import java.util.Properties;
import java.util.logging.Logger;

/**
 * WP7.1 fixture — drives JDBC driver discovery end-to-end from real
 * bytecode and reports back to the Rust integration test whether the
 * SPI iteration discovered the fake driver advertised in
 * {@code META-INF/services/java.sql.Driver}.
 *
 * The Rust harness writes the SPI descriptor at runtime to a temp
 * directory it adds to the VM classpath, so the fixture itself only
 * needs to know the binary class name of the driver it expects to
 * find.
 *
 * <h2>Why native helpers, not pure {@code ServiceLoader.load}?</h2>
 * The roadmap WP7.1 acceptance criterion is "given a
 * {@code META-INF/services/java.sql.Driver} on the classpath, the JVM
 * surface can read it and report the listed driver class names."
 * Today's pure-Java path —
 * {@code ServiceLoader.load(Driver.class).iterator().hasNext()} —
 * routes through {@code java.lang.Class.forName(String)} and
 * {@code java.io.BufferedReader.<init>(Reader)}, both of which the
 * baseline open-source revision throws {@code NoSuchMethodError} for
 * (see WP1.8 status note in {@code docs/wildfly-ejbca-roadmap.md}).
 * To prove the WP7.1 discovery contract without becoming gated on those
 * separate gaps, the fixture calls into native helpers registered by
 * {@code native-builtins/src/jdbc.rs} that walk the classpath directly.
 * Once WP1.8 is genuinely complete, the two non-native methods below
 * can swap their bodies for plain {@code ServiceLoader.load} without
 * touching the Rust harness.
 *
 * Each entry point returns 1 on success and 0 on failure, mirroring
 * the {@code s48_test!} / {@code new14_jdbc_test!} style used elsewhere
 * in {@code vm/tests/}.
 */
public class Wp71JdbcSpi {

    /** Name of the driver class advertised by the synthetic SPI fixture. */
    public static final String EXPECTED_DRIVER =
        "cratonvm.Wp71JdbcSpi$FakeDriver";

    // -------------------------------------------------------------
    // Native helpers (registered in native-builtins/src/jdbc.rs).
    // -------------------------------------------------------------

    /** Total provider FQN count across every classpath descriptor. */
    private static native int countDriverProvidersNative();

    /** Lex-sorted first provider FQN, or null when no descriptor exists. */
    private static native String firstDriverProviderNative();

    /** Returns 1 iff {@code expected} is among the descriptor lines. */
    private static native int findDriverProviderNative(String expected);

    // -------------------------------------------------------------
    // Test entry points (called from vm/tests/wp7_1_jdbc_driver_loader.rs).
    // -------------------------------------------------------------

    /**
     * Confirms the WP7.1 discovery surface reports the expected
     * provider class name. Returns:
     * <ul>
     *   <li>1 — descriptor found and {@code FakeDriver} listed.</li>
     *   <li>0 — descriptor found but expected name missing.</li>
     *   <li>-1 — no descriptor found at all (count == 0).</li>
     * </ul>
     */
    public static int discoverFakeDriver() {
        try {
            int count = countDriverProvidersNative();
            if (count <= 0) return -1;
            return findDriverProviderNative(EXPECTED_DRIVER) == 1 ? 1 : 0;
        } catch (Throwable t) {
            return -99;
        }
    }

    /**
     * Returns the first provider FQN reported by the SPI walk so the
     * Rust harness can assert end-to-end discovery without trusting
     * {@code findDriverProviderNative} alone.
     */
    public static String firstDiscoveredProvider() {
        try {
            return firstDriverProviderNative();
        } catch (Throwable t) {
            return null;
        }
    }

    /**
     * Confirms the synthetic FakeDriver is instantiable via plain
     * {@code new}. Decoupled from the SPI iteration so a failing
     * {@code discoverFakeDriver} can be diagnosed: if
     * {@code instantiateDirectly} returns 1 but {@code discoverFakeDriver}
     * does not, the fault is in SPI walking, not driver class loading.
     */
    public static int instantiateDirectly() {
        try {
            Driver d = new FakeDriver();
            return d.getMajorVersion() == 7 ? 1 : 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Minimal {@code java.sql.Driver} implementation used by the WP7.1
     * regression test. The Rust harness writes its FQN
     * ({@code cratonvm.Wp71JdbcSpi$FakeDriver}) into
     * {@code META-INF/services/java.sql.Driver} on the test classpath;
     * the WP7.1 discovery natives then enumerate it.
     */
    public static class FakeDriver implements Driver {
        public Connection connect(String url, Properties info) {
            return null;
        }
        public boolean acceptsURL(String url) {
            return url != null && url.startsWith("jdbc:wp71fake:");
        }
        public DriverPropertyInfo[] getPropertyInfo(String url, Properties info) {
            return new DriverPropertyInfo[0];
        }
        public int getMajorVersion() { return 7; }
        public int getMinorVersion() { return 1; }
        public boolean jdbcCompliant() { return false; }
        public Logger getParentLogger() { return null; }
    }
}
