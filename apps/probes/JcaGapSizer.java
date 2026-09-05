import java.security.Provider;
import java.security.Security;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.Set;
import java.util.TreeMap;

/**
 * Size the JCA provider gap PER SERVICE, by the only test that decided the
 * JCEKS case: does the JDK's own SPI class load here?
 *
 * `jca-provider-population-gap-20260830-FIXED.md` measured that SunJCE carries 103 of
 * HotSpot's 194 services and SUN 44 of 68, and named the six/two missing TYPES.
 * A type is not a unit of work, though. The unit is a (type, algorithm) pair
 * with an implementation class behind it, and each pair is tractable exactly
 * when that class runs as bytecode on this VM -- which is how
 * `com.sun.crypto.provider.JceKeyStore` turned out to be a one-line table entry
 * rather than an implementation project.
 *
 * Run on HOTSPOT this enumerates the truth: every service of the named
 * providers, with the class the JDK would instantiate. Run on CRATONVM with
 * `--check <classname>...` it reports, for each, whether the class LOADS and
 * whether it can be INSTANTIATED -- two different answers, because a class that
 * resolves can still fail its `<clinit>` or its no-arg constructor, and only the
 * second is enough to serve the service.
 *
 * Printing both VMs' answers side by side turns "half the services are missing"
 * into a list with names, and a list is something somebody can work.
 */
public final class JcaGapSizer {
    public static void main(String[] args) throws Exception {
        if (args.length > 0 && args[0].equals("--check")) {
            for (int i = 1; i < args.length; i++) {
                check(args[i]);
            }
            return;
        }
        String[] providers = args.length > 0 ? args : new String[] {"SunJCE", "SUN"};
        for (String name : providers) {
            Provider p = Security.getProvider(name);
            if (p == null) {
                System.out.println("PROVIDER " + name + " ABSENT");
                continue;
            }
            Set<Provider.Service> svcs = p.getServices();
            // type -> "algorithm=class" lines, sorted, so two VMs' dumps diff cleanly.
            TreeMap<String, List<String>> byType = new TreeMap<>();
            for (Provider.Service s : svcs) {
                byType.computeIfAbsent(s.getType(), k -> new ArrayList<>())
                      .add(s.getAlgorithm() + "=" + s.getClassName());
            }
            for (var e : byType.entrySet()) {
                Collections.sort(e.getValue());
                for (String line : e.getValue()) {
                    System.out.println("SVC " + name + " " + e.getKey() + " " + line);
                }
            }
        }
    }

    static void check(String cn) {
        String loads, inst;
        Class<?> c = null;
        try {
            c = Class.forName(cn, false, JcaGapSizer.class.getClassLoader());
            loads = "yes";
        } catch (Throwable t) {
            loads = t.getClass().getSimpleName();
        }
        if (c == null) {
            inst = "-";
        } else {
            try {
                // Initialise separately from construct: a class can resolve and
                // still die in <clinit>, and that is a different repair.
                Class.forName(cn, true, JcaGapSizer.class.getClassLoader());
                try {
                    var ctor = c.getDeclaredConstructor();
                    ctor.setAccessible(true);
                    ctor.newInstance();
                    inst = "yes";
                } catch (Throwable t) {
                    inst = "ctor:" + t.getClass().getSimpleName();
                }
            } catch (Throwable t) {
                inst = "clinit:" + t.getClass().getSimpleName();
            }
        }
        System.out.println("CHK " + cn + " loads=" + loads + " instantiates=" + inst);
    }
}
