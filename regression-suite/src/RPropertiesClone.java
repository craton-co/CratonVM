import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;
import java.util.Properties;

/**
 * Regression: {@code java.util.Properties.clone()} and {@code replaceAll()} on
 * a Properties this VM built natively.
 *
 * WHY THIS VECTOR EXISTS. CratonVM does not store a Properties' entries where
 * the JDK does. Entries live in an identity-keyed side-table, and the inherited
 * {@code ConcurrentHashMap map} field is, per the note on
 * {@code register_properties_sidetable}, "deliberately never populated" — which
 * is why every read/write method of Properties is natively overridden. Two
 * methods never got that treatment, and both dereference {@code map}:
 *
 * <pre>
 *   Properties.clone()      -&gt; clone.map = new ConcurrentHashMap&lt;&gt;(map)
 *   Properties.replaceAll() -&gt; map.replaceAll(function)
 * </pre>
 *
 * So both threw {@code NullPointerException} on any Properties that had never
 * been WRITTEN through — a fresh {@code new Properties()}, and
 * {@code System.getProperties()}, which the VM synthesises and never writes.
 * A Properties that HAD been written worked, because the write paths lazily
 * create the CHM; that is why this vector exercises all three shapes and not
 * just the convenient one.
 *
 * Found via Testcontainers: {@code DefaultDockerClientConfig
 * .createDefaultConfigBuilder} does {@code (Properties) System.getProperties()
 * .clone()}, {@code ServiceLoader} rewrapped the NPE as a
 * {@code ServiceConfigurationError}, and 138 of 191 hibernate-reactive classes
 * failed identically in one batch.
 *
 * WHAT IS ASSERTED, and why it is not "did it throw". A clone that returns an
 * empty Properties, or one that ALIASES the original's backing, would pass any
 * no-exception check. Both are live failure modes here: the shallow copy this
 * VM's {@code Object.clone} performs leaves {@code map} pointing at the
 * receiver's CHM unless the override replaces it, and the side-table is keyed
 * by object identity, so a clone starts with an EMPTY one unless the override
 * replicates it. Every assertion below is therefore about CONTENTS and about
 * INDEPENDENCE IN BOTH DIRECTIONS.
 *
 * ORACLE: HotSpot 25, same class, same assertions — all 8 shapes clean there.
 */
public class RPropertiesClone {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static class SubProps extends Properties {}

    /** Sorted key list via propertyNames(), which reads the side-table. */
    static List<String> names(Properties p) {
        List<String> out = new ArrayList<>();
        for (Enumeration<?> e = p.propertyNames(); e.hasMoreElements(); ) {
            out.add(String.valueOf(e.nextElement()));
        }
        java.util.Collections.sort(out);
        return out;
    }

    public static void main(String[] args) {
        // ---- 1. a Properties that has never been written through. This is the
        //         shape that threw; a populated one did not.
        Properties empty = new Properties();
        Properties emptyClone = (Properties) empty.clone();
        check(emptyClone != empty, "clone of empty must be a new object");
        check(emptyClone.size() == 0, "clone of empty must be empty, got " + emptyClone.size());
        check(emptyClone.getClass() == Properties.class,
                "clone must keep the concrete class, got " + emptyClone.getClass());

        // ---- 2. contents survive
        Properties src = new Properties();
        src.setProperty("alpha", "1");
        src.setProperty("beta", "2");
        Properties copy = (Properties) src.clone();
        check(copy.size() == 2, "clone size, got " + copy.size());
        check("1".equals(copy.getProperty("alpha")), "clone alpha, got " + copy.getProperty("alpha"));
        check("2".equals(copy.getProperty("beta")), "clone beta, got " + copy.getProperty("beta"));
        check(names(copy).equals(List.of("alpha", "beta")), "clone propertyNames, got " + names(copy));

        // ---- 3. independence, BOTH directions. A shallow copy that aliases the
        //         receiver's backing passes a one-direction test.
        copy.setProperty("alpha", "CHANGED");
        copy.setProperty("gamma", "3");
        check("1".equals(src.getProperty("alpha")),
                "writing the clone must not touch the original, got " + src.getProperty("alpha"));
        check(src.getProperty("gamma") == null,
                "adding to the clone must not touch the original, got " + src.getProperty("gamma"));
        src.setProperty("beta", "ORIGINAL");
        check("2".equals(copy.getProperty("beta")),
                "writing the original must not touch the clone, got " + copy.getProperty("beta"));
        check(src.size() == 2, "original size after its own write, got " + src.size());
        check(copy.size() == 3, "clone size after its own writes, got " + copy.size());

        // ---- 4. System.getProperties() — the receiver Testcontainers clones,
        //         which this VM synthesises and never writes through.
        Properties sys = System.getProperties();
        Properties sysClone = (Properties) sys.clone();
        check(sysClone != sys, "sysprops clone must be a new object");
        check(sysClone.size() == sys.size(),
                "sysprops clone size " + sysClone.size() + " != " + sys.size());
        check(sysClone.getProperty("java.version") != null, "sysprops clone lost java.version");
        check(sysClone.getProperty("file.separator") != null, "sysprops clone lost file.separator");
        check(sysClone.getProperty("java.version").equals(sys.getProperty("java.version")),
                "sysprops clone java.version differs");
        // Mutating the clone must not reach the real system properties.
        sysClone.setProperty("cratonvm.rpropsclone.probe", "yes");
        check(System.getProperty("cratonvm.rpropsclone.probe") == null,
                "writing a CLONE of system properties must not set a real system property");

        // ---- 5. defaults chain is carried, and is still a fallback
        Properties defs = new Properties();
        defs.setProperty("dk", "dv");
        Properties withDefs = new Properties(defs);
        withDefs.setProperty("own", "ov");
        Properties defsClone = (Properties) withDefs.clone();
        check("ov".equals(defsClone.getProperty("own")), "clone own key, got " + defsClone.getProperty("own"));
        check("dv".equals(defsClone.getProperty("dk")),
                "clone must still fall back to defaults, got " + defsClone.getProperty("dk"));

        // ---- 6. a subclass clones as itself
        SubProps sub = new SubProps();
        sub.setProperty("s", "1");
        Object subClone = sub.clone();
        check(subClone.getClass() == SubProps.class,
                "subclass must clone as itself, got " + subClone.getClass());
        check("1".equals(((Properties) subClone).getProperty("s")), "subclass clone lost its entry");

        // ---- 7. replaceAll — the second method with the same root cause. On a
        //         never-written receiver it threw; making `map` merely non-null
        //         would have made it silently replace NOTHING instead.
        Properties fresh = new Properties();
        fresh.replaceAll((k, v) -> "unused");
        check(fresh.size() == 0, "replaceAll on empty must leave it empty, got " + fresh.size());

        Properties rep = new Properties();
        rep.setProperty("a", "1");
        rep.setProperty("b", "2");
        rep.replaceAll((k, v) -> k + "=" + v);
        check("a=1".equals(rep.getProperty("a")), "replaceAll a, got " + rep.getProperty("a"));
        check("b=2".equals(rep.getProperty("b")), "replaceAll b, got " + rep.getProperty("b"));
        check(rep.size() == 2, "replaceAll must not change the key set, got " + rep.size());
        check(names(rep).equals(List.of("a", "b")), "replaceAll keys, got " + names(rep));

        // On a clone of system properties: a never-written receiver with real
        // content. Uppercasing every value must change them and keep the keys.
        Properties sysCopy = (Properties) System.getProperties().clone();
        int before = sysCopy.size();
        String verBefore = sysCopy.getProperty("java.version");
        sysCopy.replaceAll((k, v) -> String.valueOf(v).toUpperCase());
        check(sysCopy.size() == before, "replaceAll on sysprops clone changed the size");
        check(sysCopy.getProperty("java.version").equals(verBefore.toUpperCase()),
                "replaceAll on sysprops clone did not replace, got "
                        + sysCopy.getProperty("java.version"));
        check(System.getProperty("java.version").equals(verBefore),
                "replaceAll on a CLONE must not touch the real system properties");

        System.out.println("CK RPropertiesClone checks=" + checks);
        System.out.println("PASS RPropertiesClone (" + checks + " checks)");
    }
}
