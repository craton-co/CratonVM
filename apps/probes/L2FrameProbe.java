/** What is the TOP stack frame of a throwable built by real JDK bytecode?
 *
 *  Retiring `VirtualMachineError.<init>` and friends took eight probe rows from
 *  "top frame is the constructing class" to false. HotSpot's `fillInStackTrace`
 *  skips the frames belonging to the throwable's own constructor chain; this
 *  asks what this VM leaves on top instead, and pairs each retired shape with
 *  an UNRETIRED throwable as the control -- if the control is also wrong the
 *  defect is in `fillInStackTrace`, and if only the retired ones are wrong it
 *  is in what the native was doing for them.
 */
public class L2FrameProbe {
    static int rows = 0;

    static void show(String tag, Throwable t) {
        StackTraceElement[] s = t.getStackTrace();
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < Math.min(4, s.length); i++) {
            if (i > 0) b.append(" / ");
            b.append(s[i].getClassName()).append('.').append(s[i].getMethodName());
        }
        System.out.println(tag + " depth=" + s.length + " top=|" + b + "|");
        rows++;
    }

    public static void main(String[] a) {
        // RETIRED in the trial binary
        show("VirtualMachineError", new VirtualMachineError() {});
        show("ExceptionInInitializerError", new ExceptionInInitializerError());
        show("IllegalThreadStateException", new IllegalThreadStateException("m"));
        show("UnsatisfiedLinkError", new UnsatisfiedLinkError("m"));
        show("NullPointerException", new NullPointerException("m"));
        // CONTROLS: never registered by this lane, so real bytecode either way
        show("ctrl RuntimeException", new RuntimeException("m"));
        show("ctrl Throwable", new Throwable("m"));
        show("ctrl IllegalStateException", new IllegalStateException("m"));
        show("ctrl Error", new Error("m"));
        System.out.println("rows " + rows);
        System.out.println("DONE L2FrameProbe");
    }
}
