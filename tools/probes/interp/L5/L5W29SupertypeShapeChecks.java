// Interpreter round i1, wave 29, lane L5 -- JVMS §5.3.5 step 3-4 and the
// class-file parser's supertype checks, at class definition:
//
//   final      `A extends F`, `F` final          -> IncompatibleClassChangeError
//   iface-sup  `A extends I`, `I` an interface   -> IncompatibleClassChangeError
//   class-iface `A implements C`, `C` a class    -> IncompatibleClassChangeError
//   circular   `A extends B`, `B extends A`       -> ClassCircularityError
//   control    `A extends P implements J`         -> defined
//
// Each case gets its own loader, which defines the generated classes (the
// java.lang.classfile API, final since JDK 24) only when asked, so every check
// runs at `loadClass` of `p.A`. The messages of the first two cases name only
// the classes; the class-iface message names the loader (with an identity
// hash) and the circular one is not asserted, so those two print the class.
//
// CratonVM (from the code, not run): `ClassManager::define_class_shared_with_options`
// (`classloading/src/class_manager.rs`) links a superclass or an interface
// without asking whether it IS a class / an interface, so `iface-sup` and
// `class-iface` are defined; `final` is left to the verifier
// (`verifier.rs` `verify_final_class_constraint`: a `VerifyError`, and none when
// verification is skipped); `circular` is the loading guard's
// "circular class hierarchy detected" `InvalidClassFile`, not a
// `ClassCircularityError`.
//
// Wave 29, `--jdk-only`: `supertype_shape_violation` makes the first three rows
// HotSpot's. Wave 31: `circular` is HotSpot's in every mode (the loader's
// re-entered define is refused;
// `docs/internal/fixed-bugs/interpreter-L5-a-circular-hierarchy-is-not-a-classcircularityerror-FIXED-20260928.md`).
// `--compatible` keeps the other three: expected `final: java.lang.VerifyError ...`
// (or `defined` when verification is skipped), `iface-sup` and `class-iface`
// `defined`.
//
// Run (no setup):
//   javac -d out L5W29SupertypeShapeChecks.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W29SupertypeShapeChecks
//
// Expected HotSpot 25 output (compare verbatim):
//   final: java.lang.IncompatibleClassChangeError: class p.A cannot inherit from final class p.F
//   iface-sup: java.lang.IncompatibleClassChangeError: class p.A has interface p.I as super class
//   class-iface: java.lang.IncompatibleClassChangeError
//   circular: java.lang.ClassCircularityError
//   control: defined p.A super=p.P interfaces=1

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.util.HashMap;
import java.util.Map;

public class L5W29SupertypeShapeChecks {
    static final ClassDesc OBJECT = ClassDesc.of("java.lang.Object");

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

    static final int PUBLIC = 0x0001;
    static final int FINAL = 0x0010;
    static final int SUPER = 0x0020;
    static final int INTERFACE = 0x0200;
    static final int ABSTRACT = 0x0400;

    static final class Gen extends ClassLoader {
        final Map<String, byte[]> classes = new HashMap<>();

        Gen() {
            super(L5W29SupertypeShapeChecks.class.getClassLoader());
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

    static void run(String label, boolean message, Map<String, byte[]> classes) {
        Gen gen = new Gen();
        gen.classes.putAll(classes);
        try {
            Class<?> a = gen.loadClass("p.A");
            System.out.println(label + ": defined " + a.getName() + " super="
                    + a.getSuperclass().getName() + " interfaces=" + a.getInterfaces().length);
        } catch (Throwable t) {
            System.out.println(label + ": " + (message ? t.toString() : t.getClass().getName()));
        }
    }

    public static void main(String[] args) {
        run("final", true, Map.of(
                "p.F", cls("p.F", null, PUBLIC | SUPER | FINAL),
                "p.A", cls("p.A", "p.F", PUBLIC | SUPER)));
        run("iface-sup", true, Map.of(
                "p.I", cls("p.I", null, PUBLIC | INTERFACE | ABSTRACT),
                "p.A", cls("p.A", "p.I", PUBLIC | SUPER)));
        run("class-iface", false, Map.of(
                "p.C", cls("p.C", null, PUBLIC | SUPER),
                "p.A", cls("p.A", null, PUBLIC | SUPER, "p.C")));
        run("circular", false, Map.of(
                "p.B", cls("p.B", "p.A", PUBLIC | SUPER),
                "p.A", cls("p.A", "p.B", PUBLIC | SUPER)));
        run("control", true, Map.of(
                "p.P", cls("p.P", null, PUBLIC | SUPER),
                "p.J", cls("p.J", null, PUBLIC | INTERFACE | ABSTRACT),
                "p.A", cls("p.A", "p.P", PUBLIC | SUPER, "p.J")));
    }
}
