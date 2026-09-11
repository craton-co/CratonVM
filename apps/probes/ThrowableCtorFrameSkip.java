/** `getStackTrace()[0]` against HotSpot, for the frame skip `H22-2` priced.
 *
 *  HotSpot's `fillInStackTrace` skips every frame up to and including the LAST
 *  `<init>` of a `Throwable` subclass, so the top frame is the method that did
 *  `new`. `H22-2` measured CratonVM's armed/retired path losing that skip on
 *  61 of 61 classes, and recorded the unarmed path as "correct by
 *  construction" because the native `<init>` captures from a native frame.
 *
 *  That last part holds only for a JDK throwable instantiated DIRECTLY. This
 *  probe is 4 + 5 rows and it takes 20 seconds:
 *
 *    A/D  JDK throwable, direct `new`     -> correct on both VMs
 *    B/C  application subclass, depth 1/2 -> CratonVM keeps the constructor
 *                                            chain; depth 2 tops at D1.<init>,
 *                                            the native's immediate caller
 *    5    explicit fillInStackTrace()     -> CratonVM tops at
 *                                            Throwable.fillInStackTrace
 *
 *  The native drops only the ctors that ARE natives, which is the JDK half of
 *  the chain. Every application-level `<init>` survives -- so a user-defined
 *  exception, at any depth, reports its own constructor as the throw site in
 *  the SHIPPING configuration, not only under
 *  `register_throwable_subclass_natives`.
 *
 *  Run it the way the numbers above were taken:
 *
 *    javac -d apps/probes/out apps/probes/ThrowableCtorFrameSkip.java
 *    "$JDK/bin/java"  -cp apps/probes/out ThrowableCtorFrameSkip
 *    cratonvm --java-home "$JDK" --jdk-only -cp apps/probes/out ThrowableCtorFrameSkip
 *
 *  Every row is one line, `<tag> |<value>|`, so `diff -a` scores it. Use `-a`:
 *  a probe that writes one NUL makes `diff` answer "Binary files differ",
 *  which `grep -c '^[<>]'` scores as a PERFECT match.
 */
public class ThrowableCtorFrameSkip {

    static class Custom extends RuntimeException {
        Custom(String m) { super(m); }
    }

    /** Depth 1 and 2, to show WHICH frame survives rather than just that one does. */
    static class D1 extends RuntimeException { D1() { super("d1"); } }
    static class D2 extends D1 { D2() { super(); } }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "<no frames>"
                : st[0].getClassName() + "." + st[0].getMethodName();
    }

    static Throwable jdkDirect()     { return new RuntimeException("r"); }
    static Throwable appDepth1()     { return new D1(); }
    static Throwable appDepth2()     { return new D2(); }
    static Throwable jdkChecked()    { return new java.io.IOException("i"); }
    static Throwable jdkReflective() { return new java.lang.reflect.InaccessibleObjectException("n"); }

    public static void main(String[] args) {
        System.out.println("A JDK direct RuntimeException |" + top(jdkDirect()) + "|");
        System.out.println("B app subclass depth 1        |" + top(appDepth1()) + "|");
        System.out.println("C app subclass depth 2        |" + top(appDepth2()) + "|");
        System.out.println("D JDK direct IOException      |" + top(jdkChecked()) + "|");
        System.out.println("E JDK InaccessibleObjectExc   |" + top(jdkReflective()) + "|");
        System.out.println("F new Exception               |" + top(new Exception("p")) + "|");
        try {
            throw new Custom("t");
        } catch (Throwable t) {
            System.out.println("G thrown and caught           |" + top(t) + "|");
        }
        System.out.println("H explicit fillInStackTrace   |"
                + top(new Custom("f").fillInStackTrace()) + "|");
        // A trace captured OUTSIDE any constructor is the control: if this row
        // ever differs, the defect is not the constructor skip.
        System.out.println("I control, no ctor in the way |"
                + top(Thread.currentThread().getStackTrace().length > 0
                      ? new Exception("c") : new Exception("c")) + "|");
    }
}
