// Interpreter round i1, wave 11, lane L2 — the JPMS export clause of
// JVMS 5.4.4 on class constants.
//
// A class-path (unnamed-module) class may reach a public class of a named
// module only if its package is exported to it: unqualified, or to
// ALL-UNNAMED. `jdk.internal.misc` is exported by java.base to a list of JDK
// modules only, so resolving `jdk.internal.misc.Unsafe` from class-path code
// throws IllegalAccessError on HotSpot, for every instruction that resolves a
// class constant (`LinkResolver::check_klass_accessibility`).
//
// javac refuses such a reference without --add-exports, so the bytecode is
// generated with the java.lang.classfile API (final since JDK 24).
//
// Expected HotSpot 25 stdout:
//   ldc: java.lang.IllegalAccessError
//   checkcast: java.lang.IllegalAccessError
//   instanceof: java.lang.IllegalAccessError
//   anewarray: java.lang.IllegalAccessError
//   ldc-exported: ok
//   checkcast-null: ok
//
// Matches HotSpot in both modes: `--jdk-only` enforces the export clause
// since wave 12, `--compatible` since wave 14 (its workload census in
// docs/internal/fixed-bugs/interpreter-L2-jpms-export-clause-not-checked-on-class-constants-FIXED-20260925.md
// read zero).

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.InvocationTargetException;
import java.util.function.Consumer;

public class L2ModuleExport {
    static final ClassDesc UNSAFE = ClassDesc.of("jdk.internal.misc.Unsafe");

    public static void main(String[] args) throws Throwable {
        run("ldc", "Ldc", cb -> cb.ldc(UNSAFE).pop().return_());
        run("checkcast", "Cast", cb -> cb.ldc("x").checkcast(UNSAFE).pop().return_());
        run("instanceof", "Inst", cb -> cb.ldc("x").instanceOf(UNSAFE).pop().return_());
        run("anewarray", "Anew", cb -> cb.iconst_0().anewarray(UNSAFE).pop().return_());
        run("ldc-exported", "LdcString", cb -> cb.ldc(ConstantDescs.CD_String).pop().return_());
        run("checkcast-null", "CastNull",
                cb -> cb.aconst_null().checkcast(UNSAFE).pop().return_());
    }

    static void run(String row, String suffix, Consumer<CodeBuilder> body) throws Throwable {
        byte[] bytes = ClassFile.of().build(ClassDesc.of("L2ModuleExportGen" + suffix), clb -> clb
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
