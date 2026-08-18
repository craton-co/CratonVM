import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Enumeration;
import java.util.Properties;
import java.util.Set;

/**
 * G60-1 N2 — does {@code java/util/Properties.getProperty} need a native at all?
 *
 * <p>The census says a {@code Bridge} runs over the real
 * {@code java.util.Properties.getProperty} bytecode, and N2 asks whether the
 * side table behind it is load-bearing or whether the real {@code String}-keyed
 * JDK code would answer correctly on its own. The question is not answerable by
 * reading {@code getProperty} alone: its state comes from {@code load},
 * {@code setProperty} and {@code put}, and each of those has its own native. If
 * the WRITERS keep their entries somewhere the real reader cannot see, retiring
 * the reader breaks the round trip — and the reverse if they write real
 * {@code Hashtable} state.
 *
 * <p>So this probe measures the round trips, not the reader. Run it against
 * {@code CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/Properties}, which yields
 * every {@code Properties} shadow on the same §1.4 predicate a retirement uses.
 * A {@code load}/{@code getProperty} pair that disagrees with HotSpot under the
 * dial is the answer to N2: the native is load-bearing and the reason belongs in
 * the retirement table's doc.
 *
 * <p>Every line is a fixed name plus a value so the three arms diff directly.
 */
public final class JdkOnlyPropsShadowProbe {

    static int checks;
    static int fails;

    static void ck(String name, Object got, Object want) {
        checks++;
        boolean ok = got == null ? want == null : got.equals(want);
        if (!ok) {
            fails++;
        }
        System.out.println("CK " + name + " got=" + got + " want=" + want + (ok ? "" : "  MISMATCH"));
    }

    public static void main(String[] args) throws Exception {
        setPropertyRoundTrip();
        loadRoundTrip();
        hashtableViewOfTheSameObject();
        defaultsChain();
        systemProperties();
        System.out.println("CK checks=" + checks);
        System.out.println("CK fails=" + fails);
        System.out.println((fails == 0 ? "PASS " : "FAIL ") + "JdkOnlyPropsShadowProbe");
    }

    /** The narrowest round trip: one writer, one reader, one object. */
    static void setPropertyRoundTrip() {
        Properties p = new Properties();
        Object prev = p.setProperty("k", "v");
        ck("set.returnsPrevNull", prev, null);
        ck("set.get", p.getProperty("k"), "v");
        ck("set.getMissing", p.getProperty("nope"), null);
        ck("set.getMissingWithDefault", p.getProperty("nope", "dflt"), "dflt");
        Object prev2 = p.setProperty("k", "v2");
        ck("set.returnsPrev", prev2, "v");
        ck("set.getAfterOverwrite", p.getProperty("k"), "v2");
    }

    /**
     * The round trip the side table was built for: {@code load} is real JDK
     * bytecode calling {@code this.put(k, v)} in the reference implementation,
     * so this is where a writer/reader split shows up.
     */
    static void loadRoundTrip() throws IOException {
        String text = "x=1\ny = 2\n# comment\nz:3\nempty=\ncont=a\\\n  b\n";
        Properties p = new Properties();
        p.load(new ByteArrayInputStream(text.getBytes(StandardCharsets.ISO_8859_1)));
        ck("load.x", p.getProperty("x"), "1");
        ck("load.y", p.getProperty("y"), "2");
        ck("load.zColonSeparated", p.getProperty("z"), "3");
        ck("load.emptyValue", p.getProperty("empty"), "");
        ck("load.continuation", p.getProperty("cont"), "ab");
        ck("load.commentIsNotAKey", p.getProperty("# comment"), null);
        ck("load.size", p.size(), 5);
    }

    /**
     * {@code Properties} IS a {@code Hashtable}, and half its documented API is
     * inherited. A writer that keeps entries outside the real table answers
     * these differently from {@code getProperty} — which is the shape that makes
     * the reader's native load-bearing rather than redundant.
     */
    static void hashtableViewOfTheSameObject() {
        Properties p = new Properties();
        p.setProperty("a", "1");
        p.put("b", "2");
        ck("ht.getA", p.get("a"), "1");
        ck("ht.getB", p.get("b"), "2");
        ck("ht.getPropertyB", p.getProperty("b"), "2");
        ck("ht.containsKeyA", p.containsKey("a"), Boolean.TRUE);
        ck("ht.containsValue1", p.containsValue("1"), Boolean.TRUE);
        ck("ht.size", p.size(), 2);
        ck("ht.isEmpty", p.isEmpty(), Boolean.FALSE);
        Set<String> names = p.stringPropertyNames();
        ck("ht.stringPropertyNamesSize", names.size(), 2);
        ck("ht.stringPropertyNamesHasA", names.contains("a"), Boolean.TRUE);
        int keys = 0;
        for (Enumeration<?> e = p.propertyNames(); e.hasMoreElements(); ) {
            e.nextElement();
            keys++;
        }
        ck("ht.propertyNamesCount", keys, 2);
        ck("ht.keySetSize", p.keySet().size(), 2);
        ck("ht.entrySetSize", p.entrySet().size(), 2);
        Object removed = p.remove("a");
        ck("ht.removeReturnsValue", removed, "1");
        ck("ht.getAfterRemove", p.getProperty("a"), null);
        ck("ht.sizeAfterRemove", p.size(), 1);
    }

    /**
     * {@code getProperty} is the ONE reader that walks the {@code defaults}
     * chain, and {@code get} deliberately does not. A native reader that ignores
     * the chain answers the second line below wrongly, and a real one cannot.
     */
    static void defaultsChain() {
        Properties base = new Properties();
        base.setProperty("inherited", "fromBase");
        base.setProperty("shadowed", "fromBase");
        Properties child = new Properties(base);
        child.setProperty("shadowed", "fromChild");
        child.setProperty("own", "fromChild");
        ck("defaults.inherited", child.getProperty("inherited"), "fromBase");
        ck("defaults.getIgnoresChain", child.get("inherited"), null);
        ck("defaults.childWins", child.getProperty("shadowed"), "fromChild");
        ck("defaults.own", child.getProperty("own"), "fromChild");
        ck("defaults.sizeExcludesChain", child.size(), 2);
        ck("defaults.stringPropertyNamesIncludesChain",
                child.stringPropertyNames().size(), 3);
    }

    /**
     * The interop N2 asks about by name: if the side table exists to back
     * {@code System.getProperties()}, this is where that shows.
     */
    static void systemProperties() {
        ck("sys.javaVersionPresent", System.getProperty("java.version") != null, Boolean.TRUE);
        Properties sp = System.getProperties();
        ck("sys.getPropertiesNonNull", sp != null, Boolean.TRUE);
        ck("sys.javaHomeViaObject", sp.getProperty("java.home") != null, Boolean.TRUE);
        ck("sys.pathSeparatorAgrees",
                sp.getProperty("path.separator"), System.getProperty("path.separator"));
        System.setProperty("g601.probe.key", "g601");
        ck("sys.setThenGet", System.getProperty("g601.probe.key"), "g601");
        ck("sys.setThenGetViaObject",
                System.getProperties().getProperty("g601.probe.key"), "g601");
    }
}
