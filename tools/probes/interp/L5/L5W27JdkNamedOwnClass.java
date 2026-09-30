// Interpreter round i1, wave 27, lane L5 — a user loader's OWN class under a
// JDK-looking name (`javax.*`, `jdk.*`, `sun.*`, `com.sun.*`) is what that
// loader's references to the name resolve to (JVMS §5.3 / §5.4.3.1: the
// referencing class's defining loader is the initiating loader, and a class it
// has defined is in its dictionary), even when the JDK has a same-named class
// and even when another user loader defined its own copy too.
//
// The sharper successor of `L5W26SerializationAccessorSpoof` rows 3-5: that
// probe's `jdk.internal.reflect.SerializationConstructorAccessorImpl` no longer
// exists in JDK 25, and each of its JDK-named classes had exactly ONE
// definition in the process, so CratonVM's loader-blind flat lookup
// (`ClassManager::resolve_fast_path_class_id`'s lone-user-loader answer)
// happened to pick the right class. Here every name is either a JDK class too
// (`X500PrivateCredential`, `X500Principal`: the flat lookup answers
// java.base's) or is defined by TWO loaders (`L5Only`: the flat lookup is
// ambiguous and answers nothing).
//
// Two child-first loaders A (salt 100) and B (salt 200) each define, in
// `javax.security.auth.x500` (not a package CratonVM's define-time guard
// `is_prohibited_package_name` refuses):
//   X500PrivateCredential  own copy of a java.base class; defined up front
//   L5Only                 not in the JDK;                  defined up front
//   L5Lazy                 not in the JDK;                  defined on first request
//   X500Principal          own copy of a java.base class;   defined on first request
// and `l5n.User`, whose `applyAsInt(k)` performs:
//   0 invokestatic  X500PrivateCredential.v()          salt+1
//   1 getstatic     X500PrivateCredential.F            salt+2
//   2 ldc           X500PrivateCredential; getClassLoader() is the user loader -> 1
//   3 new X500PrivateCredential; <init>(); invokevirtual w()   salt+3
//   4 invokestatic  L5Only.v()                         salt+4
//   5 checkcast     L5Only of an L5Only instance      -> salt+7 (instance field)
//   6 invokestatic  L5Lazy.v()                         salt+5
//   7 invokestatic  X500Principal.v()                  salt+6
// Only exception CLASSES are printed.
//
// Run (no setup):
//   javac -d out L5W27JdkNamedOwnClass.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W27JdkNamedOwnClass
//
// Expected HotSpot 25 output (compare verbatim):
//   A own jdk class, invokestatic: 101
//   A own jdk class, getstatic: 102
//   A own jdk class, ldc loader: 1
//   A own jdk class, new+invokevirtual: 103
//   A two-loader name, invokestatic: 104
//   A two-loader name, checkcast: 107
//   A lazy name, invokestatic: 105
//   A lazy jdk class, invokestatic: 106
//   B own jdk class, invokestatic: 201
//   B own jdk class, getstatic: 202
//   B own jdk class, ldc loader: 1
//   B own jdk class, new+invokevirtual: 203
//   B two-loader name, invokestatic: 204
//   B two-loader name, checkcast: 207
//   B lazy name, invokestatic: 205
//   B lazy jdk class, invokestatic: 206
//
// CratonVM before wave 27 (from the code, not run): `is_global_resolution_namespace`
// sent every `javax/` reference past the loader's own definitions
// (`lookup_loader_initiated` / `lookup_loader_defined_exact` answered `None`
// for it), so rows 0-3 bound java.base's `X500PrivateCredential`
// (`NoSuchMethodError` / `NoSuchFieldError`, `ldc loader: 0`) and rows 4-5
// found no unique class (`NoClassDefFoundError`). Wave 27, lane L5: those two
// lookups (and `class_resolved_without_loading`, the threadless field
// resolver) answer the loader's own exact definition of a non-`java/` global
// name first — HotSpot's dictionary hit. Rows 6-7 (the loader has NOT defined
// the name yet) still differ: CratonVM does not ask a user loader for a
// `javax/` name at all (`drive_defining_loader_load` declines the global
// namespaces), so row 6 is `NoClassDefFoundError` and row 7 binds java.base's
// class (`NoSuchMethodError`). See
// docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md.
// Wave 27 fixed row 6 (the after-miss ask). Wave 28 (lane L5): under
// `--jdk-only` a loader that overrides `loadClass` (this probe's does) is
// asked for a JDK-global name before the global route
// (`constants.rs` `drive_loader_for_global_name`, reached here through
// `execute_invokestatic`'s owner arm), so row 7 is expected as HotSpot there
// (106 / 206); `--compatible` keeps `NoSuchMethodError` for row 7.

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

public class L5W27JdkNamedOwnClass {
    static final String PKG = "javax.security.auth.x500.";
    static final ClassDesc CRED = ClassDesc.of(PKG + "X500PrivateCredential");
    static final ClassDesc PRINCIPAL = ClassDesc.of(PKG + "X500Principal");
    static final ClassDesc ONLY = ClassDesc.of(PKG + "L5Only");
    static final ClassDesc LAZY = ClassDesc.of(PKG + "L5Lazy");
    static final ClassDesc USER = ClassDesc.of("l5n.User");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final MethodTypeDesc INT = MethodTypeDesc.of(I);
    static final MethodTypeDesc INT_INT = MethodTypeDesc.of(I, I);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// A public class with a public no-arg constructor, `static int v()`
    /// returning `v`, `static int F = f`, `int w()` returning `w` and an
    /// instance field `int g = g`.
    static byte[] owned(ClassDesc self, int v, int f, int w, int g) {
        return ClassFile.of().build(self, clb -> clb
                .withFlags(PUBLIC)
                .withField("F", I, PUBLIC | STATIC)
                .withField("g", I, PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .aload(0).sipush(g).putfield(self, "g", I)
                                .return_())
                .withMethodBody("v", INT, PUBLIC | STATIC, cb -> cb.sipush(v).ireturn())
                .withMethodBody("w", INT, PUBLIC, cb -> cb.sipush(w).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.sipush(f).putstatic(self, "F", I).return_()));
    }

    static void arm(CodeBuilder cb, Label at, Consumer<CodeBuilder> body) {
        cb.labelBinding(at);
        body.accept(cb);
        cb.ireturn();
    }

    static byte[] user() {
        ClassDesc cls = ConstantDescs.CD_Class;
        ClassDesc loader = ClassDesc.of("java.lang.ClassLoader");
        List<Consumer<CodeBuilder>> arms = List.of(
                cb -> cb.invokestatic(CRED, "v", INT),
                cb -> cb.getstatic(CRED, "F", I),
                cb -> {
                    Label isNull = cb.newLabel();
                    Label done = cb.newLabel();
                    cb.ldc(CRED)
                            .invokevirtual(cls, "getClassLoader", MethodTypeDesc.of(loader))
                            .ldc(USER)
                            .invokevirtual(cls, "getClassLoader", MethodTypeDesc.of(loader))
                            .if_acmpne(isNull)
                            .iconst_1().goto_(done)
                            .labelBinding(isNull).iconst_0()
                            .labelBinding(done);
                },
                cb -> cb.new_(CRED).dup()
                        .invokespecial(CRED, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .invokevirtual(CRED, "w", INT),
                cb -> cb.invokestatic(ONLY, "v", INT),
                cb -> cb.new_(ONLY).dup()
                        .invokespecial(ONLY, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .checkcast(ConstantDescs.CD_Object)
                        .checkcast(ONLY)
                        .getfield(ONLY, "g", I),
                cb -> cb.invokestatic(LAZY, "v", INT),
                cb -> cb.invokestatic(PRINCIPAL, "v", INT));
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    Label[] labels = new Label[arms.size()];
                    List<SwitchCase> cases = new ArrayList<>();
                    for (int i = 0; i < labels.length; i++) {
                        labels[i] = cb.newLabel();
                        cases.add(SwitchCase.of(i, labels[i]));
                    }
                    Label dflt = cb.newLabel();
                    cb.iload(1).tableswitch(0, labels.length - 1, dflt, cases);
                    for (int i = 0; i < labels.length; i++) {
                        arm(cb, labels[i], arms.get(i));
                    }
                    cb.labelBinding(dflt).iconst_m1().ireturn();
                }));
    }

    /// Child-first for its own names, parent = the platform loader.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader(int salt) {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put(PKG + "X500PrivateCredential",
                    owned(CRED, salt + 1, salt + 2, salt + 3, salt + 8));
            bytes.put(PKG + "L5Only", owned(ONLY, salt + 4, salt + 9, salt + 9, salt + 7));
            bytes.put(PKG + "L5Lazy", owned(LAZY, salt + 5, salt + 9, salt + 9, salt + 9));
            bytes.put(PKG + "X500Principal",
                    owned(PRINCIPAL, salt + 6, salt + 9, salt + 9, salt + 9));
            bytes.put("l5n.User", user());
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
        "own jdk class, invokestatic",
        "own jdk class, getstatic",
        "own jdk class, ldc loader",
        "own jdk class, new+invokevirtual",
        "two-loader name, invokestatic",
        "two-loader name, checkcast",
        "lazy name, invokestatic",
        "lazy jdk class, invokestatic",
    };

    static IntUnaryOperator prepare(int salt) throws Exception {
        Loader loader = new Loader(salt);
        loader.loadClass(PKG + "X500PrivateCredential");
        loader.loadClass(PKG + "L5Only");
        Class<?> user = loader.loadClass("l5n.User");
        return (IntUnaryOperator) user.getConstructor().newInstance();
    }

    static void run(String tag, IntUnaryOperator op) {
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
        // java.base's copies are loaded first, as an application would have
        // them: the flat lookup's built-in chain then answers them by name.
        Class.forName("javax.security.auth.x500.X500PrivateCredential");
        Class.forName("javax.security.auth.x500.X500Principal");
        IntUnaryOperator a = prepare(100);
        IntUnaryOperator b = prepare(200);
        run("A", a);
        run("B", b);
    }
}
