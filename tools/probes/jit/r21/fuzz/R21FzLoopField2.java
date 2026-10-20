// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: narrow cells under the cell promotions
// (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY` for `this.f`, the
// static promotion (default ON) for `S.f`, the fresh-object promotion
// (default ON) for `new H().f`, and `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`
// whose kind screen must refuse byte / short / char / boolean arrays).
//
// Risk targeted: `MemKind::Int` also carries the narrowed byte / short /
// char / boolean fields, and a promoted cell lives in a register across the
// loop. javac always narrows a compound assignment (`i2b`), so the shape that
// matters is bytecode WITHOUT the narrowing: `getfield b; sipush 200; iadd;
// putfield b`. JVMS `putfield` / `putstatic` store only the field's width
// (boolean: `value & 1`), and `bastore` into a boolean[] stores `value & 1`.
// A promoted register that skips the builder's `narrow_to_field_type` would
// carry the unnarrowed sum to the next trip and to the write-back. The class
// is generated with java.lang.classfile (JDK 24+) so no javac narrowing is
// present.
//
// Run:
//   javac -d out R21FzLoopField2.java
//   java -Xmx256m -cp out R21FzLoopField2                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopField2
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm -Xmx256m -cp out R21FzLoopField2
//   CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE=1 cratonvm -Xmx256m -cp out R21FzLoopField2
//   CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE=0 CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE=0 cratonvm ... (control)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Consumer;

public class R21FzLoopField2 {
    static final int WARM = 50_000;
    static final ClassDesc GEN = ClassDesc.of("R21FzNarrowGen");
    static final String[] F = {"b", "s", "c", "z"};
    static final ClassDesc[] T = {ConstantDescs.CD_byte, ConstantDescs.CD_short, ConstantDescs.CD_char,
            ConstantDescs.CD_boolean};
    static final int[] K = {200, 40_000, 70_001, 3};

    /** for (i = 0; i < n; i++) body; with n in slot nSlot and i in slot iSlot. */
    static void loop(CodeBuilder code, int nSlot, int iSlot, Consumer<CodeBuilder> body) {
        Label top = code.newLabel();
        Label end = code.newLabel();
        code.iconst_0().istore(iSlot);
        code.labelBinding(top);
        code.iload(iSlot).iload(nSlot).if_icmpge(end);
        body.accept(code);
        code.iinc(iSlot, 1).goto_(top);
        code.labelBinding(end);
    }

    static byte[] generate() {
        MethodTypeDesc ii = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
        return ClassFile.of().build(GEN, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            for (int f = 0; f < 4; f++) {
                cb.withField(F[f], T[f], ClassFile.ACC_PUBLIC);
                cb.withField("s" + F[f], T[f], ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            }
            cb.withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, code -> code
                    .aload(0).invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void).return_());
            for (int f = 0; f < 4; f++) {
                final String name = F[f];
                final ClassDesc ty = T[f];
                final Integer k = K[f];
                // instance: this.f = this.f + K (no narrowing op)
                cb.withMethodBody("f" + name, ii, ClassFile.ACC_PUBLIC, code -> {
                    loop(code, 1, 2, c -> c.aload(0).aload(0).getfield(GEN, name, ty).ldc(k).iadd()
                            .putfield(GEN, name, ty));
                    code.aload(0).getfield(GEN, name, ty).ireturn();
                });
                // static: S.f = S.f + K
                cb.withMethodBody("s" + name, ii, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                    loop(code, 0, 1, c -> c.getstatic(GEN, "s" + name, ty).ldc(k).iadd()
                            .putstatic(GEN, "s" + name, ty));
                    code.getstatic(GEN, "s" + name, ty).ireturn();
                });
                // fresh: h = new Gen(); h.f = h.f + K; return h.f
                cb.withMethodBody("n" + name, ii, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                    code.new_(GEN).dup().invokespecial(GEN, "<init>", ConstantDescs.MTD_void).astore(1);
                    loop(code, 0, 2, c -> c.aload(1).aload(1).getfield(GEN, name, ty).ldc(k).iadd()
                            .putfield(GEN, name, ty));
                    code.aload(1).getfield(GEN, name, ty).ireturn();
                });
            }
            // arrays: a[0] = a[0] + K with baload/bastore (byte[] and boolean[]), saload/sastore, caload/castore
            cb.withMethodBody("ab", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_byte.arrayType(),
                    ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                loop(code, 1, 2, c -> c.aload(0).iconst_0().aload(0).iconst_0().baload().ldc(Integer.valueOf(200))
                        .iadd().bastore());
                code.aload(0).iconst_0().baload().ireturn();
            });
            cb.withMethodBody("az", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_boolean.arrayType(),
                    ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                loop(code, 1, 2, c -> c.aload(0).iconst_0().aload(0).iconst_0().baload().iconst_3()
                        .iadd().bastore());
                code.aload(0).iconst_0().baload().ireturn();
            });
            cb.withMethodBody("as", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_short.arrayType(),
                    ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                loop(code, 1, 2, c -> c.aload(0).iconst_0().aload(0).iconst_0().saload().ldc(Integer.valueOf(40_000))
                        .iadd().sastore());
                code.aload(0).iconst_0().saload().ireturn();
            });
            cb.withMethodBody("ac", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_char.arrayType(),
                    ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                loop(code, 1, 2, c -> c.aload(0).iconst_0().aload(0).iconst_0().caload().ldc(Integer.valueOf(70_001))
                        .iadd().castore());
                code.aload(0).iconst_0().caload().ireturn();
            });
        });
    }

    static MethodHandle[] inst = new MethodHandle[4];
    static MethodHandle[] stat = new MethodHandle[4];
    static MethodHandle[] fresh = new MethodHandle[4];
    static MethodHandle ab, az, as, ac;
    static MethodHandle ctor;

    static void rows(String tag, Object obj, int n) throws Throwable {
        StringBuilder sb = new StringBuilder("row " + tag + " n=" + n);
        for (int f = 0; f < 4; f++) {
            int a = (int) inst[f].invoke(obj, n);
            int b = (int) stat[f].invoke(n);
            int c = (int) fresh[f].invoke(n);
            sb.append(' ').append(F[f]).append('=').append(a).append('/').append(b).append('/').append(c);
        }
        byte[] by = {5};
        boolean[] bo = {false};
        short[] sh = {7};
        char[] ch = {'x'};
        sb.append(" ab=").append((int) ab.invoke(by, n));
        sb.append(" az=").append((int) az.invoke(bo, n));
        sb.append(" as=").append((int) as.invoke(sh, n));
        sb.append(" ac=").append((int) ac.invoke(ch, n));
        System.out.println(sb);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> c = l.defineClass(generate());
        MethodType ii = MethodType.methodType(int.class, int.class);
        for (int f = 0; f < 4; f++) {
            inst[f] = l.findVirtual(c, "f" + F[f], ii);
            stat[f] = l.findStatic(c, "s" + F[f], ii);
            fresh[f] = l.findStatic(c, "n" + F[f], ii);
        }
        ab = l.findStatic(c, "ab", MethodType.methodType(int.class, byte[].class, int.class));
        az = l.findStatic(c, "az", MethodType.methodType(int.class, boolean[].class, int.class));
        as = l.findStatic(c, "as", MethodType.methodType(int.class, short[].class, int.class));
        ac = l.findStatic(c, "ac", MethodType.methodType(int.class, char[].class, int.class));
        ctor = l.findConstructor(c, MethodType.methodType(void.class));
        Object o = ctor.invoke();
        for (int n = 0; n <= 3; n++) {
            rows("cold", o, n);
        }
        rows("cold", o, 7);
        long h = 0;
        byte[] by = {1};
        boolean[] bo = {true};
        short[] sh = {1};
        char[] ch = {1};
        for (int r = 0; r < WARM; r++) {
            int n = 1 + (r & 7);
            for (int f = 0; f < 4; f++) {
                h = h * 31 + (int) inst[f].invoke(o, n);
                h = h * 31 + (int) stat[f].invoke(n);
                h = h * 31 + (int) fresh[f].invoke(n);
            }
            h = h * 31 + (int) ab.invoke(by, n);
            h = h * 31 + (int) az.invoke(bo, n);
            h = h * 31 + (int) as.invoke(sh, n);
            h = h * 31 + (int) ac.invoke(ch, n);
        }
        System.out.println("warm hash " + h);
        Object o2 = ctor.invoke();
        for (int n = 0; n <= 3; n++) {
            rows("hot", o2, n);
        }
        rows("hot", o2, 7);
        rows("hot", o2, 1001);
        try {
            int x = (int) ab.invoke((byte[]) null, 2);
            System.out.println("null ab " + x);
        } catch (NullPointerException e) {
            System.out.println("null ab NPE");
        }
        rows("after", ctor.invoke(), 5);
    }
}
