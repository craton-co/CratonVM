import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.Properties;

/**
 * The ORDER half of `Properties`, which `PropertiesShadowSweep` does not ask
 * about and which is the whole subject of `ordered_snapshot_kv`.
 *
 * Since JDK 9 `Properties` is not a `Hashtable` bucket walk: it delegates to a
 * private `ConcurrentHashMap`, so `keys()` / `stringPropertyNames()` /
 * `entrySet()` enumerate in THAT CHM's order, and real code depends on it
 * (gh-11892: Spring Boot's `MapBinder.bindEntries` lets whichever of
 * `commit.id` and `commit.id.abbrev` comes first decide whether the value binds
 * as a scalar or a nested map). So the keys below are that exact shape.
 *
 * NOTHING HERE IS SORTED. Sorting the output would normalise away the only
 * property under test. Each row prints a sequence, and a differing sequence is
 * the finding.
 *
 * `replaceAll` is asked for the order it invokes the user's BiFunction in --
 * the order is handed to Java as the sequence of `apply` calls, so the function
 * records it rather than transforming anything interesting.
 */
public final class PropsOrderSweep {
    static int n = 0;

    static void p(String label, Object v) {
        System.out.println(++n + " " + label + " |" + v + "|");
    }

    static Properties git() {
        Properties q = new Properties();
        q.setProperty("commit.id.full", "abcdef0123");
        q.setProperty("branch", "main");
        q.setProperty("commit.id.abbrev", "abcdef0");
        q.setProperty("commit.id", "abcdef0123");
        return q;
    }

    static List<String> keysOf(Properties q) {
        List<String> out = new ArrayList<>();
        for (Enumeration<?> e = q.keys(); e.hasMoreElements(); ) {
            out.add(String.valueOf(e.nextElement()));
        }
        return out;
    }

    public static void main(String[] args) {
        // ---- the receiver's own order, as a control -----------------------
        Properties q = git();
        p("keys order", keysOf(q));
        p("stringPropertyNames order", new ArrayList<>(q.stringPropertyNames()));

        // ---- clone: the clone must enumerate the way the source does ------
        Properties c = (Properties) q.clone();
        p("clone keys order", keysOf(c));
        p("clone stringPropertyNames order", new ArrayList<>(c.stringPropertyNames()));
        p("clone order equals source order", keysOf(c).equals(keysOf(q)));
        p("clone size", c.size());
        p("clone is independent", clonesAreIndependent(q));

        // ---- replaceAll: the order it applies the function in -------------
        Properties r = git();
        List<String> seen = new ArrayList<>();
        r.replaceAll((k, v) -> {
            seen.add(String.valueOf(k));
            return String.valueOf(v) + "!";
        });
        p("replaceAll apply order", seen);
        p("replaceAll apply order equals keys order", seen.equals(keysOf(git())));
        p("replaceAll keys order after", keysOf(r));
        p("replaceAll transformed a value", r.getProperty("branch"));
        p("replaceAll size unchanged", r.size());

        // ---- the shapes whose `map` is null: the reason the natives exist --
        Properties fresh = new Properties();
        p("fresh clone size", ((Properties) fresh.clone()).size());
        p("fresh keys order", keysOf((Properties) fresh.clone()));
        fresh.replaceAll((k, v) -> v);
        p("fresh replaceAll survived", fresh.size());

        Properties sys = System.getProperties();
        Properties sysClone = (Properties) sys.clone();
        p("System clone has entries", sysClone.size() > 10);
        p("System clone order equals System order", keysOf(sysClone).equals(keysOf(sys)));
        p("System clone java.version present", sysClone.getProperty("java.version") != null);

        // ---- a single-entry receiver takes ordered_snapshot_kv's early exit
        Properties one = new Properties();
        one.setProperty("solo", "1");
        p("single clone keys", keysOf((Properties) one.clone()));
        List<String> soloSeen = new ArrayList<>();
        one.replaceAll((k, v) -> { soloSeen.add(String.valueOf(k)); return v; });
        p("single replaceAll order", soloSeen);

        // ---- defaults are NOT enumerated by keys(), but ARE by
        //      stringPropertyNames(); clone keeps the same defaults chain -----
        Properties defs = new Properties();
        defs.setProperty("d", "dv");
        Properties child = new Properties(defs);
        child.setProperty("own", "ov");
        Properties childClone = (Properties) child.clone();
        p("child clone keys order", keysOf(childClone));
        p("child clone sees default", childClone.getProperty("d"));
        List<String> names = new ArrayList<>(childClone.stringPropertyNames());
        Collections.sort(names); // membership only: the SET is the question here
        p("child clone stringPropertyNames set", names);
        // A terminal marker, so a run that dies partway is not read as a
        // clean diff of a short file. The three-way runner checks for one and
        // this probe reported DONE=0 on all three arms without it.
        System.out.println("DONE PropsOrderSweep");
    }

    static boolean clonesAreIndependent(Properties src) {
        Properties c = (Properties) src.clone();
        c.setProperty("added.to.clone", "x");
        return src.getProperty("added.to.clone") == null && c.size() == src.size() + 1;
    }
}
