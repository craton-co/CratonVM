// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L2 -- what Java can see of a throwable's
// backtrace, which a VM that keeps it in the throwable
// (CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1) must answer exactly as one that
// keeps it in a registry, and both as HotSpot. Probe for
// docs/internal/fixed-bugs/interpreter-i73-L2-proposal-backtrace-owned-by-the-throwable-KEPT-OFF-20261115.md.
//
//   site-line       the throw site's line, against StackWalker's on the next line
//   unused-then-read  a throwable read only after 1000 dropped ones and a GC
//   arrays          getStackTrace() hands out a fresh, equal array each call
//   print           printStackTrace with a cause ("Caused by", "... 1 more")
//   cap             1101 frames deep: 1024 kept, the innermost
//   set / unwritable  setStackTrace, and the writableStackTrace=false refusal
//   refill-*        fillInStackTrace() from another method, read before / not
//   clone-*         a Cloneable exception's clone shares the trace it was
//                   cloned with; the original's later refill does not move it
//                   (HotSpot's clone copies the `backtrace` reference).
//                   Both switch states since 2026-10-11 (the clone gets its own
//                   registry row in Object.clone()):
//                   docs/internal/fixed-bugs/i82-L2-a-cloned-throwable-does-not-keep-its-trace-FIXED-20261011.md
//   serial          a never-read throwable serialised and read back
//   thread          a throwable built on a thread that has ended
//   implicit-*      VM-raised ArithmeticException / NullPointerException
//   quiet           a fillInStackTrace() override that captures nothing
//   walker          StackWalker and Throwable agree on the frames
//
// Line numbers are cut from the printStackTrace text ("(File.java)") and
// compared relatively elsewhere, so the block does not depend on the layout.
// The block is derived from the JDK 25 sources (Throwable, StackTraceElement,
// StackWalker); refresh it from $J/java before reading a DIFF.
//
// Run each of (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W82LazyBacktraceShapes.java
//   $B --java-home $J/.. -cp out L2W82LazyBacktraceShapes
//   $B --compatible --java-home $J/.. -cp out L2W82LazyBacktraceShapes
//   env CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1 $B --java-home $J/.. -cp out L2W82LazyBacktraceShapes
//   env CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1 $B --compatible --java-home $J/.. -cp out L2W82LazyBacktraceShapes
//   (each also with --nojit)
//   $J/java -cp out L2W82LazyBacktraceShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// site-line top=siteLine walker-minus-trace=1 file=L2W82LazyBacktraceShapes.java
// unused-then-read top=makeAt next=unusedThenRead length=3
// arrays distinct=true equal=true length=2
// print | java.lang.RuntimeException: outer
// print |   at L2W82LazyBacktraceShapes.print(L2W82LazyBacktraceShapes.java)
// print |   at L2W82LazyBacktraceShapes.main(L2W82LazyBacktraceShapes.java)
// print | Caused by: java.lang.IllegalStateException: inner
// print |   at L2W82LazyBacktraceShapes.nested(L2W82LazyBacktraceShapes.java)
// print |   at L2W82LazyBacktraceShapes.nested(L2W82LazyBacktraceShapes.java)
// print |   at L2W82LazyBacktraceShapes.nested(L2W82LazyBacktraceShapes.java)
// print |   at L2W82LazyBacktraceShapes.print(L2W82LazyBacktraceShapes.java)
// print |   ... 1 more
// cap length=1024 first=deep last=deep
// set top=Made.up(Made.java:7) length=1
// unwritable length=0
// refill-read before=L2W82LazyBacktraceShapes.refillRead after=L2W82LazyBacktraceShapes.refillFrom
// refill-unread after=L2W82LazyBacktraceShapes.refillFrom
// clone-shared clone=L2W82LazyBacktraceShapes.makeCopyable
// clone-after-original-refill clone=L2W82LazyBacktraceShapes.makeCopyable original=L2W82LazyBacktraceShapes.refillFrom
// serial top=L2W82LazyBacktraceShapes.makeSerial equal=true length=3
// thread top=L2W82LazyBacktraceShapes$Maker.run
// implicit-arith top=L2W82LazyBacktraceShapes.implicit msg=/ by zero
// implicit-npe top=L2W82LazyBacktraceShapes.implicit
// quiet length=0
// walker same=true
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;

public class L2W82LazyBacktraceShapes {
    static Object sink;

    static final class Copyable extends Exception implements Cloneable {
        Copyable copy() throws CloneNotSupportedException {
            return (Copyable) clone();
        }
    }

    static final class Unwritable extends Exception {
        Unwritable() {
            super("unwritable", null, false, false);
        }
    }

    static final class Quiet extends Exception {
        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    }

    static final class Maker implements Runnable {
        Exception made;

        public void run() {
            made = new Exception("from-thread");
        }
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "<empty>" : st[0].getClassName() + "." + st[0].getMethodName();
    }

    static void siteLine() {
        Exception e = new Exception();
        int walker = StackWalker.getInstance().walk(s -> s.findFirst().get().getLineNumber());
        StackTraceElement[] st = e.getStackTrace();
        System.out.println("site-line top=" + st[0].getMethodName()
                + " walker-minus-trace=" + (walker - st[0].getLineNumber())
                + " file=" + st[0].getFileName());
    }

    static Exception makeAt() {
        return new Exception("kept");
    }

    static void unusedThenRead() {
        Exception kept = makeAt();
        for (int i = 0; i < 1000; i++) {
            sink = new RuntimeException();
        }
        byte[][] junk = new byte[256][];
        for (int i = 0; i < junk.length; i++) {
            junk[i] = new byte[4096];
        }
        sink = junk;
        System.gc();
        StackTraceElement[] st = kept.getStackTrace();
        System.out.println("unused-then-read top=" + st[0].getMethodName()
                + " next=" + st[1].getMethodName() + " length=" + st.length);
    }

    static void arrays() {
        Exception e = new Exception();
        StackTraceElement[] a = e.getStackTrace();
        StackTraceElement[] b = e.getStackTrace();
        System.out.println("arrays distinct=" + (a != b) + " equal=" + Arrays.equals(a, b)
                + " length=" + a.length);
    }

    static Exception nested(int k) {
        return k == 0 ? new IllegalStateException("inner") : nested(k - 1);
    }

    static void print() {
        Exception cause = nested(2);
        Exception outer = new RuntimeException("outer", cause);
        StringWriter sw = new StringWriter();
        outer.printStackTrace(new PrintWriter(sw, true));
        String text = sw.toString()
                .replaceAll("\\(L2W82LazyBacktraceShapes\\.java:\\d+\\)", "(L2W82LazyBacktraceShapes.java)")
                .replace("\r\n", "\n")
                .replace("\t", "  ");
        for (String line : text.split("\n")) {
            System.out.println("print | " + line);
        }
    }

    static Exception deep(int k) {
        return k == 0 ? new Exception() : deep(k - 1);
    }

    static void cap() {
        StackTraceElement[] st = deep(1100).getStackTrace();
        System.out.println("cap length=" + st.length + " first=" + st[0].getMethodName()
                + " last=" + st[st.length - 1].getMethodName());
    }

    static void set() {
        Exception e = new Exception();
        e.setStackTrace(new StackTraceElement[] {new StackTraceElement("Made", "up", "Made.java", 7)});
        System.out.println("set top=" + e.getStackTrace()[0] + " length=" + e.getStackTrace().length);
        Unwritable u = new Unwritable();
        u.setStackTrace(new StackTraceElement[] {new StackTraceElement("Made", "up", "Made.java", 7)});
        System.out.println("unwritable length=" + u.getStackTrace().length);
    }

    static void refillFrom(Throwable t) {
        t.fillInStackTrace();
    }

    static void refillRead() {
        Exception e = new Exception();
        String before = top(e);
        refillFrom(e);
        System.out.println("refill-read before=" + before + " after=" + top(e));
    }

    static void refillUnread() {
        Exception e = new Exception();
        refillFrom(e);
        System.out.println("refill-unread after=" + top(e));
    }

    static Copyable makeCopyable() {
        return new Copyable();
    }

    static void cloned() throws CloneNotSupportedException {
        Copyable c = makeCopyable();
        Copyable k = c.copy();
        System.out.println("clone-shared clone=" + top(k));
        Copyable c2 = makeCopyable();
        Copyable k2 = c2.copy();
        refillFrom(c2);
        System.out.println("clone-after-original-refill clone=" + top(k2) + " original=" + top(c2));
    }

    static Exception makeSerial() {
        return new Exception("serial");
    }

    static void serial() throws Exception {
        Exception e = makeSerial();
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
            oos.writeObject(e);
        }
        Exception back;
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            back = (Exception) ois.readObject();
        }
        System.out.println("serial top=" + top(back)
                + " equal=" + Arrays.equals(back.getStackTrace(), e.getStackTrace())
                + " length=" + back.getStackTrace().length);
    }

    static void thread() throws InterruptedException {
        Maker m = new Maker();
        Thread t = new Thread(m, "maker");
        t.start();
        t.join();
        System.out.println("thread top=" + top(m.made));
    }

    static String nothing() {
        return null;
    }

    static void implicit() {
        int zero = Integer.parseInt("0");
        try {
            sink = 1 / zero;
        } catch (ArithmeticException ae) {
            System.out.println("implicit-arith top=" + top(ae) + " msg=" + ae.getMessage());
        }
        String s = nothing();
        try {
            sink = s.length();
        } catch (NullPointerException npe) {
            System.out.println("implicit-npe top=" + top(npe));
        }
    }

    static void quiet() {
        System.out.println("quiet length=" + new Quiet().getStackTrace().length);
    }

    static void walker() {
        Exception e = new Exception();
        List<String> walked = StackWalker.getInstance()
                .walk(s -> s.map(StackWalker.StackFrame::getMethodName).collect(Collectors.toList()));
        List<String> traced = new ArrayList<>();
        for (StackTraceElement el : e.getStackTrace()) {
            traced.add(el.getMethodName());
        }
        System.out.println("walker same=" + walked.equals(traced));
    }

    public static void main(String[] args) throws Exception {
        siteLine();
        unusedThenRead();
        arrays();
        print();
        cap();
        set();
        refillRead();
        refillUnread();
        cloned();
        serial();
        thread();
        implicit();
        quiet();
        walker();
    }
}
