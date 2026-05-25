import org.apache.hadoop.conf.Configuration;
import org.apache.hadoop.fs.Path;

public class HadoopProbe {
    public static void main(String[] args) {
        try {
            // 1. Build a Configuration; set 4 types of properties; assert round-trip.
            Configuration conf = new Configuration(false);
            conf.set("probe.string", "hello hadoop");
            conf.setInt("probe.int", 42);
            conf.setLong("probe.long", 1234567890123L);
            conf.setBoolean("probe.bool", true);
            conf.setStrings("probe.array", "a", "b", "c");

            if (!"hello hadoop".equals(conf.get("probe.string"))) {
                System.out.println("FAIL: string"); System.exit(1);
            }
            if (conf.getInt("probe.int", 0) != 42) {
                System.out.println("FAIL: int"); System.exit(1);
            }
            if (conf.getLong("probe.long", 0) != 1234567890123L) {
                System.out.println("FAIL: long"); System.exit(1);
            }
            if (!conf.getBoolean("probe.bool", false)) {
                System.out.println("FAIL: bool"); System.exit(1);
            }
            String[] arr = conf.getStrings("probe.array");
            if (arr == null || arr.length != 3 || !"b".equals(arr[1])) {
                System.out.println("FAIL: array"); System.exit(1);
            }
            System.out.println("Configuration round-trip OK (5 types)");

            // 2. Path parsing (no FS instantiation — uses URI parser).
            Path p = new Path("file:///tmp/probe.txt");
            if (!"probe.txt".equals(p.getName())) {
                System.out.println("FAIL: Path.getName"); System.exit(1);
            }
            if (!"file".equals(p.toUri().getScheme())) {
                System.out.println("FAIL: Path.scheme"); System.exit(1);
            }
            System.out.println("Path OK: " + p);

            System.out.println("OK");
        } catch (Throwable t) {
            t.printStackTrace();
            System.exit(1);
        }
        System.exit(0);
    }
}
