// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L4 (review): the invoke-exact flag of a
// memory-layout VarHandle
// (docs/internal/fixed-bugs/interpreter-L4-ffm-layout-varhandles-write-their-width-into-the-exact-field-FIXED-20261001.md).
// CratonVM's FFM layout mint (`phases_late/foreign_ffm.rs`) wrote the
// layout's width into slot 1 of a real-layout `VarHandle`, which is
// `VarHandle.exact`, so `hasInvokeExactBehavior()` (JDK bytecode reading that
// field) answered `true` for every plain layout handle (wave-37 integration
// run: byte/int/long/int-exact-then-plain/path-int printed `true`, default and
// `--nojit`). Fixed in wave 37: the mint writes those slots on the synthetic
// layout only, and `withInvokeExactBehavior()` copies a layout handle with the
// flag set.
//
// Run: javac -d out L4W37FfmVarHandleExactFlag.java && cratonvm --java-home <jdk25> [--nojit] -cp out L4W37FfmVarHandleExactFlag
//
// Expected HotSpot 25 output (default and -Xint):
//   byte: false
//   int: false
//   long: false
//   int-exact: true
//   int-exact-then-plain: false
//   path-int: false
import java.lang.foreign.ValueLayout;
import java.lang.invoke.VarHandle;

public class L4W37FfmVarHandleExactFlag {
    public static void main(String[] args) {
        VarHandle b = ValueLayout.JAVA_BYTE.varHandle();
        VarHandle i = ValueLayout.JAVA_INT.varHandle();
        VarHandle l = ValueLayout.JAVA_LONG.varHandle();
        System.out.println("byte: " + b.hasInvokeExactBehavior());
        System.out.println("int: " + i.hasInvokeExactBehavior());
        System.out.println("long: " + l.hasInvokeExactBehavior());
        VarHandle ie = i.withInvokeExactBehavior();
        System.out.println("int-exact: " + ie.hasInvokeExactBehavior());
        System.out.println("int-exact-then-plain: " + ie.withInvokeBehavior().hasInvokeExactBehavior());
        VarHandle p = java.lang.foreign.MemoryLayout.structLayout(ValueLayout.JAVA_INT.withName("x"))
                .varHandle(java.lang.foreign.MemoryLayout.PathElement.groupElement("x"));
        System.out.println("path-int: " + p.hasInvokeExactBehavior());
    }
}
