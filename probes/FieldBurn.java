// A single-shape workload for a CPU-TIME A/B: almost every bytecode executed
// is the getfield/putfield pair under test. No self-timing — the instrument is
// the process's own user+kernel CPU time, which contention perturbs far less
// than wall clock.
//   FieldBurn field <iters>   -> loop body is  o.f = o.f + i
//   FieldBurn ctl   <iters>   -> same loop, same op count, locals only
public class FieldBurn {
    static class Box { int f; }
    public static void main(String[] a) {
        String mode = a[0];
        int n = Integer.parseInt(a[1]);
        int sink = 0;
        if (mode.equals("field")) {
            Box o = new Box();
            for (int i = 0; i < n; i++) { o.f = o.f + i; }
            sink = o.f;
        } else {
            int v = 0;
            for (int i = 0; i < n; i++) { v = v + i; }
            sink = v;
        }
        if (sink == 42) System.out.println("x");
    }
}
