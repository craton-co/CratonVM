// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 13, lane L5: an inherited static reached through
// the subclass (`C.m()` for an `m` declared in `P`, which javac compiles to
// `invokestatic JitInheritedStaticInit$C.m`) initializes P only (JVMS 5.5,
// JLS 12.4.1) — also when the call is first executed from COMPILED code.
//
// The interpreter's `execute_invokestatic` initializes the declaring class
// (`invokestatic_init_class`). The by-name tails do not: `invoke_or_native`
// dispatches on the symbolic owner (`invoke_by_class_id_shared` / the
// `invoke_shared` fallback), and both initialize THAT class. `hot` is
// compiled while its `C.m()` branch has never run, so the first execution of
// the branch is a compiled call site; if it reaches the by-name tail,
// CratonVM prints "C.<clinit>" before "m=7".
//
// Run with the default settings and with --nojit; diff against HotSpot 25,
// which prints exactly:
//
//   warm 0
//   P.<clinit>
//   m=7
//   done
public class JitInheritedStaticInit {
    static class P {
        static {
            System.out.println("P.<clinit>");
        }

        static int m() {
            return 7;
        }
    }

    static class C extends P {
        static {
            System.out.println("C.<clinit>");
        }
    }

    static int hot(int i, boolean take) {
        if (take) {
            return C.m();
        }
        return i & 1;
    }

    public static void main(String[] args) {
        int sum = 0;
        for (int round = 0; round < 400; round++) {
            for (int i = 0; i < 1000; i++) {
                sum += hot(i, false);
            }
        }
        System.out.println("warm " + (sum - 200000));
        System.out.println("m=" + hot(0, true));
        System.out.println("done");
    }
}
