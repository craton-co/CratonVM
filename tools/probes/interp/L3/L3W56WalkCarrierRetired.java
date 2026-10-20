// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 --
// docs/internal/fixed-bugs/interpreter-i55-L3-proposal-retire-the-p59-stack-frame-carrier-DONE-20261014.md:
// `CRATONVM_RETIRE_WALK_CARRIER` retires the VM's `StackWalker$StackFrame`
// carrier over a real JDK: the native walk always hands out the JDK's frame
// classes, and the carrier's natives on the INTERFACE are not registered --
// the two that stood in front of the interface's DEFAULT `getMethodType()` /
// `getDescriptor()` bodies among them, so a `ClassFrameInfo` and a class of
// the program's own implementing `StackFrame` run those bodies (`throw new
// UnsupportedOperationException()`), as on HotSpot.
//
// Each raise row prints the throwable's class and message, the frames above
// the probe's first frame as `class.method`, then the first probe frame.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   user-getMethodType / user-getDescriptor -- the default bodies on a
//       `StackFrame` of the probe's own class;
//   frame-class / dropped-class -- the class of a default walker's frame and
//       of a `DROP_METHOD_INFO` walker's;
//   dropped-getMethodType / dropped-getDescriptor -- the default bodies on a
//       `ClassFrameInfo`;
//   retained-descriptor -- a RETAIN_CLASS_REFERENCE walker's frame descriptor
//       (control: `StackFrameInfo`'s own body).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// user-getMethodType: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getMethodType] L3W56WalkCarrierRetired.lambda$main$0
// user-getDescriptor: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getDescriptor] L3W56WalkCarrierRetired.lambda$main$1
// frame-class: java.lang.StackFrameInfo
// dropped-class: java.lang.ClassFrameInfo
// dropped-getMethodType: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getMethodType] L3W56WalkCarrierRetired.lambda$main$4
// dropped-getDescriptor: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getDescriptor] L3W56WalkCarrierRetired.lambda$main$5
// retained-descriptor: ([Ljava/lang/String;)V
// user-getMethodType-again: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getMethodType] L3W56WalkCarrierRetired.lambda$main$7
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 56: `--compatible` answered both `user-*` rows (and,
// with `CRATONVM_COMPATIBLE_JDK_WALK_FRAMES=0`, every frame row) with the
// carrier natives' `No access to RETAIN_CLASS_REFERENCE`; `--jdk-only` ran the
// default body for the first `user-getMethodType` and the native for the
// `-again` row (the vtable cache's forced pair).
//
// Positive control: with `CRATONVM_COMPATIBLE_JDK_WALK_FRAMES=0` and the
// switch on, `--compatible`'s `frame-class` row still prints
// `java.lang.StackFrameInfo`, and `CRATONVM_FRAME_TRACE=1` prints
//     [WALK_CARRIER] retired: the walk builds the JDK's frame classes
// (with the switch off it prints `java.lang.StackWalker$StackFrame`).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56WalkCarrierRetired`.
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.EnumSet;

public class L3W56WalkCarrierRetired {
    /** A frame of the program's own: the interface's default bodies run. */
    static final class OwnFrame implements StackFrame {
        public String getClassName() { return "Own"; }
        public String getMethodName() { return "own"; }
        public Class<?> getDeclaringClass() { return OwnFrame.class; }
        public int getByteCodeIndex() { return 0; }
        public String getFileName() { return null; }
        public int getLineNumber() { return -1; }
        public boolean isNativeMethod() { return false; }
        public StackTraceElement toStackTraceElement() { return null; }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            Object v = r.run();
            System.out.println(name + ": " + v);
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int k = 0;
            while (k < st.length && !st[k].getClassName().startsWith("L3W56")) {
                jdk.append(k == 0 ? "" : " ").append(st[k].getClassName()).append('.').append(st[k].getMethodName());
                k++;
            }
            String first = k < st.length ? st[k].getClassName() + "." + st[k].getMethodName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    public static void main(String[] args) {
        StackFrame own = new OwnFrame();
        row("user-getMethodType", () -> own.getMethodType());
        row("user-getDescriptor", () -> own.getDescriptor());
        StackFrame frame = StackWalker.getInstance().walk(s -> s.findFirst().get());
        System.out.println("frame-class: " + frame.getClass().getName());
        StackFrame dropped = StackWalker.getInstance(Option.DROP_METHOD_INFO).walk(s -> s.findFirst().get());
        System.out.println("dropped-class: " + dropped.getClass().getName());
        row("dropped-getMethodType", () -> dropped.getMethodType());
        row("dropped-getDescriptor", () -> dropped.getDescriptor());
        StackFrame retained = StackWalker.getInstance(EnumSet.of(Option.RETAIN_CLASS_REFERENCE))
                .walk(s -> s.findFirst().get());
        System.out.println("retained-descriptor: " + retained.getDescriptor());
        row("user-getMethodType-again", () -> own.getMethodType());
    }
}
