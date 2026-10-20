// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L3: a StackWalker.StackFrame walked
// BEFORE a redefinition of its class and read AFTER it
// (docs/internal/fixed-bugs/interpreter-L3-a-stack-frames-line-and-file-are-not-read-lazily-FIXED-20261005.md,
// item 2). HotSpot builds a frame's StackTraceElement at the first
// getLineNumber / getFileName / toStackTraceElement / toString, and gives it
// no line and no file when the frame's method is no longer the current
// version of its class then; an element built before the redefinition keeps
// what it had.
// Frames, each the first frame of a walk made from the named method:
//   unread -- `Tgt.here()`, nothing read before `Tgt` is redefined (with its
//             own bytes);
//   read   -- `Tgt.here()`, its line read before the redefinition;
//   other  -- `Other.here()`, a class not redefined, nothing read before.
// Each row prints the frame's line (`line` when positive) and file after the
// redefinition.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     unread line=-1 file=null
//     read line=line file=L3W40WalkedFrameReadAfterRedefinition.java
//     other line=line file=L3W40WalkedFrameReadAfterRedefinition.java
// CratonVM before wave 40, read from the code: `unread line=line
// file=L3W40WalkedFrameReadAfterRedefinition.java` (the line is fixed at the
// walk, the file read from the class). Since wave 40 the native walk's
// carrier keeps the walk's redefinition count and settles its line and file
// at the first read (`reflect_invoke::p59_frame_settle`). The JDK walk's
// carrier (CRATONVM_SW_JDK_WALK=1) builds its element at the walk and still
// prints the old row. --compatible prints the same lines as the default.
// Positive control: CRATONVM_DBG_RETRANSFORM=1 prints one
//     [redefine] stack frame read after its class's redefinition: no line: class <id> ...
// line, for the `unread` row.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W40WalkedFrameReadAfterRedefinition$Agent
//     Can-Redefine-Classes: true
// containing L3W40WalkedFrameReadAfterRedefinition*.class (compiled with line
// numbers, javac's default), then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W40WalkedFrameReadAfterRedefinition
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L3W40WalkedFrameReadAfterRedefinition {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** Redefined with its own bytes. */
    public static class Tgt {
        public static StackWalker.StackFrame here() {
            return StackWalker.getInstance().walk(s -> s.findFirst().get());
        }
    }

    /** Not redefined. */
    public static class Other {
        public static StackWalker.StackFrame here() {
            return StackWalker.getInstance().walk(s -> s.findFirst().get());
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W40WalkedFrameReadAfterRedefinition.class
                .getResourceAsStream("L3W40WalkedFrameReadAfterRedefinition$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    static String row(String name, StackWalker.StackFrame f) {
        int line = f.getLineNumber();
        return name + " line=" + (line > 0 ? "line" : Integer.toString(line))
                + " file=" + f.getFileName();
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        StackWalker.StackFrame unread = Tgt.here();
        StackWalker.StackFrame read = Tgt.here();
        StackWalker.StackFrame other = Other.here();
        int before = read.getLineNumber();
        i.redefineClasses(new ClassDefinition(Tgt.class, bytesOf("Tgt")));
        System.out.println(row("unread", unread));
        System.out.println(row("read", read) + (read.getLineNumber() == before ? "" : " moved"));
        System.out.println(row("other", other));
    }
}
