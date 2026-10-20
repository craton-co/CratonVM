// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: `StringConcatFactory.makeConcatWithConstants`
// sites at their recipe edges, each generated with the java.lang.classfile API
// (javac never emits most of them): numeric constants rendered by
// `StringBuilder.append(Object)` (`Float.toString` / `Double.toString`),
// char / byte / short / boolean / long arguments, null arguments, an argument
// whose `toString()` answers null (`StringConcatHelper.stringOf`: "null"), a
// constant holding the tag characters \1 and \2 themselves (constants are
// text, never re-parsed), the empty recipe, a constant-only recipe, a site of
// exactly 200 argument slots (`MAX_INDY_CONCAT_ARG_SLOTS`) and one of 201
// (`StringConcatException: Too many concat argument slots: 201, can only
// accept 200`, wrapped in `BootstrapMethodError` and recorded: the second run
// fails the same way). Complements `L4W35ConcatValidation` (tag-count and
// return-type refusals) and `L4W38ConcatClassConstant`.
//
// In the output \u0001 / \u0002 print as <1> / <2>.
//
// Run: javac -d . L4W84ConcatRecipeEdges.java
//      cratonvm --jdk-only [--nojit] -cp . L4W84ConcatRecipeEdges
//      cratonvm --compatible [--nojit] -cp . L4W84ConcatRecipeEdges
//
// Expected output derived from StringConcatFactory.java / StringConcatHelper.java
// (JDK 25.0.3 src.zip); slots-201 confirmed on HotSpot (Temurin 25.0.3).
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4)
// numeric-constants: 1.0|-0.0|NaN|1.0E10|1.4E-45|1.0E21|1.0E-4|0.001|-9223372036854775808|-2147483648
// small-args: A,65,-1,300,true,1099511627776,c
// null-args: [null|null|null]
// toString-null: <null>
// tag-constants: <1>x<2>
// empty-recipe: ""
// constant-only: k
// slots-200: len=199 head=012345678901 tail=789012345671
// slots-201: java.lang.BootstrapMethodError: bootstrap method initialization exception <- java.lang.invoke.StringConcatException: Too many concat argument slots: 201, can only accept 200 | java.lang.BootstrapMethodError: bootstrap method initialization exception
// HOTSPOT_EXPECTED_END
//
// `slots-201`: `makeConcatWithConstants` throws the checked
// `StringConcatException` inside `BootstrapMethodInvoker.invoke`'s try, whose
// `catch (Throwable)` wraps it as `BootstrapMethodError("bootstrap method
// initialization exception")`; `CallSite.makeSite` passes that Error through
// (its own "CallSite bootstrap method initialization exception" is only for a
// non-CallSite / wrongly typed answer). The linkage error is recorded; an
// indy row is recorded by class and message only (no cause), so the second
// execution throws a new, cause-less error (`RecordedLinkageError` in
// vm/src/runtime/invokedynamic.rs already models this).
import java.lang.classfile.ClassFile;
import java.lang.classfile.TypeKind;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class L4W84ConcatRecipeEdges {
    static final DirectMethodHandleDesc BSM = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC,
            ClassDesc.of("java.lang.invoke.StringConcatFactory"), "makeConcatWithConstants",
            MethodTypeDesc.of(ConstantDescs.CD_CallSite, ConstantDescs.CD_MethodHandles_Lookup,
                    ConstantDescs.CD_String, ConstantDescs.CD_MethodType, ConstantDescs.CD_String,
                    ConstantDescs.CD_Object.arrayType()));

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W84ConcatRecipeEdges.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();
    static int classes;

    public static final class NullToString {
        @Override
        public String toString() {
            return null;
        }
    }

    static Method site(String recipe, ClassDesc[] params, ConstantDesc... constants) throws Exception {
        String cls = "Cc" + (classes++);
        ConstantDesc[] staticArgs = new ConstantDesc[constants.length + 1];
        staticArgs[0] = recipe;
        System.arraycopy(constants, 0, staticArgs, 1, constants.length);
        MethodTypeDesc type = MethodTypeDesc.of(ConstantDescs.CD_String, params);
        byte[] b = ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("c", type, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                int slot = 0;
                for (ClassDesc p : params) {
                    TypeKind k = TypeKind.from(p);
                    code.loadLocal(k, slot);
                    slot += k.slotSize();
                }
                code.invokedynamic(DynamicCallSiteDesc.of(BSM, "concat", type, staticArgs));
                code.areturn();
            });
        });
        Class<?> c = LOADER.define(cls, b);
        for (Method m : c.getMethods()) {
            if (m.getName().equals("c")) {
                return m;
            }
        }
        throw new AssertionError("no c");
    }

    static String describe(Throwable t) {
        StringBuilder sb = new StringBuilder();
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
        }
        return sb.toString();
    }

    static String visible(String s) {
        return s.replace("\u0001", "<1>").replace("\u0002", "<2>");
    }

    interface Shape {
        String apply(String s);
    }

    static void row(String name, int runs, Shape shape, String recipe, ClassDesc[] params, Object[] values,
            ConstantDesc... constants) {
        StringBuilder out = new StringBuilder();
        Method m;
        try {
            m = site(recipe, params, constants);
        } catch (Throwable t) {
            System.out.println(name + ": setup: " + t);
            return;
        }
        for (int i = 0; i < runs; i++) {
            if (i > 0) {
                out.append(" | ");
            }
            try {
                out.append(shape.apply((String) m.invoke(null, values)));
            } catch (InvocationTargetException e) {
                out.append(describe(e.getCause()));
            } catch (Throwable t) {
                out.append("call: ").append(t);
            }
        }
        System.out.println(name + ": " + out);
    }

    static ClassDesc[] ps(ClassDesc... p) {
        return p;
    }

    public static void main(String[] args) {
        Shape plain = L4W84ConcatRecipeEdges::visible;
        row("numeric-constants", 1, plain, "\2|\2|\2|\2|\2|\2|\2|\2|\2|\2", ps(), new Object[0],
                1.0f, -0.0f, Float.NaN, 1.0E10f, Float.MIN_VALUE, 1.0E21, 1.0E-4, 0.001, Long.MIN_VALUE,
                Integer.MIN_VALUE);
        row("small-args", 1, plain, "\1,\1,\1,\1,\1,\1,\1",
                ps(ConstantDescs.CD_char, ConstantDescs.CD_int, ConstantDescs.CD_byte, ConstantDescs.CD_short,
                        ConstantDescs.CD_boolean, ConstantDescs.CD_long, ConstantDescs.CD_char),
                new Object[] {'A', 65, (byte) -1, (short) 300, true, 1L << 40, 'c'});
        row("null-args", 1, plain, "[\1|\1|\1]",
                ps(ConstantDescs.CD_String, ConstantDescs.CD_Object, ClassDesc.of("java.lang.CharSequence")),
                new Object[] {null, null, null});
        row("toString-null", 1, plain, "<\1>", ps(ConstantDescs.CD_Object), new Object[] {new NullToString()});
        row("tag-constants", 1, plain, "\2\1\2", ps(ConstantDescs.CD_String), new Object[] {"x"}, "\u0001", "\u0002");
        row("empty-recipe", 1, s -> "\"" + s + "\"", "", ps(), new Object[0]);
        row("constant-only", 1, plain, "\2", ps(), new Object[0], "k");

        ClassDesc[] p200 = new ClassDesc[199];
        Object[] v200 = new Object[199];
        for (int i = 0; i < 198; i++) {
            p200[i] = ConstantDescs.CD_int;
            v200[i] = i % 10;
        }
        p200[198] = ConstantDescs.CD_long;
        v200[198] = 1L;
        Shape summary = s -> "len=" + s.length() + " head=" + s.substring(0, 12) + " tail="
                + s.substring(s.length() - 12);
        row("slots-200", 1, summary, "\1".repeat(199), p200, v200);

        ClassDesc[] p201 = new ClassDesc[200];
        Object[] v201 = new Object[200];
        for (int i = 0; i < 199; i++) {
            p201[i] = ConstantDescs.CD_int;
            v201[i] = i % 10;
        }
        p201[199] = ConstantDescs.CD_long;
        v201[199] = 1L;
        row("slots-201", 2, plain, "\1".repeat(200), p201, v201);
    }
}
