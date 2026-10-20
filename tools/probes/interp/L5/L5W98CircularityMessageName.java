// Interpreter round i4, wave 98, lane L5 -- the MESSAGE of a
// `ClassCircularityError` names the class in internal (slash) form.
//
// HotSpot 25 `SystemDictionary::resolve_with_circularity_detection` throws
// `THROW_MSG_NULL(ClassCircularityError, class_name->as_C_string())`; a
// Symbol's C string is the internal name, so a packaged class reads `p/A`.
// The cycle is detected while `p/A`'s supertype is being resolved for the
// second time on this thread, so the message names `p/A` (the class first
// asked for), and the error propagates out of every nested `loadClass`.
// Earlier waves only printed the TYPE (L5W29 `circular`) or used a
// default-package name (L5W38, L5W61), where both spellings agree; CratonVM's
// `linkage_throwable` dotted the name (`p.A`).
//
// Run (no setup):
//   javac -d out L5W98CircularityMessageName.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W98CircularityMessageName
//
// Expected HotSpot 25 output (from the JDK sources; not measured by this lane):
// HOTSPOT_EXPECTED_BEGIN
// class-cycle plain: java.lang.ClassCircularityError: p/A
// class-cycle parallel: java.lang.ClassCircularityError: p/A
// iface-cycle plain: java.lang.ClassCircularityError: p/A
// iface-cycle parallel: java.lang.ClassCircularityError: p/A
// deep-cycle plain: java.lang.ClassCircularityError: q/r/A
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.util.HashMap;
import java.util.Map;

public class L5W98CircularityMessageName {
    static final ClassDesc OBJECT = ClassDesc.of("java.lang.Object");
    static final int PUBLIC = 0x0001;
    static final int SUPER = 0x0020;
    static final int INTERFACE = 0x0200;
    static final int ABSTRACT = 0x0400;

    static byte[] cls(String name, String sup, int flags, String... ifaces) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(flags);
            cb.withSuperclass(sup == null ? OBJECT : ClassDesc.of(sup));
            ClassDesc[] ids = new ClassDesc[ifaces.length];
            for (int i = 0; i < ifaces.length; i++) {
                ids[i] = ClassDesc.of(ifaces[i]);
            }
            cb.withInterfaceSymbols(ids);
        });
    }

    static class Gen extends ClassLoader {
        final Map<String, byte[]> classes = new HashMap<>();

        Gen() {
            super(L5W98CircularityMessageName.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = classes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static final class ParallelGen extends Gen {
        static {
            registerAsParallelCapable();
        }
    }

    static void run(String label, Gen gen, String first, Map<String, byte[]> classes) {
        gen.classes.putAll(classes);
        try {
            Class<?> a = gen.loadClass(first);
            System.out.println(label + ": defined " + a.getName());
        } catch (Throwable t) {
            System.out.println(label + ": " + t);
        }
    }

    public static void main(String[] args) {
        Map<String, byte[]> classCycle = Map.of(
                "p.A", cls("p.A", "p.B", PUBLIC | SUPER),
                "p.B", cls("p.B", "p.A", PUBLIC | SUPER));
        Map<String, byte[]> ifaceCycle = Map.of(
                "p.A", cls("p.A", null, PUBLIC | INTERFACE | ABSTRACT, "p.B"),
                "p.B", cls("p.B", null, PUBLIC | INTERFACE | ABSTRACT, "p.A"));
        run("class-cycle plain", new Gen(), "p.A", classCycle);
        run("class-cycle parallel", new ParallelGen(), "p.A", classCycle);
        run("iface-cycle plain", new Gen(), "p.A", ifaceCycle);
        run("iface-cycle parallel", new ParallelGen(), "p.A", ifaceCycle);
        run("deep-cycle plain", new Gen(), "q.r.A", Map.of(
                "q.r.A", cls("q.r.A", "q.r.B", PUBLIC | SUPER),
                "q.r.B", cls("q.r.B", "q.r.C", PUBLIC | SUPER),
                "q.r.C", cls("q.r.C", "q.r.A", PUBLIC | SUPER)));
    }
}
