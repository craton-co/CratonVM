import java.util.Arrays;

/**
 * Sibling check for the DERENCODABLE-SEALED-20260801 fix: `getNestMembers0()`
 * shares `resolve_nestmate_via_defining_loader` with `getPermittedSubclasses0()`,
 * so a bootstrap-loaded nest HOST had the same passive-lookup hole — it could
 * only report the nest members that some earlier code happened to have loaded.
 *
 * Prints the nest-member count for a few JDK-owned nest hosts, in a process
 * that has deliberately not touched any of their members. Compare against real
 * HotSpot.
 */
public class NestMembersBootstrapProbe {

    static void report(String name) {
        try {
            Class<?> c = Class.forName(name);
            Class<?>[] m = c.getNestMembers();
            String[] names = Arrays.stream(m)
                    .map(x -> x == null ? "<NULL>" : x.getName())
                    .sorted()
                    .toArray(String[]::new);
            System.out.println("NEST " + name + " n=" + m.length + " " + Arrays.toString(names));
        } catch (Throwable t) {
            System.out.println("NEST " + name + " THREW " + t);
        }
    }

    public static void main(String[] args) {
        for (String n : new String[] {
                "java.lang.Character",
                "java.util.Map",
                "java.lang.ProcessBuilder",
                "java.util.concurrent.ConcurrentHashMap",
                "java.security.KeyPair",
        }) {
            report(n);
        }
        System.out.println("PROBE-DONE");
    }
}
