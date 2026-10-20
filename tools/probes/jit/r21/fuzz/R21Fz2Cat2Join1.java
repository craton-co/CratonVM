// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: the category-2 pair split at a join
// (`CRATONVM_JIT_IR_MERGE_CAT2_PAIR_KILL`, default ON:
// `IrBuilder::kill_cat2_values_split_at_a_join`; and the VM backstop
// `CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP`, default ON:
// `deopt_resume::cat2_pair_split_in_locals`).
//
// Risk targeted: bytecode that javac never emits but ECJ, Kotlin and
// rewriters do -- a `long` / `double` at slots n, n+1 on one path, slot n+1
// (or n) overwritten on the other, the two meeting at a join. The verifier
// makes both slots TOP there. A frame state after the join that still names
// the cat-2 value, with a live value in its high half, made
// `ir_deopt_locals` skip that slot and shift every later local by one: after
// a deopt the interpreter reads the wrong value for every local past the
// pair. Each generated method keeps live values in slots BEFORE and AFTER
// the pair, takes the split path or not, and then deopts after the join (an
// interface call whose receiver class is new after the warm-up), and
// returns a function of every live local. Shapes: long hi-half <- int;
// double hi-half <- float; long hi-half <- reference; long LOW half <- int;
// a long PARAMETER's hi-half <- int; the split at a LOOP header (two forward
// entries) with the deopt inside the loop. Generated with java.lang.classfile.
//
// Run:
//   javac -d out R21Fz2Cat2Join1.java
//   java -Xmx256m -cp out R21Fz2Cat2Join1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2Cat2Join1
//   CRATONVM_JIT_IR_MERGE_CAT2_PAIR_KILL=0 cratonvm -Xmx256m -cp out R21Fz2Cat2Join1   (kill)
//   CRATONVM_DEOPT_LOCALS_SPLIT_CAT2_TOP=0 cratonvm -Xmx256m -cp out R21Fz2Cat2Join1   (kill)
//   both kills together; CRATONVM_DBG_DEOPT=1 (stderr: "[cratonvm-deopt] locals slot ... treated as TOP")
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm (a kill arm may show the old defect).

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class R21Fz2Cat2Join1 {
    static final int WARM = 200_000;
    static final ClassDesc GEN = ClassDesc.of("R21Fz2Cat2Gen");
    static final ClassDesc FN = ClassDesc.of("R21Fz2Cat2Join1$Fn");
    static final ClassDesc STR = ConstantDescs.CD_String;
    static final MethodTypeDesc II = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final MethodTypeDesc LEN = MethodTypeDesc.of(ConstantDescs.CD_int);

    public interface Fn {
        int apply(int v);
    }

    static final class Inc implements Fn {
        public int apply(int v) {
            return v + 1;
        }
    }

    static final class Twice implements Fn {
        public int apply(int v) {
            return v * 2;
        }
    }

    // (int c, Fn op, int live, String s) -> int; slots 0..3, the pair at 4-5.
    static final MethodTypeDesc SHAPE = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int, FN,
            ConstantDescs.CD_int, STR);

    /** After the join: r = op.apply(k@6) + k + live + s.length() + extra. */
    static void tail(CodeBuilder code, int kSlot) {
        code.aload(1).iload(kSlot).invokeinterface(FN, "apply", II).istore(9);
        code.iload(9).iload(kSlot).iadd().iload(2).iadd().aload(3).invokevirtual(STR, "length", LEN).iadd();
    }

    static byte[] generate() {
        return ClassFile.of().build(GEN, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            // m1: long at 4-5; the c != 0 arm writes an int into slot 5.
            cb.withMethodBody("m1", SHAPE, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label join = code.newLabel();
                code.ldc(Long.valueOf(0x1234_5678_9ABCL)).lstore(4);
                code.iload(2).iconst_3().imul().lload(4).l2i().iadd().istore(6);
                code.iload(0).ifeq(join);
                code.bipush(5).istore(5);
                code.labelBinding(join);
                tail(code, 6);
                code.ireturn();
            });
            // m2: double at 4-5; the arm writes a float into slot 5.
            cb.withMethodBody("m2", SHAPE, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label join = code.newLabel();
                code.ldc(Double.valueOf(2.75)).dstore(4);
                code.iload(2).dload(4).d2i().iadd().istore(6);
                code.iload(0).ifeq(join);
                code.fconst_2().fstore(5);
                code.labelBinding(join);
                tail(code, 6);
                code.ireturn();
            });
            // m3: long at 4-5; the arm writes a reference into slot 5; a live
            // reference in slot 7 and an int in slot 6.
            cb.withMethodBody("m3", SHAPE, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label join = code.newLabel();
                code.ldc(Long.valueOf(-77L)).lstore(4);
                code.iload(2).lload(4).l2i().isub().istore(6);
                code.aload(3).astore(7);
                code.iload(0).ifeq(join);
                code.aload(3).astore(5);
                code.labelBinding(join);
                tail(code, 6);
                code.aload(7).invokevirtual(STR, "length", LEN).bipush(100).imul().iadd();
                code.ireturn();
            });
            // m4: long at 4-5; the arm writes an int into the LOW slot 4.
            cb.withMethodBody("m4", SHAPE, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label join = code.newLabel();
                code.ldc(Long.valueOf(1L << 40)).lstore(4);
                code.iload(2).lload(4).bipush(40).lushr().l2i().iadd().istore(6);
                code.iload(0).ifeq(join);
                code.iconst_2().istore(4);
                code.labelBinding(join);
                tail(code, 6);
                code.ireturn();
            });
            // m5: (long p, int c, Fn op) -> int; the arm writes p's high half.
            cb.withMethodBody("m5", MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_long,
                    ConstantDescs.CD_int, FN), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label join = code.newLabel();
                code.lload(0).l2i().iload(2).iadd().istore(4);
                code.iconst_m1().istore(5);
                code.iload(2).ifeq(join);
                code.bipush(9).istore(1);
                code.labelBinding(join);
                code.aload(3).iload(4).invokeinterface(FN, "apply", II).iload(4).iadd().iload(5).iadd();
                code.ireturn();
            });
            // m6: the split at a LOOP header: two forward entries (long kept
            // / hi half written), then for (i = 0; i < 8; i++) sum += op.apply(i + k).
            cb.withMethodBody("m6", SHAPE, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> {
                Label head = code.newLabel();
                Label end = code.newLabel();
                Label skip = code.newLabel();
                code.ldc(Long.valueOf(5L)).lstore(4);
                code.iload(2).lload(4).l2i().imul().istore(6);
                code.iconst_0().istore(7);
                code.iconst_0().istore(8);
                code.iload(0).ifeq(skip);
                code.bipush(3).istore(5);
                code.labelBinding(skip);
                code.labelBinding(head);
                code.iload(7).bipush(8).if_icmpge(end);
                code.iload(8).aload(1).iload(7).iload(6).iadd().invokeinterface(FN, "apply", II).iadd().istore(8);
                code.iinc(7, 1).goto_(head);
                code.labelBinding(end);
                code.iload(8).iload(6).iadd().iload(2).iadd().aload(3).invokevirtual(STR, "length", LEN).iadd();
                code.ireturn();
            });
        });
    }

    static MethodHandle[] ms = new MethodHandle[5];
    static MethodHandle m5;

    static String row(int c, Fn op, int live, String s) throws Throwable {
        StringBuilder sb = new StringBuilder();
        for (MethodHandle m : ms) {
            sb.append(' ').append((int) m.invokeExact(c, op, live, s));
        }
        sb.append(' ').append((int) m5.invokeExact((long) live * 1_000_000_007L, c, op));
        return sb.toString();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> g = l.defineClass(generate());
        MethodType shape = MethodType.methodType(int.class, int.class, Fn.class, int.class, String.class);
        String[] names = {"m1", "m2", "m3", "m4", "m6"};
        for (int i = 0; i < names.length; i++) {
            ms[i] = l.findStatic(g, names[i], shape);
        }
        m5 = l.findStatic(g, "m5", MethodType.methodType(int.class, long.class, int.class, Fn.class));
        Fn inc = new Inc();
        int[][] cases = {{0, 3}, {1, 3}, {0, -40}, {7, 100_000}};
        for (int[] cs : cases) {
            System.out.println("row cold c=" + cs[0] + " live=" + cs[1] + " |" + row(cs[0], inc, cs[1], "abc"));
        }
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int c = (r & 1);
            for (MethodHandle m : ms) {
                h = h * 31 + (int) m.invokeExact(c, inc, r & 1023, "xy");
            }
            h = h * 31 + (int) m5.invokeExact((long) r * 3, c, inc);
        }
        System.out.println("warm hash " + h);
        for (int[] cs : cases) {
            System.out.println("row hot c=" + cs[0] + " live=" + cs[1] + " |" + row(cs[0], inc, cs[1], "abc"));
        }
        // The deopt trigger: a receiver class the call sites never saw, on
        // both the split and the unsplit path.
        Fn twice = new Twice();
        for (int[] cs : cases) {
            System.out.println("row late c=" + cs[0] + " live=" + cs[1] + " |" + row(cs[0], twice, cs[1], "abcd"));
        }
        for (int r = 0; r < WARM / 4; r++) {
            Fn op = (r & 3) == 0 ? twice : inc;
            for (MethodHandle m : ms) {
                h = h * 31 + (int) m.invokeExact(r & 1, op, r & 1023, "xy");
            }
        }
        System.out.println("rewarm hash " + h);
        Fn third = v -> v - 7;
        for (int[] cs : cases) {
            System.out.println("row after c=" + cs[0] + " live=" + cs[1] + " |" + row(cs[0], third, cs[1], "q"));
        }
    }
}
