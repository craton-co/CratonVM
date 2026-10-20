// Interpreter round i1, wave 8, lane L2 — class-reference access checks
// (rows extended in wave 9).
//
// JVMS 5.4.3.1 / 5.4.4: resolving a symbolic class reference checks that the
// referencing class may access the named class (public, or the same runtime
// package). HotSpot runs that check for EVERY instruction that resolves a
// class constant (`ConstantPool::klass_at_impl` -> `verify_constant_pool_resolve`),
// so `new`, `anewarray`, `multianewarray`, `ldc`, and a `checkcast` /
// `instanceof` of a NON-NULL operand, naming a package-private class from
// another package, all throw `IllegalAccessError`. See
// docs/internal/fixed-bugs/interpreter-L2-class-access-checked-only-by-new-FIXED-20260925.md.
//
// javac cannot emit such a reference, so this probe generates the bytecode
// with the java.lang.classfile API (final since JDK 24) and defines it next to
// itself with `MethodHandles.lookup().defineClass`. The target is
// `java.util.ImmutableCollections$ListN`: package-private, in an EXPORTED
// package of java.base, so module access is not the question.
//
// `checkcast` / `instanceof` of a NULL reference are controls: HotSpot's
// interpreter does not resolve the class for a null operand, so both print
// `ok` on HotSpot. `new` is the other control: CratonVM has always checked it.
//
// Expected HotSpot 25 stdout:
//   new: java.lang.IllegalAccessError
//   anewarray: java.lang.IllegalAccessError
//   multianewarray: java.lang.IllegalAccessError
//   ldc: java.lang.IllegalAccessError
//   checkcast: java.lang.IllegalAccessError
//   instanceof: java.lang.IllegalAccessError
//   checkcast-array: java.lang.IllegalAccessError
//   checkcast-null: ok
//   instanceof-null: ok
//   instanceof-array-receiver: java.lang.IllegalAccessError
//   checkcast-array-receiver: java.lang.IllegalAccessError
//
// No setup: CratonVM matches HotSpot row for row in every mode (wave 9
// enforced the non-`new` rows under `--jdk-only`, wave 10 in `--compatible`
// too). The two `-array-receiver` rows (wave 11) resolve the class constant
// for an ARRAY operand, which CratonVM answered from names before
// (docs/internal/fixed-bugs/interpreter-L2-array-casts-skip-class-constant-resolution-FIXED-20260925.md).
// Each row runs once, i.e. interpreted in the usual tiering; a COMPILED
// `instanceof` at an unbound site raises too since wave 11
// (docs/internal/fixed-bugs/interpreter-L2-compiled-instanceof-cannot-raise-class-access-error-FIXED-20260925.md).

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Consumer;

public class L2ClassAccess {
    static final ClassDesc HIDDEN = ClassDesc.of("java.util.ImmutableCollections$ListN");

    public static void main(String[] args) throws Throwable {
        run("new", "New", cb -> cb.new_(HIDDEN).pop().return_());
        run("anewarray", "Anew", cb -> cb.iconst_0().anewarray(HIDDEN).pop().return_());
        run("multianewarray", "Multi",
                cb -> cb.iconst_0().multianewarray(HIDDEN.arrayType(2), 1).pop().return_());
        run("ldc", "Ldc", cb -> cb.ldc(HIDDEN).pop().return_());
        run("checkcast", "Cast",
                cb -> cb.ldc("x").checkcast(HIDDEN).pop().return_());
        run("instanceof", "Inst",
                cb -> cb.ldc("x").instanceOf(HIDDEN).pop().return_());
        run("checkcast-array", "CastArr",
                cb -> cb.ldc("x").checkcast(HIDDEN.arrayType()).pop().return_());
        run("checkcast-null", "CastNull",
                cb -> cb.aconst_null().checkcast(HIDDEN).pop().return_());
        run("instanceof-null", "InstNull",
                cb -> cb.aconst_null().instanceOf(HIDDEN).pop().return_());
        run("instanceof-array-receiver", "InstArrRecv",
                cb -> cb.iconst_0().anewarray(ConstantDescs.CD_Object)
                        .instanceOf(HIDDEN).pop().return_());
        run("checkcast-array-receiver", "CastArrRecv",
                cb -> cb.iconst_0().anewarray(ConstantDescs.CD_Object)
                        .checkcast(HIDDEN).pop().return_());
    }

    static void run(String row, String suffix, Consumer<CodeBuilder> body) throws Throwable {
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L2ClassAccessGen" + suffix), clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withMethodBody("run", MethodTypeDesc.of(ConstantDescs.CD_void),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, body));
        Class<?> gen = MethodHandles.lookup().defineClass(bytes);
        try {
            gen.getMethod("run").invoke(null);
            System.out.println(row + ": ok");
        } catch (InvocationTargetException e) {
            System.out.println(row + ": " + e.getCause().getClass().getName());
        } catch (Throwable t) {
            System.out.println(row + ": " + t.getClass().getName());
        }
    }
}
