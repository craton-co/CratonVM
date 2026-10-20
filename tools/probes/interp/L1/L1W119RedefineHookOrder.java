// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L1 -- stage 2 of
// docs/internal/fixed-bugs/interpreter-i27-L3-proposal-verify-a-redefinition-before-installing-any-class-DONE-20261224.md:
// a C agent's `ClassFileLoadHook` runs for EVERY class of a
// `RedefineClasses` call before any class is installed, and its output is
// what the call verifies. HotSpot posts the hook while it parses the new
// versions (`VM_RedefineClasses::load_new_class_versions`), so a hook that
// hands back a class file failing verification for the call's second class
// leaves the first unchanged.
//
// `A.value()` returns 1 and `B.value()` 2. The driver asks one
// `RedefineClasses` for both (A returning 3, B returning 4) twice:
//
//   bad hook   the agent's hook replaces B's class file with one whose
//              `value()` is `nop; ireturn` (a stack underflow): the call
//              fails verification and neither class changes;
//   no hook    the hook hands back nothing: both change.
//
// Needs the agent L1W119RedefineHookOrder.c (build lines there), loaded both
// as an agent and with System.load:
//
//   javac -d %TEMP%\l1w119 L1W119RedefineHookOrder.java
//   java     -agentpath:%TEMP%\L1W119RedefineHookOrder.dll -cp %TEMP%\l1w119 L1W119RedefineHookOrder %TEMP%\L1W119RedefineHookOrder.dll
//   cratonvm [--compatible] -agentpath:%TEMP%\L1W119RedefineHookOrder.dll -cp %TEMP%\l1w119 L1W119RedefineHookOrder %TEMP%\L1W119RedefineHookOrder.dll
//
// Without the argument it prints only the usage line. CratonVM posts a C
// agent's ClassFileLoadHook only in a build with
// `--features cratonvm-vm/experimental-debug`.
//
// HotSpot 25.0.3 (Windows, MSVC-built agent, run in wave 119):
//
//   before: a=1 b=2
//   bad hook: error=62 hooked=2 a=1 b=2
//   no hook: error=0 hooked=2 a=3 b=4
//
// (62 is `JVMTI_ERROR_FAILS_VERIFICATION`; `hooked=2`: the hook ran for A
// and for B in each call.) The stdout block for the battery:
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, run in wave 119; needs the agent)
// before: a=1 b=2
// bad hook: error=62 hooked=2 a=1 b=2
// no hook: error=0 hooked=2 a=3 b=4
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;

public class L1W119RedefineHookOrder {
    static final class A {
        static int value() {
            return 1;
        }
    }

    static final class B {
        static int value() {
            return 2;
        }
    }

    static native void setBad(byte[] bytes);

    static native int hookCalls();

    static native int redefine(Class<?> a, byte[] ab, Class<?> b, byte[] bb);

    static byte[] bytesOf(Class<?> c) throws IOException {
        String resource = c.getName().substring(c.getName().lastIndexOf('.') + 1) + ".class";
        try (InputStream in = c.getResourceAsStream(resource)) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with its one `<from>; ireturn` made `<to>; ireturn`. */
    static byte[] patched(byte[] bytes, int from, int to) {
        byte[] out = bytes.clone();
        int found = -1;
        for (int i = 0; i + 1 < out.length; i++) {
            if ((out[i] & 0xff) == from && (out[i + 1] & 0xff) == 0xac) {
                if (found >= 0) {
                    throw new IllegalStateException("not unique: " + from);
                }
                found = i;
            }
        }
        if (found < 0) {
            throw new IllegalStateException("not found: " + from);
        }
        out[found] = (byte) to;
        return out;
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W119RedefineHookOrder <path of the agent library>");
            return;
        }
        System.load(args[0]);
        final int iconst1 = 0x04, iconst2 = 0x05, iconst3 = 0x06, iconst4 = 0x07, nop = 0x00;
        byte[] a3 = patched(bytesOf(A.class), iconst1, iconst3);
        byte[] b4 = patched(bytesOf(B.class), iconst2, iconst4);
        byte[] bBad = patched(bytesOf(B.class), iconst2, nop);
        System.out.println("before: a=" + A.value() + " b=" + B.value());

        setBad(bBad);
        int calls = hookCalls();
        int err = redefine(A.class, a3, B.class, b4);
        System.out.println("bad hook: error=" + err + " hooked=" + (hookCalls() - calls)
                + " a=" + A.value() + " b=" + B.value());

        setBad(null);
        calls = hookCalls();
        err = redefine(A.class, a3, B.class, b4);
        System.out.println("no hook: error=" + err + " hooked=" + (hookCalls() - calls)
                + " a=" + A.value() + " b=" + B.value());
    }
}
