// Interpreter round i1, wave 11, lane L2 — `checkcast` / `instanceof` resolve
// their class constant whatever the receiver is.
//
// JVMS 6.5: for a non-null operand "the named class, array, or interface type
// is resolved (5.4.3.1)" before the type test, so a class missing at run time
// is a NoClassDefFoundError whether the receiver or the target is an array.
// Before wave 11 CratonVM resolved only a non-array receiver against a
// non-array target and answered every array shape from names (`false` / a
// ClassCastException). See
// docs/internal/fixed-bugs/interpreter-L2-array-casts-skip-class-constant-resolution-FIXED-20260925.md.
//
// javac cannot compile a reference to a class that does not exist, so the
// referencing classes are generated with the java.lang.classfile API (final
// since JDK 24) and defined next to this one; `l2gen.Gone` is never defined.
// The `jit-*` rows call a fresh class's method 20000 times with null (which
// resolves nothing) before the one non-null call, so a compiled body meets the
// unresolved site first (the compiled-`instanceof` half:
// docs/internal/fixed-bugs/interpreter-L2-compiled-instanceof-cannot-raise-class-access-error-FIXED-20260925.md).
//
// Expected HotSpot 25 stdout:
//   instanceof int[] vs Gone: java.lang.NoClassDefFoundError: l2gen/Gone
//   instanceof String vs Gone[]: java.lang.NoClassDefFoundError: [Ll2gen/Gone;
//   instanceof int[] vs Gone[]: java.lang.NoClassDefFoundError: [Ll2gen/Gone;
//   checkcast Object[] vs Gone: java.lang.NoClassDefFoundError: l2gen/Gone
//   checkcast String vs Gone[]: java.lang.NoClassDefFoundError: [Ll2gen/Gone;
//   checkcast Object[] vs Gone[]: java.lang.NoClassDefFoundError: [Ll2gen/Gone;
//   instanceof null vs Gone: 0
//   checkcast null vs Gone[]: null
//   jit-instanceof String vs Gone: java.lang.NoClassDefFoundError: l2gen/Gone
//   jit-instanceof int[] vs Gone: java.lang.NoClassDefFoundError: l2gen/Gone
//   jit-checkcast Object[] vs Gone[]: java.lang.NoClassDefFoundError: [Ll2gen/Gone;
//
// No setup; compare with and without `--nojit`.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L2MissingCastTarget {
    static final ClassDesc GONE = ClassDesc.of("l2gen.Gone");
    static final MethodTypeDesc INT_OF_OBJECT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object);
    static final MethodTypeDesc OBJECT_OF_OBJECT =
            MethodTypeDesc.of(ConstantDescs.CD_Object, ConstantDescs.CD_Object);
    static int generated;

    /// A fresh class `L2MissingCastTargetGen<n>` with `inst`, `instArr`,
    /// `cast`, `castArr`: `o instanceof Gone`, `o instanceof Gone[]`,
    /// `(Gone) o`, `(Gone[]) o`.
    static Class<?> generate() throws Throwable {
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        byte[] bytes = ClassFile.of().build(
                ClassDesc.of("L2MissingCastTargetGen" + (generated++)), clb -> clb
                        .withFlags(ClassFile.ACC_PUBLIC)
                        .withMethodBody("inst", INT_OF_OBJECT, flags,
                                cb -> cb.aload(0).instanceOf(GONE).ireturn())
                        .withMethodBody("instArr", INT_OF_OBJECT, flags,
                                cb -> cb.aload(0).instanceOf(GONE.arrayType()).ireturn())
                        .withMethodBody("cast", OBJECT_OF_OBJECT, flags,
                                cb -> cb.aload(0).checkcast(GONE).areturn())
                        .withMethodBody("castArr", OBJECT_OF_OBJECT, flags,
                                cb -> cb.aload(0).checkcast(GONE.arrayType()).areturn()));
        return MethodHandles.lookup().defineClass(bytes);
    }

    static void row(String label, Class<?> gen, String method, Object arg) throws Throwable {
        Method m = gen.getMethod(method, Object.class);
        try {
            System.out.println(label + ": " + m.invoke(null, arg));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    /// 20000 null calls first: nothing resolves, and the method is hot.
    static void warmRow(String label, String method, Object arg) throws Throwable {
        Class<?> gen = generate();
        Method m = gen.getMethod(method, Object.class);
        for (int i = 0; i < 20_000; i++) {
            m.invoke(null, (Object) null);
        }
        row(label, gen, method, arg);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> gen = generate();
        row("instanceof int[] vs Gone", gen, "inst", new int[0]);
        row("instanceof String vs Gone[]", generate(), "instArr", "x");
        row("instanceof int[] vs Gone[]", generate(), "instArr", new int[0]);
        row("checkcast Object[] vs Gone", generate(), "cast", new Object[0]);
        row("checkcast String vs Gone[]", generate(), "castArr", "x");
        row("checkcast Object[] vs Gone[]", generate(), "castArr", new Object[0]);
        row("instanceof null vs Gone", generate(), "inst", null);
        row("checkcast null vs Gone[]", generate(), "castArr", null);
        warmRow("jit-instanceof String vs Gone", "inst", "x");
        warmRow("jit-instanceof int[] vs Gone", "inst", new int[0]);
        warmRow("jit-checkcast Object[] vs Gone[]", "castArr", new Object[0]);
    }
}
