import com.hazelcast.config.Config;

// The smallest thing that runs the code path
// `HazelcastAutoConfigurationClientTests.@BeforeAll` fails in: Hazelcast's own
// `Config.load()`, which parses the bundled `hazelcast-default.xml` and hands
// the DOM to `AbstractXmlConfigHelper.schemaValidation`. No Spring, no JUnit.
//
// A hand-rolled replica of that validation (XsdValidateProbe) passes on the
// binary where the real one fails, so the difference is inside Hazelcast's own
// schema assembly — this probe keeps that part real.
//
// Prints `HZLOAD: ok clusterName=<name>` or the failure.
public class HzConfigLoadProbe {

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        int ok = 0;
        int fail = 0;
        for (int i = 0; i < iterations; i++) {
            try {
                Config config = Config.load();
                ok++;
                if (i == 0) {
                    System.out.println("HZLOAD: ok clusterName=" + config.getClusterName()
                            + " joinKubernetesEnabled="
                            + config.getNetworkConfig().getJoin().getKubernetesConfig().isEnabled());
                }
            }
            catch (Throwable t) {
                fail++;
                if (fail == 1) {
                    System.out.println("HZLOAD: FAILED at iteration " + i + " -> "
                            + t.getClass().getName() + ": " + firstLine(t.getMessage()));
                }
            }
        }
        System.out.println("HZLOAD-SUMMARY: ok=" + ok + " fail=" + fail);
    }

    private static String firstLine(String s) {
        if (s == null) {
            return "(no message)";
        }
        int nl = s.indexOf('\n');
        String line = nl >= 0 ? s.substring(0, nl) : s;
        return line.length() > 200 ? line.substring(0, 200) + "..." : line;
    }
}
