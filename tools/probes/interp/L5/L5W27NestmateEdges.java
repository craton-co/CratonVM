// Interpreter round i1, wave 27, lane L5 — JVMS §5.4.4 nestmate edges of the
// wave-26 member-access checks (enforced under `--jdk-only`).
//
//   1  A hidden class defined with `NESTMATE` through a Lookup on a NESTED
//      class (`L5W27NestmateEdges$Inner`) joins the lookup class's NEST, whose
//      host is the OUTER class (JEP 371: `lookupClass().getNestHost()`), so it
//      may call a private method of the lookup class and one of the outer
//      class. HotSpot: nest host `L5W27NestmateEdges`, 7, 9.
//   2  A nested class whose NestHost is the same-named class of ANOTHER loader
//      is not that class's nestmate: the host resolved through the member's
//      loader is in a different run-time package, so the member is its own
//      nest host (`InstanceKlass::nest_host`) and its `getstatic` of the
//      host's private field is an `IllegalAccessError`. Loader L1 defines
//      `l5n2.Outer$In` (NestHost `l5n2.Outer`) and delegates `l5n2.Outer` to
//      L2, which defines it with `NestMembers: l5n2.Outer$In`.
//   3  Control: the ordinary javac nest (`Inner` reading `Outer`'s private
//      field through a direct `getstatic`, javac 11+): 5.
//
// Run (no setup):
//   javac -d out L5W27NestmateEdges.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W27NestmateEdges
//
// Expected HotSpot 25 output (compare verbatim):
//   hidden nest host: L5W27NestmateEdges
//   hidden -> lookup class private: 7
//   hidden -> outer private: 9
//   cross-loader nest claim: java.lang.IllegalAccessError
//   javac nest: 5
//
// CratonVM before wave 27 (from the code, not run): `ClassLoader.defineClass0`
// (`native-builtins/src/lang_system.rs` `native_classloader_define_class0`,
// the path `Lookup.defineHiddenClass`'s real bytecode reaches) named the
// LOOKUP class as the nest host, so row 1 read `L5W27NestmateEdges$Inner` and,
// under `--jdk-only`, `hidden -> outer private` was an `IllegalAccessError`
// (the Lookup-native path `lookup_define.rs` already resolved the host). Row 2
// printed `5` in every mode: `access_control::are_nestmates` compared nest-host
// NAMES and confirmed a claim through a loader-blind `find_by_name`. Wave 27
// (lane L5): `--jdk-only` prints HotSpot's rows; `--compatible` admits row 2
// (member checks are counted there, not enforced) and prints `5`.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.IntSupplier;

public class L5W27NestmateEdges {
    private static int outerSecret() {
        return 9;
    }

    private static int outerField = 5;

    static final class Inner {
        private static int secret() {
            return 7;
        }

        static int javacNest() {
            return outerField;
        }

        static Object[] hidden() throws Throwable {
            ClassDesc self = ClassDesc.of("L5W27NestmateEdges$Inner$H");
            ClassDesc inner = ClassDesc.of("L5W27NestmateEdges$Inner");
            ClassDesc outer = ClassDesc.of("L5W27NestmateEdges");
            MethodTypeDesc intDesc = MethodTypeDesc.of(ConstantDescs.CD_int);
            byte[] bytes = ClassFile.of().build(self, clb -> clb
                    .withFlags(ClassFile.ACC_PUBLIC)
                    .withMethodBody("a", intDesc, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                            cb -> cb.invokestatic(inner, "secret", intDesc).ireturn())
                    .withMethodBody("b", intDesc, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                            cb -> cb.invokestatic(outer, "outerSecret", intDesc).ireturn()));
            MethodHandles.Lookup hl = MethodHandles.lookup()
                    .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE);
            Class<?> h = hl.lookupClass();
            MethodType mt = MethodType.methodType(int.class);
            return new Object[] {
                h.getNestHost().getName(),
                hl.findStatic(h, "a", mt),
                hl.findStatic(h, "b", mt),
            };
        }
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, ClassLoader> delegate = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                ClassLoader d = delegate.get(name);
                if (d != null) {
                    return d.loadClass(name);
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static String crossLoaderNestClaim() throws Throwable {
        ClassDesc outer = ClassDesc.of("l5n2.Outer");
        ClassDesc in = ClassDesc.of("l5n2.Outer$In");
        byte[] outerBytes = ClassFile.of().build(outer, clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .with(java.lang.classfile.attribute.NestMembersAttribute.ofSymbols(List.of(in)))
                .withField("s", ConstantDescs.CD_int, ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void,
                        ClassFile.ACC_STATIC,
                        cb -> cb.iconst_5().putstatic(outer, "s", ConstantDescs.CD_int).return_()));
        byte[] inBytes = ClassFile.of().build(in, clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .with(java.lang.classfile.attribute.NestHostAttribute.of(outer))
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntSupplier"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("getAsInt", MethodTypeDesc.of(ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC,
                        cb -> cb.getstatic(outer, "s", ConstantDescs.CD_int).ireturn()));
        Loader l2 = new Loader();
        l2.bytes.put("l5n2.Outer", outerBytes);
        Loader l1 = new Loader();
        l1.bytes.put("l5n2.Outer$In", inBytes);
        l1.delegate.put("l5n2.Outer", l2);
        IntSupplier s = (IntSupplier) l1.loadClass("l5n2.Outer$In").getConstructor().newInstance();
        try {
            return String.valueOf(s.getAsInt());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) throws Throwable {
        Object[] h = Inner.hidden();
        System.out.println("hidden nest host: " + h[0]);
        for (int i = 1; i <= 2; i++) {
            String label = i == 1 ? "hidden -> lookup class private" : "hidden -> outer private";
            try {
                System.out.println(label + ": "
                        + (int) ((java.lang.invoke.MethodHandle) h[i]).invokeExact());
            } catch (Throwable t) {
                System.out.println(label + ": " + t.getClass().getName());
            }
        }
        System.out.println("cross-loader nest claim: " + crossLoaderNestClaim());
        System.out.println("javac nest: " + Inner.javacNest());
    }
}
