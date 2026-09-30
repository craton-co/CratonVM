// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4 (review): the RESTART index of
// `SwitchBootstraps.enumSwitch` / `typeSwitch` call sites (javac passes the
// index after a failed guard). Each row generates (java.lang.classfile) a
// class `Lo<Row>` whose static `sw(Object, int)` is one `invokedynamic` over
// the bootstrap with the row's labels and returns its answer; the row prints
// the answer for each restart index in -1, 0, 1, 2, 3, 4 (an exception prints
// its simple class name and message). HotSpot's generated switch makes
// `Objects.checkIndex(restart, labels.length + 1)` first; CratonVM clamped a
// negative index to 0 and answered the default for one past the end.
//
//   enum-names     enumSwitch  labels "B", "A", "B"          selector E.B
//   enum-unknown   enumSwitch  labels "Z", "B"               selector E.B
//   enum-mixed     enumSwitch  labels "A", E.class, "B"      selector E.B
//   enum-body      enumSwitch  labels "C", "A"               selector E.C (a constant with a body)
//   type-mixed     typeSwitch  labels String, Integer, E.class selector E.B
//   type-null      typeSwitch  labels String, Integer             selector null
//   enum-null      enumSwitch  labels "A", "B"                    selector null
//
// Run: javac -d out L4W42SwitchRestartIndex.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W42SwitchRestartIndex
//
// Expected HotSpot 25 output (default and -Xint, measured locally), also
// `--compatible`'s (the fix is in every mode; javac only passes an index in
// `0..=labels.length`):
//   enum-names: IndexOutOfBoundsException(Index -1 out of bounds for length 4) 0 2 2 3 IndexOutOfBoundsException(Index 4 out of bounds for length 4)
//   enum-unknown: IndexOutOfBoundsException(Index -1 out of bounds for length 3) 1 1 2 IndexOutOfBoundsException(Index 3 out of bounds for length 3) IndexOutOfBoundsException(Index 4 out of bounds for length 3)
//   enum-mixed: IndexOutOfBoundsException(Index -1 out of bounds for length 4) 1 1 2 3 IndexOutOfBoundsException(Index 4 out of bounds for length 4)
//   enum-body: IndexOutOfBoundsException(Index -1 out of bounds for length 3) 0 2 2 IndexOutOfBoundsException(Index 3 out of bounds for length 3) IndexOutOfBoundsException(Index 4 out of bounds for length 3)
//   type-mixed: IndexOutOfBoundsException(Index -1 out of bounds for length 4) 2 2 2 3 IndexOutOfBoundsException(Index 4 out of bounds for length 4)
//   type-null: IndexOutOfBoundsException(Index -1 out of bounds for length 3) -1 -1 -1 IndexOutOfBoundsException(Index 3 out of bounds for length 3) IndexOutOfBoundsException(Index 4 out of bounds for length 3)
//   enum-null: -1 -1 -1 -1 -1 -1
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W42SwitchRestartIndex {
    public enum E {
        A,
        B,
        C {
            @Override
            public String toString() {
                return "c";
            }
        }
    }

    static final ClassDesc SB = ClassDesc.of("java.lang.runtime.SwitchBootstraps");
    static final ClassDesc OBJ = ConstantDescs.CD_Object;
    static final ClassDesc INT = ConstantDescs.CD_int;
    static final ClassDesc ENUM = ClassDesc.of("L4W42SwitchRestartIndex$E");

    static DirectMethodHandleDesc bsm(String name) {
        return MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, SB, name,
                MethodTypeDesc.of(ClassDesc.of("java.lang.invoke.CallSite"),
                        ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"), ConstantDescs.CD_String,
                        ClassDesc.of("java.lang.invoke.MethodType"), OBJ.arrayType()));
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W42SwitchRestartIndex.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String row(String name, String bootstrap, ClassDesc selector, Object value, ConstantDesc... labels) {
        DynamicCallSiteDesc site = DynamicCallSiteDesc.of(bsm(bootstrap), bootstrap,
                MethodTypeDesc.of(INT, selector, INT), labels);
        String cls = "Lo" + name.replace("-", "");
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("sw", MethodTypeDesc.of(INT, OBJ, INT), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.aload(0).checkcast(selector).iload(1).invokedynamic(site).ireturn());
        });
        try {
            Method sw = LOADER.define(cls, b).getMethod("sw", Object.class, int.class);
            StringBuilder out = new StringBuilder();
            for (int restart = -1; restart <= 4; restart++) {
                if (restart > -1) {
                    out.append(' ');
                }
                try {
                    out.append(sw.invoke(null, value, restart));
                } catch (InvocationTargetException e) {
                    out.append(e.getCause().getClass().getSimpleName()).append('(')
                            .append(e.getCause().getMessage()).append(')');
                }
            }
            return out.toString();
        } catch (Throwable t) {
            return "setup: " + t;
        }
    }

    public static void main(String[] args) {
        System.out.println("enum-names: " + row("enum-names", "enumSwitch", ENUM, E.B, "B", "A", "B"));
        System.out.println("enum-unknown: " + row("enum-unknown", "enumSwitch", ENUM, E.B, "Z", "B"));
        System.out.println("enum-mixed: " + row("enum-mixed", "enumSwitch", ENUM, E.B, "A", ENUM, "B"));
        System.out.println("enum-body: " + row("enum-body", "enumSwitch", ENUM, E.C, "C", "A"));
        System.out.println("type-mixed: " + row("type-mixed", "typeSwitch", OBJ, E.B, ConstantDescs.CD_String,
                ClassDesc.of("java.lang.Integer"), ENUM));
        System.out.println("type-null: " + row("type-null", "typeSwitch", OBJ, null, ConstantDescs.CD_String,
                ClassDesc.of("java.lang.Integer")));
        System.out.println("enum-null: " + row("enum-null", "enumSwitch", ENUM, null, "A", "B"));
    }
}
