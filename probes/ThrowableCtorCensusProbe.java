import java.io.BufferedReader;
import java.io.FileReader;
import java.lang.reflect.Constructor;
import java.lang.reflect.Modifier;
import java.util.ArrayList;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Set;

/**
 * For every class in the throwable_classes list, print the JVM descriptor of
 * every DECLARED constructor with its access, so the registered four-descriptor
 * set can be diffed against what JDK 25 actually declares.
 *
 * Output, one line per class:
 *   <internal/name> | PUB:<desc>,<desc>,... | NONPUB:<desc>,... | DEADREG:<desc>,... | MISSREG:<desc>,...
 * where DEADREG is a registered descriptor that is NOT a public ctor, and
 * MISSREG is a public ctor that is NOT registered.
 */
public class ThrowableCtorCensusProbe {
    static final String[] REGISTERED = {
        "()V",
        "(Ljava/lang/String;)V",
        "(Ljava/lang/String;Ljava/lang/Throwable;)V",
        "(Ljava/lang/Throwable;)V",
    };

    static String desc(Constructor<?> c) {
        StringBuilder sb = new StringBuilder("(");
        for (Class<?> p : c.getParameterTypes()) sb.append(sig(p));
        return sb.append(")V").toString();
    }

    static String sig(Class<?> t) {
        if (t == boolean.class) return "Z";
        if (t == byte.class) return "B";
        if (t == char.class) return "C";
        if (t == short.class) return "S";
        if (t == int.class) return "I";
        if (t == long.class) return "J";
        if (t == float.class) return "F";
        if (t == double.class) return "D";
        if (t.isArray()) return "[" + sig(t.getComponentType());
        return "L" + t.getName().replace('.', '/') + ";";
    }

    public static void main(String[] args) throws Exception {
        List<String> names = new ArrayList<>();
        try (BufferedReader r = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = r.readLine()) != null) if (!line.isBlank()) names.add(line.trim());
        }
        int missing = 0, deadTotal = 0, missTotal = 0;
        for (String internal : names) {
            Class<?> k;
            try {
                k = Class.forName(internal.replace('/', '.'));
            } catch (Throwable t) {
                System.out.println(internal + " | ABSENT-ON-JDK25");
                missing++;
                continue;
            }
            Set<String> pub = new LinkedHashSet<>();
            Set<String> nonpub = new LinkedHashSet<>();
            for (Constructor<?> c : k.getDeclaredConstructors()) {
                if (Modifier.isPublic(c.getModifiers())) pub.add(desc(c));
                else nonpub.add(desc(c) + "[" + Modifier.toString(c.getModifiers()) + "]");
            }
            List<String> dead = new ArrayList<>();
            for (String d : REGISTERED) if (!pub.contains(d)) dead.add(d);
            List<String> miss = new ArrayList<>();
            for (String d : pub) {
                boolean found = false;
                for (String r : REGISTERED) if (r.equals(d)) found = true;
                if (!found) miss.add(d);
            }
            deadTotal += dead.size();
            missTotal += miss.size();
            System.out.println(internal
                + " | PUB:" + String.join(",", pub)
                + " | NONPUB:" + String.join(",", nonpub)
                + " | DEADREG:" + String.join(",", dead)
                + " | MISSREG:" + String.join(",", miss));
        }
        System.out.println("== classes=" + names.size() + " absent=" + missing
                + " deadRegisteredDescriptors=" + deadTotal
                + " unregisteredPublicCtors=" + missTotal);
    }
}
