// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 121, lane L4
// (`i121-L4-jdk-string-concat-factory-hidden-class-constructor-is-refused`):
// the JDK's own `StringConcatFactory` bytecode, called from user code.
//
// For two or more arguments, `InlineHiddenClassStrategy` defines
// `java.lang.String$$StringConcat` as a hidden class through `STR_LOOKUP`
// (lookup class `String`). It then calls `findConstructor` on the class's
// package-private `<init>(String[])`. `MethodHandleNatives.resolve`'s access
// check read the hidden class's package as everything before the LAST `/` of
// `java/lang/String$$StringConcat/0x<hex>`, so the constructor was refused:
// `StringConcatException: Generator failed <- Exception while spinning the
// class <- IllegalAccessException: no such constructor <- IllegalAccessError`.
// HotSpot puts a hidden class in the package of its class-file name (JEP 371).
// n=1 worked, because it uses a built-in class.
//
//   scf-<n>          makeConcatWithConstants(lookup(), "concat", (int, String,
//                    int, ...)String, "\1" x n, ...) and invoke it; n = 1, 2,
//                    3, 5, 10, 25 (the coordinator's Scf121 repro)
//   scf-const-<n>    the same with constants between the arguments
//   hidden-ctor      a user hidden class (defineHiddenClass, not a nestmate)
//                    with a package-private constructor; findConstructor
//                    from the defining lookup (same runtime package)
//   hidden-method    findVirtual of its package-private method
//   hidden-private   findVirtual of a PRIVATE method: refused (not a
//                    nestmate), the HotSpot text
//
// Switch: CRATONVM_MH_HIDDEN_CLASS_RUNTIME_PACKAGE (default ON, `=0` restores
// the old reading, under which every scf-<n> row with n >= 2 and the
// hidden-ctor / hidden-method rows fail).
//
// Run: javac -d out L4W121JdkStringConcatFactory.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L4W121JdkStringConcatFactory
//
// Expected HotSpot 25.0.3 output (default and -Xint):
//   scf-1: 0
//   scf-2: 0s1
//   scf-3: 0s12
//   scf-5: 0s12s34
//   scf-10: 0s12s34s56s78s9
//   scf-25: 0s12s34s56s78s910s1112s1314s1516s1718s1920s2122s2324
//   scf-const-1: <0>
//   scf-const-2: <0|s1>
//   scf-const-3: <0|s1|2>
//   scf-const-5: <0|s1|2|s3|4>
//   scf-const-10: <0|s1|2|s3|4|s5|6|s7|8|s9>
//   scf-const-25: <0|s1|2|s3|4|s5|6|s7|8|s9|10|s11|12|s13|14|s15|16|s17|18|s19|20|s21|22|s23|24>
//   hidden-ctor: made 7
//   hidden-method: got 7
//   hidden-private: java.lang.IllegalAccessException
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.StringConcatFactory;

public class L4W121JdkStringConcatFactory {
    static String scf(int n, boolean constants) {
        try {
            Class<?>[] params = new Class<?>[n];
            Object[] args = new Object[n];
            StringBuilder recipe = new StringBuilder(constants ? "<" : "");
            for (int i = 0; i < n; i++) {
                boolean str = (i % 2) == 1;
                params[i] = str ? String.class : int.class;
                args[i] = str ? "s" + i : (Object) i;
                if (constants && i > 0) {
                    recipe.append('|');
                }
                recipe.append('\u0001');
            }
            if (constants) {
                recipe.append('>');
            }
            MethodType mt = MethodType.methodType(String.class, params);
            MethodHandle mh = StringConcatFactory.makeConcatWithConstants(
                    MethodHandles.lookup(), "concat", mt, recipe.toString()).getTarget();
            return String.valueOf(mh.invokeWithArguments(args));
        } catch (Throwable t) {
            return describe(t);
        }
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (int depth = 0; t != null && depth < 5; depth++, t = t.getCause()) {
            if (depth > 0) {
                sb.append(" <- ");
            }
            sb.append(t.getClass().getName()).append(": ").append(t.getMessage());
        }
        return sb.toString();
    }

    /** A hidden class `L4W121Hid` with `Hid(int)`, `int get()` (package-private), `priv()`. */
    static byte[] hiddenBytes() {
        ClassDesc self = ClassDesc.of("L4W121Hid");
        return ClassFile.of().build(self, cb -> {
            cb.withField("v", ConstantDescs.CD_int, 0);
            cb.withMethodBody(ConstantDescs.INIT_NAME,
                    MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int), 0, code -> code
                            .aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .aload(0)
                            .iload(1)
                            .putfield(self, "v", ConstantDescs.CD_int)
                            .return_());
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_int), 0, code -> code
                    .aload(0)
                    .getfield(self, "v", ConstantDescs.CD_int)
                    .ireturn());
            cb.withMethodBody("priv", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PRIVATE, code -> code
                    .iconst_1()
                    .ireturn());
        });
    }

    public static void main(String[] args) throws Throwable {
        int[] sizes = {1, 2, 3, 5, 10, 25};
        for (int n : sizes) {
            System.out.println("scf-" + n + ": " + scf(n, false));
        }
        for (int n : sizes) {
            System.out.println("scf-const-" + n + ": " + scf(n, true));
        }
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(hiddenBytes(), true);
        Class<?> hid = hl.lookupClass();
        MethodHandles.Lookup mine = MethodHandles.lookup();
        Object made = null;
        try {
            made = mine.findConstructor(hid, MethodType.methodType(void.class, int.class)).invoke(7);
            System.out.println("hidden-ctor: made " + (made != null ? 7 : 0));
        } catch (Throwable t) {
            System.out.println("hidden-ctor: " + describe(t));
        }
        try {
            Object m = made != null ? made : hl.findConstructor(hid, MethodType.methodType(void.class, int.class)).invoke(7);
            System.out.println("hidden-method: got "
                    + mine.findVirtual(hid, "get", MethodType.methodType(int.class)).invoke(m));
        } catch (Throwable t) {
            System.out.println("hidden-method: " + describe(t));
        }
        try {
            mine.findVirtual(hid, "priv", MethodType.methodType(int.class));
            System.out.println("hidden-private: found");
        } catch (Throwable t) {
            System.out.println("hidden-private: " + t.getClass().getName());
        }
    }
}
