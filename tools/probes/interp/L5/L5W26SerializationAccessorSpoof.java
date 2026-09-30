// Interpreter round i1, wave 26, lane L5 — HotSpot's access-check exemption
// for `jdk.internal.reflect.SerializationConstructorAccessorImpl` subclasses
// (`Reflection::verify_class_access` / `verify_member_access`) names THE
// bootstrap class (`vmClasses::reflect_SerializationConstructorAccessorImpl_klass`),
// not a name: a user loader may define its own class under that binary name
// (only `java.*` is a prohibited package), and a subclass of THAT gets no
// exemption.
//
// A child-first loader defines its own `jdk.internal.reflect.SerializationConstructorAccessorImpl`
// and `l5sp.Evil extends` it; `Evil.applyAsInt(k)` then performs:
//   0  `ldc l5acc2.Hidden` (a package-private class of another package; the
//      class-constant check, enforced in every mode since wave 10)
//   1  `getstatic l5acc2.Secret.P` (a private static; the member check,
//      enforced under `--jdk-only` since wave 26)
//   2  `getstatic l5acc2.Secret.F` (public control)
// and three accesses HotSpot ADMITS (lane L5b): classes the same loader
// defined under JDK names are in that loader's unnamed module, so no export
// clause applies to them, whatever package they are in —
//   3  `getstatic jdk/internal/reflect/L5Helper.X` (a public class of the loader in
//      java.base's non-exported package `jdk.internal.reflect`; CratonVM's define-time
//      guard (`class_manager.rs` `is_prohibited_package_name`, stricter than
//      HotSpot, which prohibits only `java.*`) refuses a user loader's class
//      in `jdk.internal.misc` but exempts `jdk.internal.reflect`)
//   4  `invokestatic jdk/internal/reflect/L5Helper.m()`
//   5  `invokestatic jdk/internal/reflect/SerializationConstructorAccessorImpl.sm()`
//      (Evil's own superclass, named like java.base's class)
// `Evil`'s constructor (`invokespecial` of its superclass's `<init>`) is a
// fourth: HotSpot runs it. Only exception CLASSES are printed.
//
// Run (no setup):
//   javac -d out L5W26SerializationAccessorSpoof.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W26SerializationAccessorSpoof
//
// Expected HotSpot 25 output (compare verbatim):
//   spoof loader: L5W26SerializationAccessorSpoof$Loader
//   class constant: java.lang.IllegalAccessError
//   private static: java.lang.IllegalAccessError
//   public static: 11
//   jdk-named class, getstatic: 21
//   jdk-named class, invokestatic: 22
//   jdk-named superclass, invokestatic: 23
//
// `--compatible` (member checks counted, not enforced) prints `private
// static: 7` and matches the other rows.
//
// CratonVM before wave 26 (from the code, not run): the exemption was asked by
// NAME (`ClassManager::is_subclass_of_by_name`, loader-blind), so the
// class-constant row printed `class constant: 0` in every mode. Now asked by
// identity (`constants::extends_serialization_constructor_accessor`). If the
// first line differs (`spoof loader: null`), `Evil`'s superclass resolved to
// java.base's class instead of its own loader's — a loader-namespace defect
// (`jdk/` names routed globally), not this check.
//
// Wave-26 host run (lane L5 merged): `--jdk-only` died in `Evil.<init>` with
// `IllegalAccessError: class l5sp.Evil (in unnamed module) cannot access class
// jdk.internal.reflect.SerializationConstructorAccessorImpl (in module
// java.base) because module java.base does not export jdk.internal.reflect to
// unnamed module`: the new owner-class check read the user loader's class as
// a java.base member (membership was attributed by package name) and/or
// judged java.base's same-named class. Lane L5b: `access_control::member_module_of`
// and `field_access::owner_in_referencing_namespace` /
// `owner_foreign_to_referencing_loader`. Rows 3-5 then depend on CratonVM
// resolving a `jdk/` name through the user loader, which it does not
// (`is_global_resolution_namespace`): row 5 is expected to differ (the
// reference binds java.base's class, which has no `sm`), and rows 3-4 match
// only through the flat lookup's lone-user-loader answer. See
// docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md.
//
// Wave 27 (lane L5): rows 3-5 matched on the wave-26 host run in every mode,
// row 5 included. JDK 25 has no `jdk.internal.reflect.SerializationConstructorAccessorImpl`
// and no `L5Helper`, so each JDK-named class here has one definition in the
// process and the flat lookup's lone-user-loader answer is right. The sharper
// probe is `L5W27JdkNamedOwnClass` (a name java.base also has; a name two
// loaders define).

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

public class L5W26SerializationAccessorSpoof {
    static final ClassDesc SCAI =
            ClassDesc.of("jdk.internal.reflect.SerializationConstructorAccessorImpl");
    static final ClassDesc SECRET = ClassDesc.of("l5acc2.Secret");
    static final ClassDesc HIDDEN = ClassDesc.of("l5acc2.Hidden");
    static final ClassDesc EVIL = ClassDesc.of("l5sp.Evil");
    static final ClassDesc HELPER = ClassDesc.of("jdk.internal.reflect.L5Helper");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    static byte[] scai() {
        return ClassFile.of().build(SCAI, clb -> clb
                .withFlags(PUBLIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("sm", INT, PUBLIC | STATIC, cb -> cb.bipush(23).ireturn()));
    }

    /// A public class of the spoof loader in java.base's non-exported
    /// package `jdk.internal.reflect`.
    static byte[] helper() {
        return ClassFile.of().build(HELPER, clb -> clb
                .withFlags(PUBLIC)
                .withField("X", I, PUBLIC | STATIC)
                .withMethodBody("m", INT, PUBLIC | STATIC, cb -> cb.bipush(22).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(21).putstatic(HELPER, "X", I).return_()));
    }

    static byte[] secret() {
        return ClassFile.of().build(SECRET, clb -> clb
                .withFlags(PUBLIC)
                .withField("P", I, ClassFile.ACC_PRIVATE | STATIC)
                .withField("F", I, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(7).putstatic(SECRET, "P", I)
                                .bipush(11).putstatic(SECRET, "F", I)
                                .return_()));
    }

    static byte[] hidden() {
        return ClassFile.of().build(HIDDEN, clb -> clb.withFlags(0));
    }

    static void arm(CodeBuilder cb, Label at, Consumer<CodeBuilder> body) {
        cb.labelBinding(at);
        body.accept(cb);
        cb.ireturn();
    }

    static byte[] evil() {
        List<Consumer<CodeBuilder>> arms = List.of(
                cb -> cb.ldc(HIDDEN).pop().iconst_0(),
                cb -> cb.getstatic(SECRET, "P", I),
                cb -> cb.getstatic(SECRET, "F", I),
                cb -> cb.getstatic(HELPER, "X", I),
                cb -> cb.invokestatic(HELPER, "m", INT),
                cb -> cb.invokestatic(SCAI, "sm", INT));
        return ClassFile.of().build(EVIL, clb -> clb
                .withFlags(PUBLIC)
                .withSuperclass(SCAI)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(SCAI, ConstantDescs.INIT_NAME,
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

    /// Child-first for its own names, so `Evil`'s superclass is ITS
    /// `SerializationConstructorAccessorImpl`, not java.base's.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("jdk.internal.reflect.SerializationConstructorAccessorImpl", scai());
            bytes.put("l5acc2.Secret", secret());
            bytes.put("l5acc2.Hidden", hidden());
            bytes.put("l5sp.Evil", evil());
            bytes.put("jdk.internal.reflect.L5Helper", helper());
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

    static void row(String label, IntUnaryOperator op, int k) {
        try {
            System.out.println(label + ": " + op.applyAsInt(k));
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName());
        }
    }

    public static void main(String[] args) throws Exception {
        Loader loader = new Loader();
        Class<?> evil = loader.loadClass("l5sp.Evil");
        loader.loadClass("jdk.internal.reflect.L5Helper");
        ClassLoader spoofLoader = evil.getSuperclass().getClassLoader();
        System.out.println("spoof loader: "
                + (spoofLoader == null ? "null" : spoofLoader.getClass().getName()));
        IntUnaryOperator op = (IntUnaryOperator) evil.getConstructor().newInstance();
        row("class constant", op, 0);
        row("private static", op, 1);
        row("public static", op, 2);
        row("jdk-named class, getstatic", op, 3);
        row("jdk-named class, invokestatic", op, 4);
        row("jdk-named superclass, invokestatic", op, 5);
    }
}
