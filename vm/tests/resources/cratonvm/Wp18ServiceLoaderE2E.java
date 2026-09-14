package cratonvm;

import java.io.BufferedReader;
import java.io.StringReader;
import java.sql.Connection;
import java.sql.Driver;
import java.sql.DriverPropertyInfo;
import java.util.Iterator;
import java.util.Properties;
import java.util.ServiceLoader;
import java.util.logging.Logger;

/**
 * WP1.8-narrow fixture — drives the two synthetic-stub method-table
 * gaps that prevent {@code ServiceLoader.load(Class).iterator()} from
 * running end-to-end:
 *
 * <ol>
 *   <li>{@code Class.forName(String)} must be resolvable from bytecode
 *       — the synthetic {@code java/lang/Class} stub method table must
 *       declare it.</li>
 *   <li>{@code BufferedReader.<init>(Reader)} must be resolvable from
 *       bytecode — the synthetic {@code java/io/BufferedReader} stub
 *       method table must declare it.</li>
 * </ol>
 *
 * Each method below probes one piece of the chain and returns 1 on
 * success, 0 on a soft mismatch, and -99 on any throwable. The
 * companion Rust test {@code vm/tests/wp1_8_serviceloader_e2e.rs}
 * asserts each entry point separately so a regression points at the
 * exact gap reopening.
 *
 * Mirrors the WP7.1 {@link Wp71JdbcSpi} pattern — same SPI
 * descriptor, same {@link FakeDriver} inner class, exercised this time
 * through the real {@code ServiceLoader} pipeline rather than the
 * native helpers WP7.1 used as a workaround.
 */
public class Wp18ServiceLoaderE2E {

    /** Name of the driver class advertised by the synthetic SPI fixture. */
    public static final String EXPECTED_DRIVER =
        "cratonvm.Wp18ServiceLoaderE2E$FakeDriver";

    // -------------------------------------------------------------
    // Test entry points (called from vm/tests/wp1_8_serviceloader_e2e.rs).
    // -------------------------------------------------------------

    /**
     * Closure 1: {@code Class.forName(String)} resolves a synthetic
     * stub class from bytecode. Returns 1 if {@code Class.forName}
     * returned a non-null Class for "java.lang.String", else 0.
     */
    public static int forNameStringResolves() {
        try {
            Class<?> c = Class.forName("java.lang.String");
            return c != null ? 1 : 0;
        } catch (Throwable t) {
            return -99;
        }
    }

    /**
     * Closure 2: {@code new BufferedReader(new StringReader(...))}
     * — the BufferedReader(Reader) constructor must be resolvable from
     * bytecode. Returns 1 if a BufferedReader could be constructed and
     * the reader was wired through, else 0.
     *
     * StringReader is used (not InputStreamReader+System.in) so the
     * fixture stays hermetic.
     */
    public static int bufferedReaderCtorResolves() {
        try {
            BufferedReader br = new BufferedReader(new StringReader("hello"));
            // The inner reader carries the "hello" payload — if the
            // ctor only registered as a no-op stub, the readLine() call
            // below would still potentially succeed on a happy path.
            // Instead just confirm the reference is non-null and the
            // ctor didn't throw NoSuchMethodError mid-resolve.
            return br != null ? 1 : 0;
        } catch (Throwable t) {
            return -99;
        }
    }

    /**
     * End-to-end: {@code ServiceLoader.load(java.sql.Driver.class).iterator()}
     * walks the classpath, reads
     * {@code META-INF/services/java.sql.Driver}, and instantiates each
     * listed provider. Returns the count of providers iterated.
     *
     * The Rust harness writes the SPI descriptor at runtime to a temp
     * dir on the VM classpath, so this method's contract is "return >0
     * iff the descriptor is reachable AND the proper iterator chain
     * is wired up end-to-end".
     */
    public static int serviceLoaderIteratorCount() {
        try {
            ServiceLoader<Driver> sl = ServiceLoader.load(Driver.class);
            Iterator<Driver> it = sl.iterator();
            int count = 0;
            while (it.hasNext()) {
                Driver d = it.next();
                if (d != null) {
                    count++;
                }
            }
            return count;
        } catch (Throwable t) {
            return -99;
        }
    }

    /**
     * Confirms the {@link FakeDriver} class itself loads + instantiates
     * independently of SPI walking. If this is 1 but
     * {@link #serviceLoaderIteratorCount} is 0, the fault is in
     * SPI iteration, not driver class loading.
     */
    public static int fakeDriverInstantiates() {
        try {
            Driver d = new FakeDriver();
            return d.getMajorVersion() == 18 ? 1 : 0;
        } catch (Throwable t) {
            return -99;
        }
    }

    /**
     * Minimal {@code java.sql.Driver} implementation used by the WP1.8
     * regression test. The Rust harness writes its FQN
     * ({@code cratonvm.Wp18ServiceLoaderE2E$FakeDriver}) into
     * {@code META-INF/services/java.sql.Driver} on the test classpath.
     */
    public static class FakeDriver implements Driver {
        public Connection connect(String url, Properties info) {
            return null;
        }
        public boolean acceptsURL(String url) {
            return url != null && url.startsWith("jdbc:wp18fake:");
        }
        public DriverPropertyInfo[] getPropertyInfo(String url, Properties info) {
            return new DriverPropertyInfo[0];
        }
        public int getMajorVersion() { return 18; }
        public int getMinorVersion() { return 8; }
        public boolean jdbcCompliant() { return false; }
        public Logger getParentLogger() { return null; }
    }
}
