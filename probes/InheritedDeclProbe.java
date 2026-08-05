import java.io.*;
import java.lang.reflect.*;
import java.util.*;

/**
 * Answer the question the schema-3 census cannot: when
 * `image_declaring_method` says a class is present but does NOT declare a
 * registered method, is that method nonetheless <em>inherited</em> — and with
 * what modifiers?
 *
 * The census asks the image about one class name. A native registered on
 * `sun/nio/ch/SocketDispatcher.close(Ljava/io/FileDescriptor;)V` comes back
 * `declared: false`, which reads as "dead registration". It is not: the method
 * is concrete bytecode on `sun.nio.ch.UnixDispatcher`, two frames up, and
 * CratonVM's receiver-driven dispatch finds the registration first — so the
 * row is a §1.4 SHADOW of inherited bytecode, not a dead entry. The two want
 * opposite dispositions, and nothing in the tooling could tell them apart.
 *
 * Runs on real HotSpot against the same image the census used. Reads one
 * `class<TAB>name<TAB>descriptor` triple per line and writes
 * `class<TAB>name<TAB>descriptor<TAB>verdict<TAB>declarer<TAB>modifiers`.
 *
 * Verdicts:
 *   ABSENT            the image has no such class
 *   DECLARED          the named class declares it (the census already knew)
 *   INHERITED         a superclass or interface declares it — the census's
 *                     `declared: false` was about the wrong class
 *   NOT-FOUND         genuinely nowhere in the hierarchy: a dead registration
 *   ERROR:<what>      the class could not be examined
 *
 * Usage: java InheritedDeclProbe <triples.tsv> [out.tsv]
 */
public final class InheritedDeclProbe {

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("usage: InheritedDeclProbe <triples.tsv> [out.tsv]");
            System.exit(2);
        }
        PrintWriter out = args.length > 1
                ? new PrintWriter(new BufferedWriter(new FileWriter(args[1])))
                : new PrintWriter(new BufferedWriter(new OutputStreamWriter(System.out)));

        int[] tally = new int[5];  // absent, declared, inherited, notfound, error
        try (BufferedReader in = new BufferedReader(new FileReader(args[0]))) {
            String line;
            while ((line = in.readLine()) != null) {
                if (line.isBlank() || line.startsWith("#")) continue;
                String[] f = line.split("\t");
                if (f.length < 3) continue;
                String[] r = examine(f[0], f[1], f[2]);
                switch (r[0]) {
                    case "ABSENT" -> tally[0]++;
                    case "DECLARED" -> tally[1]++;
                    case "INHERITED" -> tally[2]++;
                    case "NOT-FOUND" -> tally[3]++;
                    default -> tally[4]++;
                }
                out.println(f[0] + "\t" + f[1] + "\t" + f[2] + "\t"
                        + r[0] + "\t" + r[1] + "\t" + r[2]);
            }
        }
        out.flush();
        System.err.println("INHERITED-DECL absent=" + tally[0] + " declared=" + tally[1]
                + " inherited=" + tally[2] + " not_found=" + tally[3] + " error=" + tally[4]);
        if (args.length > 1) out.close();
    }

    /** @return {verdict, declaringClass, modifiers} */
    private static String[] examine(String internalName, String method, String descriptor) {
        // Boot loader first, then platform, then system: `java.sql`,
        // `java.desktop`'s service classes and the `jdk.compiler` internals are
        // in the image but NOT visible to the bootstrap loader, and resolving
        // only against `null` reports them ABSENT when they are merely
        // elsewhere on the module graph.
        String binary = internalName.replace('/', '.');
        Class<?> start = null;
        for (ClassLoader cl : new ClassLoader[] {
                null,
                ClassLoader.getPlatformClassLoader(),
                ClassLoader.getSystemClassLoader() }) {
            try {
                start = Class.forName(binary, false, cl);
                break;
            } catch (ClassNotFoundException | NoClassDefFoundError e) {
                // try the next loader
            } catch (Throwable t) {
                return new String[] {"ERROR:" + t.getClass().getSimpleName(), "-", "-"};
            }
        }
        if (start == null) {
            return new String[] {"ABSENT", "-", "-"};
        }

        try {
            // Breadth-first over the class chain, then interfaces: the first
            // declaration found is the one dispatch would reach.
            Deque<Class<?>> queue = new ArrayDeque<>();
            Set<Class<?>> seen = new HashSet<>();
            queue.add(start);
            boolean first = true;
            while (!queue.isEmpty()) {
                Class<?> c = queue.poll();
                if (!seen.add(c)) continue;
                Executable hit = find(c, method, descriptor);
                if (hit != null) {
                    return new String[] {
                        first ? "DECLARED" : "INHERITED",
                        c.getName(),
                        describe(hit)
                    };
                }
                first = false;
                if (c.getSuperclass() != null) queue.add(c.getSuperclass());
                queue.addAll(Arrays.asList(c.getInterfaces()));
            }
            return new String[] {"NOT-FOUND", "-", "-"};
        } catch (Throwable t) {
            return new String[] {"ERROR:" + t.getClass().getSimpleName(), "-", "-"};
        }
    }

    private static Executable find(Class<?> c, String method, String descriptor) {
        if (method.equals("<init>")) {
            for (Constructor<?> k : c.getDeclaredConstructors()) {
                if (descriptorOf(k.getParameterTypes(), void.class).equals(descriptor)) return k;
            }
            return null;
        }
        for (Method m : c.getDeclaredMethods()) {
            if (m.getName().equals(method)
                    && descriptorOf(m.getParameterTypes(), m.getReturnType()).equals(descriptor)) {
                return m;
            }
        }
        return null;
    }

    private static String describe(Executable e) {
        int m = e.getModifiers();
        List<String> parts = new ArrayList<>();
        if (Modifier.isNative(m)) parts.add("native");
        if (Modifier.isAbstract(m)) parts.add("abstract");
        if (!Modifier.isNative(m) && !Modifier.isAbstract(m)) parts.add("code");
        if (Modifier.isStatic(m)) parts.add("static");
        if (Modifier.isFinal(m)) parts.add("final");
        return String.join(",", parts);
    }

    private static String descriptorOf(Class<?>[] params, Class<?> ret) {
        StringBuilder sb = new StringBuilder("(");
        for (Class<?> p : params) sb.append(typeOf(p));
        return sb.append(')').append(typeOf(ret)).toString();
    }

    private static String typeOf(Class<?> t) {
        if (t.isArray()) return "[" + typeOf(t.getComponentType());
        if (!t.isPrimitive()) return "L" + t.getName().replace('.', '/') + ";";
        if (t == void.class) return "V";
        if (t == boolean.class) return "Z";
        if (t == byte.class) return "B";
        if (t == char.class) return "C";
        if (t == short.class) return "S";
        if (t == int.class) return "I";
        if (t == long.class) return "J";
        if (t == float.class) return "F";
        return "D";  // double
    }
}
