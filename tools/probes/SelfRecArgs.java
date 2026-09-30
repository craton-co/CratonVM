/**
 * The direct self-recursive call marshals the VM context pointer plus every
 * Java argument into an ENTRY-ABI REGISTER. That file is four registers on
 * Win64 (RCX, RDX, R8, R9) and six on SysV (RDI, RSI, RDX, RCX, R8, R9), so a
 * static self-recursive method can carry at most 3 arguments on Windows and 5
 * on Linux before `abi[1 + i]` runs off the end.
 *
 * Each arm is the same recursion at a different arity. The 3-arg arm is the
 * control: it fits everywhere, so an arm that dies while it lives names the
 * arity and not the recursion.
 */
public class SelfRecArgs {

    static long f1(int a)                                 { return a <= 0 ? 0 : f1(a - 1) + 1; }
    static long f2(int a, int b)                          { return a <= 0 ? b : f2(a - 1, b) + 1; }
    static long f3(int a, int b, int c)                   { return a <= 0 ? b + c : f3(a - 1, b, c) + 1; }
    static long f4(int a, int b, int c, int d)            { return a <= 0 ? b + c + d : f4(a - 1, b, c, d) + 1; }
    static long f5(int a, int b, int c, int d, int e)     { return a <= 0 ? b + c + d + e : f5(a - 1, b, c, d, e) + 1; }
    static long f6(int a, int b, int c, int d, int e, int g) {
        return a <= 0 ? b + c + d + e + g : f6(a - 1, b, c, d, e, g) + 1;
    }

    static int ITERS = Integer.getInteger("iters", 300000);

    public static void main(String[] args) {
        long s = 0;
        for (int i = 0; i < ITERS; i++) s += f1(20);
        System.out.println("CK selfrec args=1 ok=" + (s > 0));
        s = 0;
        for (int i = 0; i < ITERS; i++) s += f2(20, 1);
        System.out.println("CK selfrec args=2 ok=" + (s > 0));
        s = 0;
        for (int i = 0; i < ITERS; i++) s += f3(20, 1, 2);
        System.out.println("CK selfrec args=3 ok=" + (s > 0));
        s = 0;
        for (int i = 0; i < ITERS; i++) s += f4(20, 1, 2, 3);
        System.out.println("CK selfrec args=4 ok=" + (s > 0));
        s = 0;
        for (int i = 0; i < ITERS; i++) s += f5(20, 1, 2, 3, 4);
        System.out.println("CK selfrec args=5 ok=" + (s > 0));
        s = 0;
        for (int i = 0; i < ITERS; i++) s += f6(20, 1, 2, 3, 4, 5);
        System.out.println("CK selfrec args=6 ok=" + (s > 0));
        System.out.println("CK selfrec DONE");
    }
}
