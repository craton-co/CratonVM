import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Every `java.net.URI` raw/decoded accessor pair, printed for URIs that carry a
 * percent-escape in each component in turn.
 *
 * This is the residual of the `getRawPath()` investigation
 * (`docs/known-issues/springboot/nestedpathtests-uri-getrawpath-percent-encoding-
 * inconsistency-20260827.md`, "check whether the other raw accessors share the
 * same inconsistency"). The `getRawPath()` defect was invisible on any URI whose
 * path needed no escaping, so the only way to answer the question for the other
 * five accessors is to put a reserved character in each of them and diff the
 * whole table against HotSpot.
 *
 * Diff CratonVM's stdout against the same JDK's, byte for byte.
 */
public class UriRawAccessorSweepProbe {

    private static void dump(String label, URI u) {
        System.out.println("== " + label);
        System.out.println("  toString                     = " + u);
        System.out.println("  toASCIIString                = " + u.toASCIIString());
        System.out.println("  getScheme                    = " + u.getScheme());
        System.out.println("  isOpaque                     = " + u.isOpaque());
        System.out.println("  isAbsolute                   = " + u.isAbsolute());
        System.out.println("  getRawSchemeSpecificPart     = " + u.getRawSchemeSpecificPart());
        System.out.println("  getSchemeSpecificPart        = " + u.getSchemeSpecificPart());
        System.out.println("  getRawAuthority              = " + u.getRawAuthority());
        System.out.println("  getAuthority                 = " + u.getAuthority());
        System.out.println("  getRawUserInfo               = " + u.getRawUserInfo());
        System.out.println("  getUserInfo                  = " + u.getUserInfo());
        System.out.println("  getHost                      = " + u.getHost());
        System.out.println("  getPort                      = " + u.getPort());
        System.out.println("  getRawPath                   = " + u.getRawPath());
        System.out.println("  getPath                      = " + u.getPath());
        System.out.println("  getRawQuery                  = " + u.getRawQuery());
        System.out.println("  getQuery                     = " + u.getQuery());
        System.out.println("  getRawFragment               = " + u.getRawFragment());
        System.out.println("  getFragment                  = " + u.getFragment());
    }

    private static void dumpOrError(String label, String spec) {
        try {
            dump(label, new URI(spec));
        } catch (Exception e) {
            System.out.println("== " + label);
            System.out.println("  EXCEPTION " + e.getClass().getName() + ": " + e.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        // The original defect's route: a Path that needs escaping, turned into a
        // URI by the JDK rather than parsed from text. A fixed name keeps the
        // output diffable -- Files.createTempFile's random infix would not be.
        Path p = Path.of("/tmp/rki-probe/te st%2Fx.jar");
        dump("Path.toUri (space + literal percent)", p.toUri());
        // ...and the same route through a real file, which is what Spring Boot's
        // NestedPathTests actually does (createTempFile, not Path.of).
        Path tmp = Files.createTempDirectory("rki sweep");
        try {
            Path real = tmp.resolve("te st.jar");
            Files.createFile(real);
            URI ru = real.toUri();
            System.out.println("== createTempFile toUri: raw path stays encoded = "
                    + !ru.getRawPath().contains(" "));
            System.out.println("   nested-concat parses    = " + describe(
                    () -> new URI("nested:" + ru.getRawPath() + "/!ne%20sted.jar").toString()
                            .startsWith("nested:")));
            Files.deleteIfExists(real);
        } finally {
            Files.deleteIfExists(tmp);
        }

        // One escaped component at a time, so a single wrong accessor cannot hide
        // behind another's correct answer.
        dumpOrError("escape in path", "http://h:88/a%20b/c%2Fd?q=1#f");
        dumpOrError("escape in query", "http://h:88/ab?k=a%20b%26c#f");
        dumpOrError("escape in fragment", "http://h:88/ab?q=1#fr%20ag%23ment");
        dumpOrError("escape in userinfo", "http://us%20er:pa%40ss@h:88/ab?q=1#f");
        dumpOrError("escape in authority (reg-name)", "http://a%20b.example:88/ab");
        dumpOrError("escape everywhere", "s://u%20i@h%2Eex:9/p%20a?q%20u#f%20r");
        dumpOrError("opaque with escapes", "mailto:a%20b@c.example?subject=x%20y");
        dumpOrError("relative with escape", "a%20b/c%20d?q%20=1#f%20");
        dumpOrError("no escapes at all", "http://user:pw@h:88/a/b?q=1#f");
        dumpOrError("authority-only", "http://h");
        dumpOrError("empty authority", "file:///tmp/x");

        // Round-trip: every raw accessor must re-parse into the same URI.
        URI base = new URI("http://us%20er@h:88/p%20a/b?q%20=1#f%20r");
        StringBuilder sb = new StringBuilder(base.getScheme()).append("://");
        sb.append(base.getRawAuthority()).append(base.getRawPath());
        sb.append('?').append(base.getRawQuery()).append('#').append(base.getRawFragment());
        System.out.println("== rebuilt-from-raw");
        System.out.println("  text                         = " + sb);
        System.out.println("  equals original              = " + new URI(sb.toString()).equals(base));
    }

    private interface Thunk { Object get() throws Exception; }

    private static String describe(Thunk t) {
        try {
            return String.valueOf(t.get());
        } catch (Exception e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }
}
