/*
 * Interpreter round i4, wave 84, lane L3: the types of a record's components,
 * resolved through the RECORD's loader.
 *
 * HotSpot's JVM_GetRecordComponents builds each RecordComponent with
 * `find_java_mirror_for_type(type, holder, NCDFError)`: the type is what the
 * record's own loader resolves the name to, and a name it cannot load is a
 * NoClassDefFoundError (internal name, the loader's ClassNotFoundException as
 * its cause) thrown out of getRecordComponents(). CratonVM resolved the
 * descriptor loader-blind (an isolating loader's record could report the
 * application loader's copy of the type) and minted a stand-in mirror for a
 * type nobody could load.
 *
 * Run (both modes must print the HotSpot lines):
 *   javac -d out L3W84RecordComponentTypes.java
 *   cratonvm --jdk-only   -cp out L3W84RecordComponentTypes
 *   cratonvm --compatible -cp out L3W84RecordComponentTypes
 *   java                  -cp out L3W84RecordComponentTypes   (HotSpot 25)
 *
 * Expected HotSpot 25 output:
 *   app: kept:L3W84RecordComponentTypes$Kept x:int
 *   isolated: kept:L3W84RecordComponentTypes$Kept same-loader=true
 *   missing: java.lang.NoClassDefFoundError: L3W84RecordComponentTypes$Gone
 *   cause: java.lang.ClassNotFoundException: refused L3W84RecordComponentTypes$Gone
 */
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.RecordComponent;

public class L3W84RecordComponentTypes {
    static class Kept {}

    static class Gone {}

    record Uses(Kept kept, int x) {}

    record OnlyKept(Kept kept) {}

    record UsesGone(Gone gone) {}

    /** Defines this probe's nested classes itself; refuses every `$Gone`. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.endsWith("$Gone")) {
                    throw new ClassNotFoundException("refused " + name);
                }
                if (name.startsWith("L3W84RecordComponentTypes$")) {
                    String res = name.replace('.', '/') + ".class";
                    try (InputStream in = getParent().getResourceAsStream(res)) {
                        byte[] b = in.readAllBytes();
                        return defineClass(name, b, 0, b.length);
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static String describe(RecordComponent[] rcs) {
        StringBuilder sb = new StringBuilder();
        for (RecordComponent rc : rcs) {
            if (sb.length() > 0) {
                sb.append(' ');
            }
            sb.append(rc.getName()).append(':').append(rc.getType().getName());
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // The application loader's copy of Kept is loaded first, so a
        // loader-blind lookup has a wrong answer to give below.
        System.out.println("app: " + describe(Uses.class.getRecordComponents()));

        Isolating loader = new Isolating(L3W84RecordComponentTypes.class.getClassLoader());
        Class<?> onlyKept = loader.loadClass("L3W84RecordComponentTypes$OnlyKept");
        RecordComponent[] rcs = onlyKept.getRecordComponents();
        System.out.println("isolated: " + describe(rcs)
                + " same-loader=" + (rcs[0].getType().getClassLoader() == loader));

        Class<?> usesGone = loader.loadClass("L3W84RecordComponentTypes$UsesGone");
        try {
            System.out.println("missing: answered " + describe(usesGone.getRecordComponents()));
        } catch (Throwable t) {
            System.out.println("missing: " + t);
            System.out.println("cause: " + t.getCause());
        }
    }
}
