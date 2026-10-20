// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L3 -- review evidence for
// docs/internal/fixed-bugs/interpreter-L3-a-served-doprivileged-lists-no-accesscontroller-frame-FIXED-20261013.md:
// a `PrivilegedAction` run by `AccessController.doPrivileged` has the
// `AccessController.doPrivileged` frame below it in every view on HotSpot;
// this VM serves `doPrivileged` with a bridge that runs the action itself,
// so no such frame exists.
//
// Rows print the frames from the action's body down to (not including)
// `main`, as `Class.method@bci` for walks and `Class.method:line` for a
// Throwable.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// action-walk: P.lambda$main$0@0 < java.security.AccessController.doPrivileged@1
// action-trace: P.lambda$main$1:42 < java.security.AccessController.doPrivileged:74
// exception-action-walk: P.lambda$main$4@0 < java.security.AccessController.doPrivileged@1
// HOTSPOT_EXPECTED_END
//
// CratonVM (wave 48, both modes; local run): the `doPrivileged` frame is
// missing from every row.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W48PrivilegedFrames`.
import java.security.AccessController;
import java.security.PrivilegedAction;
import java.security.PrivilegedExceptionAction;
import java.util.Arrays;
import java.util.stream.Collectors;

public class L3W48PrivilegedFrames {
    static String walk() {
        return StackWalker.getInstance().walk(s -> s.skip(1)
                .map(f -> f.getClassName().replace("L3W48PrivilegedFrames", "P") + "." + f.getMethodName()
                        + "@" + f.getByteCodeIndex())
                .takeWhile(n -> !n.startsWith("P.main"))
                .collect(Collectors.joining(" < ")));
    }

    public static void main(String[] args) throws Exception {
        System.out.println("action-walk: " + AccessController.doPrivileged((PrivilegedAction<String>) () -> walk()));
        Throwable t = AccessController.doPrivileged((PrivilegedAction<Throwable>) () -> new Throwable());
        System.out.println("action-trace: " + Arrays.stream(t.getStackTrace())
                .map(e -> e.getClassName().replace("L3W48PrivilegedFrames", "P") + "." + e.getMethodName()
                        + ":" + e.getLineNumber())
                .takeWhile(n -> !n.startsWith("P.main"))
                .collect(Collectors.joining(" < ")));
        System.out.println("exception-action-walk: "
                + AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> walk()));
    }
}
