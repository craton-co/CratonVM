// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L3 -- the frames on top of the
// throwables the VM's `StackWalker` natives raise (`stack_walker.rs`):
// HotSpot runs the JDK methods and starts each trace at their throw; the
// funnel's mark lists them (`stackwalker::served_raise_frames`): the method's
// one construction, its one call of `Objects.requireNonNull`, or the one invoke
// an implicit NPE's message names. Each row prints the throwable's class and
// message, the frames above the probe's first frame as `class.method`, then
// the first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   caller-class    -- `getCallerClass()` without RETAIN_CLASS_REFERENCE;
//   null-set        -- `getInstance((Set) null)`: `options.isEmpty()`;
//   null-option     -- `getInstance((Option) null)`: `requireNonNull`;
//   depth-0         -- `getInstance(noneOf, 0)`: the depth check;
//   caller-again    -- `caller-class` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// caller-class: java.lang.UnsupportedOperationException This stack walker does not have RETAIN_CLASS_REFERENCE access [java.lang.StackWalker.getCallerClass] L3W53StackWalkerRaiseFrames.main
// null-set: java.lang.NullPointerException Cannot invoke "java.util.Set.isEmpty()" because "options" is null [java.lang.StackWalker.getInstance] L3W53StackWalkerRaiseFrames.main
// null-option: java.lang.NullPointerException null [java.util.Objects.requireNonNull java.lang.StackWalker.getInstance] L3W53StackWalkerRaiseFrames.main
// depth-0: java.lang.IllegalArgumentException estimateDepth must be > 0 [java.lang.StackWalker.getInstance] L3W53StackWalkerRaiseFrames.main
// caller-again: java.lang.UnsupportedOperationException This stack walker does not have RETAIN_CLASS_REFERENCE access [java.lang.StackWalker.getCallerClass] L3W53StackWalkerRaiseFrames.main
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W53StackWalkerRaiseFrames`.
import java.util.EnumSet;
import java.util.Set;

public class L3W53StackWalkerRaiseFrames {
    static void print(String name, Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder jdk = new StringBuilder();
        int i = 0;
        while (i < st.length && !st[i].getClassName().startsWith("L3W53")) {
            jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
            i++;
        }
        String first = i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
        System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
    }

    public static void main(String[] args) {
        StackWalker plain = StackWalker.getInstance();
        try { System.out.println("caller-class: " + plain.getCallerClass()); } catch (Throwable t) { print("caller-class", t); }
        try { System.out.println("null-set: " + StackWalker.getInstance((Set<StackWalker.Option>) null)); } catch (Throwable t) { print("null-set", t); }
        try { System.out.println("null-option: " + StackWalker.getInstance((StackWalker.Option) null)); } catch (Throwable t) { print("null-option", t); }
        try { System.out.println("depth-0: " + StackWalker.getInstance(EnumSet.noneOf(StackWalker.Option.class), 0)); } catch (Throwable t) { print("depth-0", t); }
        try { System.out.println("caller-again: " + plain.getCallerClass()); } catch (Throwable t) { print("caller-again", t); }
    }
}
