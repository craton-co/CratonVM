// Lane L5 probe: throwable stack traces are capped at MaxJavaStackTraceDepth.
//
// HotSpot 25 (default -XX:MaxJavaStackTraceDepth=1024) prints:
//   deep 1024
//   soe 1024
//   soe-top recurse
// CratonVM before round i1 wave 2 captured the whole stack, so `deep` printed
// ~3002 and `soe` however deep the recursion got; it now caps at 1024 like
// HotSpot. See
// docs/internal/fixed-bugs/interpreter-L5-stack-trace-depth-is-uncapped-FIXED-20260923.md.
public class StackTraceDepthCap {
    static int deepLen(int n) {
        if (n == 0) {
            return new RuntimeException("deep").getStackTrace().length;
        }
        return deepLen(n - 1);
    }

    static int sink;

    static void recurse(int n) {
        sink += n;
        recurse(n + 1);
    }

    public static void main(String[] args) {
        System.out.println("deep " + deepLen(3000));
        try {
            recurse(0);
        } catch (StackOverflowError e) {
            StackTraceElement[] st = e.getStackTrace();
            System.out.println("soe " + st.length);
            System.out.println("soe-top " + (st.length > 0 ? st[0].getMethodName() : "<none>"));
        }
    }
}
