// Single-shape workloads for the static-field A/B, in the style of FieldBurn.
//   StaticBurn get <iters>  -> loop body is  s += S            (getstatic)
//   StaticBurn put <iters>  -> loop body is  S = i             (putstatic)
//   StaticBurn ctl <iters>  -> same loop, locals only
// `S` is deliberately NOT a field of java/lang/System: the screen under test
// is the one every OTHER class's getstatic used to pay.
public class StaticBurn {
    static int S = 1;
    public static void main(String[] a) {
        String mode = a[0]; int n = Integer.parseInt(a[1]); int s = 0;
        if (mode.equals("get"))      { for (int i = 0; i < n; i++) { s += S; } }
        else if (mode.equals("put")) { for (int i = 0; i < n; i++) { S = i; } s = S; }
        else                         { int v = 1; for (int i = 0; i < n; i++) { s += v; } }
        if (s == 42) System.out.println("x");
    }
}
