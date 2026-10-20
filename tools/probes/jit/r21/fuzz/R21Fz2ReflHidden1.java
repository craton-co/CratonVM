// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: REFLECT21-5, compiled links into hidden
// classes (`CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`, opt-in;
// `MhCompiledMember::bind_hidden_static`, `mh_link_leaf_static`,
// `mh_hidden_static_call`: a STATIC member of an INITIALIZED hidden class
// called by its owner's id through `interpreter::execute`).
//
// Risk targeted: every combinator below spins `LambdaForm$MH` / `$DMH`
// forms (hidden classes) whose static `invoke` methods are now linked. The
// link must keep: argument and return conversions (asType boxing /
// widening / unboxing NPE), exceptions thrown through the forms (catchException,
// a guardWithTest's fallback), a deopt INSIDE the target (handled as a
// VM-side entry), recursion through the same form, and the frames:
// default stack traces (hidden frames excluded) and a StackWalker with
// SHOW_HIDDEN_FRAMES (form frames listed; names normalized past the
// `/0x...` suffix). Also a user hidden class (Lookup.defineHiddenClass, built
// with java.lang.classfile): its static method through findStatic, first
// called BEFORE its class is initialized (the `initialized` screen), with a
// `<clinit>` that records the call order.
// Cold, hot (50k calls per handle), a deopt trigger (a new receiver class
// inside a target), again.
//
// Run:
//   javac -d out R21Fz2ReflHidden1.java
//   java -Xmx256m -cp out R21Fz2ReflHidden1                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2ReflHidden1
//   CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1 cratonvm -Xmx256m -cp out R21Fz2ReflHidden1
//   CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1 CRATONVM_JIT_MH_COMPILED_LINK=1 cratonvm ...
//   CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1 CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1 CRATONVM_JIT_REFLECT_CTOR_LINK=1 cratonvm ...
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm. The hidden-frames
// walk lists JDK form names that are HotSpot's choice; a diff there on the
// DEFAULT arm too is a pre-existing form-shape gap, not this switch.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.stream.Collectors;

public class R21Fz2ReflHidden1 {
    static final int WARM = 50_000;
    static final StringBuilder ORDER = new StringBuilder();

    interface Shape {
        int area();
    }

    static final class Sq implements Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        public int area() {
            return s * s;
        }
    }

    static int target(int a, Shape s) {
        if (a == -1) {
            throw new IllegalStateException("neg");
        }
        return a * 31 + s.area();
    }

    static long widen(long x, double d) {
        return x * 3 + (long) d;
    }

    static Object boxer(Integer i) {
        return i == null ? "null" : i + 1;
    }

    static boolean even(int a, Shape s) {
        return (a & 1) == 0;
    }

    static int odd(int a, Shape s) {
        return -a;
    }

    static int handler(IllegalStateException e, int a, Shape s) {
        return -1000 - e.getMessage().length();
    }

    static int fact(MethodHandle self, int n) throws Throwable {
        return n <= 1 ? 1 : n * (int) self.invokeExact(self, n - 1);
    }

    static String hiddenWalk() {
        return StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES).walk(s -> s.limit(6)
                .map(f -> f.getClassName().replaceAll("/0x[0-9a-f]+", "/0x") + "." + f.getMethodName())
                .collect(Collectors.joining(",")));
    }

    static String plainWalk() {
        return StackWalker.getInstance().walk(s -> s.limit(4).map(f -> f.getMethodName())
                .collect(Collectors.joining(",")));
    }

    static volatile String walked = "";

    static int walker(int a, Shape s) {
        walked = plainWalk() + " | " + hiddenWalk();
        return a;
    }

    static byte[] hiddenBytes() {
        ClassDesc me = ClassDesc.of("R21Fz2HiddenGen");
        ClassDesc order = ClassDesc.of("R21Fz2ReflHidden1");
        MethodTypeDesc append = MethodTypeDesc.of(ClassDesc.of("java.lang.StringBuilder"), ConstantDescs.CD_String);
        return ClassFile.of().build(me, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withField("BASE", ConstantDescs.CD_int, ClassFile.ACC_STATIC);
            cb.withMethodBody("<clinit>", ConstantDescs.MTD_void, ClassFile.ACC_STATIC, code -> code
                    .getstatic(order, "ORDER", ClassDesc.of("java.lang.StringBuilder")).ldc("clinit;")
                    .invokevirtual(ClassDesc.of("java.lang.StringBuilder"), "append", append).pop()
                    .bipush(77).putstatic(me, "BASE", ConstantDescs.CD_int).return_());
            cb.withMethodBody("calc", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code
                            .iload(0).getstatic(me, "BASE", ConstantDescs.CD_int).iadd().ireturn());
        });
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType ts = MethodType.methodType(int.class, int.class, Shape.class);
        MethodHandle tgt = l.findStatic(R21Fz2ReflHidden1.class, "target", ts);
        MethodHandle filtered = MethodHandles.filterReturnValue(tgt,
                MethodHandles.insertArguments(l.findStatic(Math.class, "floorMod", MethodType.methodType(int.class,
                        int.class, int.class)), 1, 1000));
        MethodHandle guarded = MethodHandles.guardWithTest(l.findStatic(R21Fz2ReflHidden1.class, "even",
                MethodType.methodType(boolean.class, int.class, Shape.class)), filtered,
                l.findStatic(R21Fz2ReflHidden1.class, "odd", ts));
        MethodHandle caught = MethodHandles.catchException(tgt, IllegalStateException.class,
                l.findStatic(R21Fz2ReflHidden1.class, "handler", MethodType.methodType(int.class,
                        IllegalStateException.class, int.class, Shape.class)));
        MethodHandle widened = l.findStatic(R21Fz2ReflHidden1.class, "widen",
                MethodType.methodType(long.class, long.class, double.class))
                .asType(MethodType.methodType(Object.class, int.class, int.class));
        MethodHandle boxed = l.findStatic(R21Fz2ReflHidden1.class, "boxer", MethodType.methodType(Object.class,
                Integer.class)).asType(MethodType.methodType(int.class, Object.class));
        MethodHandle factH = l.findStatic(R21Fz2ReflHidden1.class, "fact", MethodType.methodType(int.class,
                MethodHandle.class, int.class));
        MethodHandle walkH = l.findStatic(R21Fz2ReflHidden1.class, "walker", ts);
        MethodHandle folded = MethodHandles.foldArguments(walkH, 0,
                MethodHandles.dropArguments(MethodHandles.constant(int.class, 5), 0, Shape.class));

        Shape sq = new Sq(3);
        for (int pass = 0; pass < 3; pass++) {
            String tag = pass == 0 ? "cold" : pass == 1 ? "hot" : "after";
            StringBuilder sb = new StringBuilder("row " + tag);
            for (int a : new int[] {0, 1, 7, -1}) {
                sb.append(" | a=").append(a);
                try {
                    sb.append(' ').append((int) tgt.invokeExact(a, sq));
                } catch (IllegalStateException e) {
                    sb.append(" ISE");
                }
                try {
                    sb.append(' ').append((int) guarded.invokeExact(a, sq));
                } catch (IllegalStateException e) {
                    sb.append(" ISE");
                }
                sb.append(' ').append((int) caught.invokeExact(a, sq));
                sb.append(' ').append((Object) widened.invokeExact(a, a * 2));
                sb.append(' ').append((int) boxed.invokeExact((Object) a));
            }
            try {
                int x = (int) boxed.invokeExact((Object) "notInt");
                sb.append(" boxedStr=").append(x);
            } catch (ClassCastException e) {
                sb.append(" boxedStr=CCE");
            }
            sb.append(" fact=").append((int) factH.invokeExact(factH, 10));
            System.out.println(sb);
            int unused = (int) folded.invokeExact(sq);
            System.out.println("walk " + tag + " " + walked + " " + unused);
            try {
                int bad = (int) guarded.invokeExact(-2, sq);
                System.out.println("trace " + tag + " none " + bad);
            } catch (IllegalStateException e) {
                System.out.println("trace " + tag + " unexpected");
            }
            try {
                int bad = (int) tgt.invokeExact(-1, sq);
                System.out.println("trace " + tag + " none " + bad);
            } catch (IllegalStateException e) {
                StackTraceElement[] st = e.getStackTrace();
                StringBuilder t = new StringBuilder("trace " + tag);
                for (int i = 0; i < st.length && i < 4; i++) {
                    t.append(' ').append(st[i].getMethodName());
                }
                System.out.println(t);
            }
            if (pass == 0) {
                long h = 0;
                for (int r = 0; r < WARM; r++) {
                    int a = r & 1023;
                    h = h * 31 + (int) tgt.invokeExact(a, sq) + (int) guarded.invokeExact(a, sq)
                            + (int) caught.invokeExact(a, sq) + (long) (Object) widened.invokeExact(a, r)
                            + (int) boxed.invokeExact((Object) a) + (int) factH.invokeExact(factH, a & 7);
                    if ((r & 15) == 0) {
                        h += (int) caught.invokeExact(-1, sq);
                    }
                }
                System.out.println("warm hash " + h);
                // A user hidden class: the handle is looked up and CALLED before
                // anything else touches the class (its <clinit> runs inside the call).
                MethodHandles.Lookup hl = l.defineHiddenClass(hiddenBytes(), false);
                MethodHandle calc = hl.findStatic(hl.lookupClass(), "calc", MethodType.methodType(int.class, int.class));
                ORDER.append("lookup;");
                int first = (int) calc.invokeExact(1);
                long hh = 0;
                for (int r = 0; r < WARM; r++) {
                    hh += (int) calc.invokeExact(r);
                }
                System.out.println("hidden first=" + first + " sum=" + hh + " order=" + ORDER + " name-prefix="
                        + hl.lookupClass().getName().replaceAll("/0x[0-9a-f]+", "/0x") + " isHidden="
                        + hl.lookupClass().isHidden());
                // Deopt trigger: a Shape class first seen inside the linked targets.
                sq = new Shape() {
                    public int area() {
                        return 1;
                    }
                };
            }
        }
    }
}
