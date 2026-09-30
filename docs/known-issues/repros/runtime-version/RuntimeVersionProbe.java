import java.util.List;
import java.util.Optional;

/** Differential probe for Runtime.version() / Runtime.Version accessors. */
public class RuntimeVersionProbe {
    static void p(String label, Object v) {
        System.out.println(label + "=" + v);
    }

    static void probe(String tag, Runtime.Version v) {
        try {
            p(tag + ".toString", v.toString());
        } catch (Throwable t) {
            p(tag + ".toString", "THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
        try {
            p(tag + ".version", v.version());
        } catch (Throwable t) {
            p(tag + ".version", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".feature", v.feature());
        } catch (Throwable t) {
            p(tag + ".feature", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".interim", v.interim());
        } catch (Throwable t) {
            p(tag + ".interim", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".update", v.update());
        } catch (Throwable t) {
            p(tag + ".update", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".patch", v.patch());
        } catch (Throwable t) {
            p(tag + ".patch", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".pre", v.pre());
        } catch (Throwable t) {
            p(tag + ".pre", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".build", v.build());
        } catch (Throwable t) {
            p(tag + ".build", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".optional", v.optional());
        } catch (Throwable t) {
            p(tag + ".optional", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".equalsSelf", v.equals(v));
        } catch (Throwable t) {
            p(tag + ".equalsSelf", "THREW " + t.getClass().getName());
        }
        try {
            p(tag + ".compareToSelf", v.compareTo(v));
        } catch (Throwable t) {
            p(tag + ".compareToSelf", "THREW " + t.getClass().getName());
        }
        try {
            v.hashCode();
            p(tag + ".hashCode", "ok");
        } catch (Throwable t) {
            p(tag + ".hashCode", "THREW " + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        p("sysprop.java.specification.version", System.getProperty("java.specification.version"));
        p("sysprop.java.version", System.getProperty("java.version"));
        p("sysprop.java.runtime.version", System.getProperty("java.runtime.version"));

        Runtime.Version v = Runtime.version();
        probe("version()", v);

        // String concatenation path (Lucene Constants does exactly this).
        try {
            p("concat", "JVM " + Runtime.version());
        } catch (Throwable t) {
            p("concat", "THREW " + t.getClass().getName() + ": " + t.getMessage());
        }

        // Lucene's Constants.<clinit> shape: split the toString on '-'/'+'.
        try {
            String s = Runtime.version().toString();
            p("luceneShape", s.split("[-+]")[0]);
        } catch (Throwable t) {
            p("luceneShape", "THREW " + t.getClass().getName());
        }

        probe("parse(25.0.1+9)", Runtime.Version.parse("25.0.1+9"));
        probe("parse(8)", Runtime.Version.parse("8"));
        probe("parse(17.0.2-ea+7-abc)", Runtime.Version.parse("17.0.2-ea+7-abc"));

        try {
            p("cmp.version-vs-parse8", Integer.signum(Runtime.version().compareTo(Runtime.Version.parse("8"))));
        } catch (Throwable t) {
            p("cmp.version-vs-parse8", "THREW " + t.getClass().getName());
        }
        try {
            p("cmp.parse-roundtrip",
                Runtime.Version.parse(Runtime.version().toString()).equals(Runtime.version()));
        } catch (Throwable t) {
            p("cmp.parse-roundtrip", "THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
        try {
            p("version.equals.sameCall", Runtime.version().equals(Runtime.version()));
        } catch (Throwable t) {
            p("version.equals.sameCall", "THREW " + t.getClass().getName());
        }
        try {
            p("version.hashEq.sameCall",
                Runtime.version().hashCode() == Runtime.version().hashCode());
        } catch (Throwable t) {
            p("version.hashEq.sameCall", "THREW " + t.getClass().getName());
        }
        p("DONE", "ok");
    }
}
