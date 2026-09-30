// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L2: the interpreter's virtual call-site
// doors tier up a target redefined before the entry was filled
// (docs/internal/fixed-bugs/interpreter-L2-interpreter-call-site-doors-never-tier-up-a-redefined-target-FIXED-20261004.md).
// Wave 39 lifted the policy at the static and non-virtual doors; the virtual
// fast door and `execute_invokevirtual_cached`'s `VirtualBytecode` arm still
// served such a target interpreted and uncounted, and a compiled caller's
// bytecode-callee templates declined it. Performance only: the output is the
// correctness guard (the redefined body must be the one that runs, compiled
// or not).
//
// `Target.add` (`x * 31 + 7`) is called through `invokevirtual` and
// `invokeinterface`, warm; then `Target` is redefined with the literal 7
// patched to 9 (`bipush 7` -> `bipush 9`), and the same sites run 40 rounds
// more. Each row prints the round's sum.
//
// HotSpot 25 (agent; the same with -Xint; local JDK 25.0.3):
//   before virtual=380327344 interface=380327344
//   after virtual=380527344 interface=380527344
//   after-last virtual=380527344 interface=380527344
// Expected on CratonVM in every mode: the same three lines.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W40RedefinedVirtualTargetTiersUp$Agent
//     Can-Redefine-Classes: true
// containing L2W40RedefinedVirtualTargetTiersUp*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W40RedefinedVirtualTargetTiersUp
// Without the agent both VMs print "no agent".
//
// Positive control (default mode): keep the two callers interpreted, so the
// redefined `add` is reached only through the interpreter's virtual door and
// `VirtualBytecode` arm:
//     CRATONVM_DBG_JITC=1 CRATONVM_JIT_DENY=L2W40RedefinedVirtualTargetTiersUp.drive \
//       cratonvm -javaagent:probe.jar -cp probe.jar L2W40RedefinedVirtualTargetTiersUp 2>&1 \
//       | grep 'bg-compile L2W40RedefinedVirtualTargetTiersUp$Target.add(I)I.*redefined-class'
// prints a line (the method-entry compile of the redefined target, offered
// by the door's counter). Before wave 40 there was none: the doors never
// counted a redefined `VirtualBytecode` target. With the callers compiled
// (no JIT_DENY), the template path (`jit::helpers`' bytecode-callee
// templates) serves the redefined target and nominates it the same way.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W40RedefinedVirtualTargetTiersUp {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public interface Adder {
        int add(int x);
    }

    public static class Target implements Adder {
        public int add(int x) {
            return x * 31 + 7;
        }
    }

    static final int N = 100_000;

    static int driveVirtual(Target t, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += t.add(i);
        }
        return acc;
    }

    static int driveInterface(Adder a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += a.add(i);
        }
        return acc;
    }

    /// `bytes` with `bipush 31; imul; bipush 7` turned into `...; bipush 9`.
    static byte[] patched(byte[] bytes) {
        byte[] b = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 5 <= b.length; i++) {
            if (b[i] == 0x10 && b[i + 1] == 31 && b[i + 2] == 0x68 && b[i + 3] == 0x10
                    && b[i + 4] == 7) {
                b[i + 4] = 9;
                hits++;
            }
        }
        if (hits != 1) {
            throw new AssertionError("pattern found " + hits + " times");
        }
        return b;
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] own;
        try (InputStream in = L2W40RedefinedVirtualTargetTiersUp.class
                .getResourceAsStream("L2W40RedefinedVirtualTargetTiersUp$Target.class")) {
            own = in.readAllBytes();
        }
        Target t = new Target();
        int v = 0;
        int f = 0;
        for (int r = 0; r < 20; r++) {
            v = driveVirtual(t, N);
            f = driveInterface(t, N);
        }
        System.out.println("before virtual=" + v + " interface=" + f);
        i.redefineClasses(new ClassDefinition(Target.class, patched(own)));
        v = driveVirtual(t, N);
        f = driveInterface(t, N);
        System.out.println("after virtual=" + v + " interface=" + f);
        for (int r = 0; r < 40; r++) {
            v = driveVirtual(t, N);
            f = driveInterface(t, N);
        }
        System.out.println("after-last virtual=" + v + " interface=" + f);
    }
}
