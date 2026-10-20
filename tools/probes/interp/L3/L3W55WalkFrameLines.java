// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L3 -- the line, file and element of the
// frames of a `StackWalker` walk, read after the walk, for a walk deeper than
// 128 frames too. Under `CRATONVM_SFI_WALK_LINES` (proposal
// `docs/internal/fixed-bugs/interpreter-L3-proposal-a-jdk-walk-frame-keeps-its-walk-entry-not-an-element-FIXED-20261019.md`) a
// walk's `StackFrameInfo` keeps no element: its first read builds one from the
// frame and ONE `int[]` per walk holding every frame's line; the first 128
// frames of a walk do, the rest are built as before. Every row must read the
// same either way, and as HotSpot's.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   first-*      -- the walk's first frame: line, file, element, toString;
//   caller-*     -- its caller's frame, read before the first frame's;
//   deep         -- a walk at depth 200 against `Throwable.getStackTrace()`
//                   from the same method: every frame but the first agrees in
//                   class, method, file and line (read through
//                   `getLineNumber()`, `getFileName()` and the element);
//   deep-150     -- the frame 150 deep's element, read after the frame 0's;
//   first-again  -- `first-*` again, from a fresh walk.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// first-line: 47
// first-file: L3W55WalkFrameLines.java
// first-element: L3W55WalkFrameLines.first(L3W55WalkFrameLines.java:47)
// first-string: L3W55WalkFrameLines.first(L3W55WalkFrameLines.java:47)
// caller-element: L3W55WalkFrameLines.main(L3W55WalkFrameLines.java:70)
// caller-line: 70
// caller-native: false
// deep: frames=202 agree=201
// deep-150: L3W55WalkFrameLines.deep(L3W55WalkFrameLines.java:52)
// deep-150-line: 52
// first-again: L3W55WalkFrameLines.first(L3W55WalkFrameLines.java:47)
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W55WalkFrameLines`.
// (`CRATONVM_SFI_WALK_LINES=1` with `CRATONVM_DEBUG_STACKWALK=1` prints
// `[SW-DBG] walk lines: frame <i> ...` per frame that keeps the walk's lines.)
import java.util.List;

public class L3W55WalkFrameLines {
    static List<StackWalker.StackFrame> deepFrames;
    static StackTraceElement[] deepTrace;

    static List<StackWalker.StackFrame> first() {
        return StackWalker.getInstance().walk(s -> s.limit(2).toList());
    }

    static void deep(int n) {
        if (n > 0) {
            deep(n - 1);
            return;
        }
        deepFrames = StackWalker.getInstance().walk(s -> s.toList());
        deepTrace = new Throwable().getStackTrace();
    }

    static boolean agree(StackWalker.StackFrame f, StackTraceElement e) {
        StackTraceElement fe = f.toStackTraceElement();
        return f.getClassName().equals(e.getClassName())
                && f.getMethodName().equals(e.getMethodName())
                && f.getLineNumber() == e.getLineNumber()
                && java.util.Objects.equals(f.getFileName(), e.getFileName())
                && fe.getLineNumber() == e.getLineNumber()
                && java.util.Objects.equals(fe.getFileName(), e.getFileName());
    }

    public static void main(String[] args) {
        List<StackWalker.StackFrame> two = first();
        StackWalker.StackFrame caller = two.get(1);
        StackWalker.StackFrame f = two.get(0);
        System.out.println("first-line: " + f.getLineNumber());
        System.out.println("first-file: " + f.getFileName());
        System.out.println("first-element: " + f.toStackTraceElement());
        System.out.println("first-string: " + f);
        System.out.println("caller-element: " + caller.toStackTraceElement());
        System.out.println("caller-line: " + caller.getLineNumber());
        System.out.println("caller-native: " + caller.isNativeMethod());
        deep(200);
        int agree = 0;
        for (int i = 1; i < deepFrames.size() && i < deepTrace.length; i++) {
            if (agree(deepFrames.get(i), deepTrace[i])) {
                agree++;
            }
        }
        System.out.println("deep: frames=" + deepFrames.size() + " agree=" + agree);
        deepFrames.get(0).toStackTraceElement();
        System.out.println("deep-150: " + deepFrames.get(150).toStackTraceElement());
        System.out.println("deep-150-line: " + deepFrames.get(150).getLineNumber());
        System.out.println("first-again: " + first().get(0).toStackTraceElement());
    }
}
