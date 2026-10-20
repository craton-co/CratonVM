// Interpreter round i6, wave 124, lane L5 -- a child-first user loader defines,
// ON REQUEST, its own class under the name of a java.base class that is
// already loaded, and its own class's `invokestatic` / `getstatic` of a static
// member java.base's class lacks reaches the loader's copy (JVMS 5.3 / 5.4.3.1:
// the loader is the initiating loader and is asked for the name).
//
// The sibling of `L5W27JdkNamedOwnClass` row 7, which regressed to
// `NoSuchMethodError` in both modes when interpreter round i5 wave 100's
// "provably unresolvable" check judged the method reference against the
// built-in chain's guess (java.base's class) before the owner door asked the
// loader. Rows:
//   0 exported   invokestatic javax.security.auth.x500.X500Principal.v()  salt+1
//   1 exported   a second site, same name (resolved)                    salt+1
//   2 unexported invokestatic sun.security.x509.X500Name.v()            salt+2
//      (java.base's package is not exported to the unnamed module: the guess
//       was an IllegalAccessError under --jdk-only)
//   3 exported   getstatic javax.security.auth.x500.X500PrivateCredential.F  salt+3
// Only exception CLASSES are printed. Page
// docs/internal/fixed-bugs/interpreter-i124-L5-lazy-jdk-named-own-class-invokestatic-nsme-FIXED-20261224.md.
//
// Run (no setup):
//   javac -d out L5W124LazyJdkNamedOwnStatic.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W124LazyJdkNamedOwnStatic
//   (and with CRATONVM_GLOBAL_NAME_OWNER_UNGUESSED=0 for the old answer)
//
// HOTSPOT_EXPECTED_BEGIN
// A 0 lazy exported invokestatic: 101
// A 1 lazy exported invokestatic again: 101
// A 2 lazy unexported invokestatic: 102
// A 3 lazy exported getstatic: 103
// B 0 lazy exported invokestatic: 201
// B 1 lazy exported invokestatic again: 201
// B 2 lazy unexported invokestatic: 202
// B 3 lazy exported getstatic: 203
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.classfile.instruction.SwitchCase;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntUnaryOperator;

public class L5W124LazyJdkNamedOwnStatic {
    static final ClassDesc PRINCIPAL = ClassDesc.of("javax.security.auth.x500.X500Principal");
    static final ClassDesc NAME = ClassDesc.of("sun.security.x509.X500Name");
    static final ClassDesc CRED = ClassDesc.of("javax.security.auth.x500.X500PrivateCredential");
    static final ClassDesc USER = ClassDesc.of("l5w124.User");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final MethodTypeDesc INT = MethodTypeDesc.of(I);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// A public class with `static int v()` returning `v` and `static int F = f`.
    static byte[] owned(ClassDesc self, int v, int f) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("F", I, PUBLIC | STATIC)
                .withMethodBody("v", INT, PUBLIC | STATIC, cb -> cb.sipush(v).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.sipush(f).putstatic(self, "F", I).return_()));
    }

    static byte[] user() {
        List<Consumer<CodeBuilder>> arms = List.of(
                cb -> cb.invokestatic(PRINCIPAL, "v", INT),
                cb -> cb.invokestatic(PRINCIPAL, "v", INT),
                cb -> cb.invokestatic(NAME, "v", INT),
                cb -> cb.getstatic(CRED, "F", I));
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", MethodTypeDesc.of(I, I), PUBLIC, cb -> {
                    Label[] labels = new Label[arms.size()];
                    List<SwitchCase> cases = new ArrayList<>();
                    for (int i = 0; i < labels.length; i++) {
                        labels[i] = cb.newLabel();
                        cases.add(SwitchCase.of(i, labels[i]));
                    }
                    Label dflt = cb.newLabel();
                    cb.iload(1).tableswitch(0, labels.length - 1, dflt, cases);
                    for (int i = 0; i < labels.length; i++) {
                        cb.labelBinding(labels[i]);
                        arms.get(i).accept(cb);
                        cb.ireturn();
                    }
                    cb.labelBinding(dflt).iconst_m1().ireturn();
                }));
    }

    /// Child-first for its own names (defined on request), parent = platform.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader(int salt) {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("javax.security.auth.x500.X500Principal", owned(PRINCIPAL, salt + 1, salt + 9));
            bytes.put("sun.security.x509.X500Name", owned(NAME, salt + 2, salt + 9));
            bytes.put("javax.security.auth.x500.X500PrivateCredential", owned(CRED, salt + 9, salt + 3));
            bytes.put("l5w124.User", user());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static final String[] LABELS = {
        "0 lazy exported invokestatic",
        "1 lazy exported invokestatic again",
        "2 lazy unexported invokestatic",
        "3 lazy exported getstatic",
    };

    static void run(String tag, int salt) throws Exception {
        Loader loader = new Loader(salt);
        IntUnaryOperator op = (IntUnaryOperator) loader.loadClass("l5w124.User")
                .getConstructor().newInstance();
        for (int k = 0; k < LABELS.length; k++) {
            String label = tag + " " + LABELS[k];
            try {
                System.out.println(label + ": " + op.applyAsInt(k));
            } catch (Throwable t) {
                System.out.println(label + ": " + t.getClass().getName());
            }
        }
    }

    public static void main(String[] args) throws Exception {
        // java.base's copies loaded first, as an application would have them.
        Class.forName("javax.security.auth.x500.X500Principal");
        Class.forName("javax.security.auth.x500.X500PrivateCredential");
        Class.forName("sun.security.x509.X500Name", false, null);
        run("A", 100);
        run("B", 200);
    }
}
