// Single-shape workloads for the array-side A/Bs, in the style of FieldBurn:
// almost every bytecode executed is the one under test, so the instrument can
// be the process's own wall/CPU time rather than a self-timed subtraction.
//
//   ArrBurn len    <iters>   -> loop body is  s += a.length          (arraylength)
//   ArrBurn aaload <iters>   -> loop body is  if (oa[i&m] != null) s++
//   ArrBurn iaload <iters>   -> the same shape over an int[]: the control for
//                               `aaload`, since it takes the quickened element
//                               path and never touches the autobox screen
//   ArrBurn ctl    <iters>   -> same loop, locals only
public class ArrBurn {
    public static void main(String[] g) {
        String mode = g[0];
        int n = Integer.parseInt(g[1]);
        int[] ia = new int[1024];
        Object[] oa = new Object[1024];
        for (int k = 0; k < 1024; k++) { ia[k] = k + 1; oa[k] = new Object(); }
        int m = 1023, s = 0;
        if (mode.equals("len")) {
            for (int i = 0; i < n; i++) { s += ia.length; }
        } else if (mode.equals("aaload")) {
            for (int i = 0; i < n; i++) { if (oa[i & m] != null) s++; }
        } else if (mode.equals("iaload")) {
            for (int i = 0; i < n; i++) { if (ia[i & m] != 0) s++; }
        } else {
            for (int i = 0; i < n; i++) { if ((i & m) != 0) s++; }
        }
        if (s == 42) System.out.println("x");
    }
}
