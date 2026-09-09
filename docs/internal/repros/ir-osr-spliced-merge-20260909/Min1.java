public class Min1 {
    static int clamp(int x, int lo, int hi) {
        int r;
        if (x < lo) r = lo; else if (x > hi) r = hi; else r = x;
        return r;
    }
    static int norm(int x) {
        int a = x < 0 ? -x : x;
        return (a * 2654435761L) > 0 ? a : a + 1;
    }
    static long runClamp(int n, int seed){ long s=0; int x=seed;
        for(int i=0;i<n;i++){ x=x*1103515245+12345; s+=clamp(x>>>20,100,900);} return s; }
    static long runNorm(int n, int seed){ long s=0; int x=seed;
        for(int i=0;i<n;i++){ x=x*1103515245+12345; s+=norm(x>>>8);} return s; }
    public static void main(String[] a){
        int reps=Integer.parseInt(a[0]), n=Integer.parseInt(a[1]);
        long c=0,m=0;
        for(int r=0;r<reps;r++) c+=runClamp(n,r);
        for(int r=0;r<reps;r++) m+=runNorm(n,r);
        System.out.println("clamp=["+c+"] norm=["+m+"]");
    }
}
