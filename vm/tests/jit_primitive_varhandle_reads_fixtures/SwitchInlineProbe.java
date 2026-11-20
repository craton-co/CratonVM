public class SwitchInlineProbe {
    static volatile int sink;
    static int calls;
    static int dense(int k, int v) {
        switch (k) {
            case -1: return v + 3;
            case 0: return v ^ 55;
            case 1: return v * 7;
            default: return v - 99;
        }
    }
    static int sparse(int k, int v) {
        switch (k) {
            case Integer.MIN_VALUE: return v + 11;
            case -1000: return v ^ 77;
            case 7: return v * 13;
            case Integer.MAX_VALUE: return v - 31;
            default: return v + 101;
        }
    }
    static int callDense(int k, int v) { return dense(k, v) + 1; }
    static int callSparse(int k, int v) { return sparse(k, v) - 1; }
    static int nested(int k, int v) { return callDense(k, callSparse(k, v)); }
    static int effect(int k, int v) { calls++; return dense(k, v); }
    static int callEffect(int k, int v) { return effect(k, v); }
    static int guarded(int k, int[] a) {
        switch (k) { case 0: return a[0]; case 1: return a[1]; default: return a[2]; }
    }
    static int callGuarded(int k, int[] a) { calls++; return guarded(k, a); }
    static String error(int k, int[] a) {
        try { callGuarded(k,a); return "NONE"; }
        catch (Throwable t) { return t.getClass().getName()+":"+t.getMessage(); }
    }
    public static void main(String[] args) {
        int[] keys = { Integer.MIN_VALUE, -1000, -2, -1, 0, 1, 7, 2, Integer.MAX_VALUE };
        int[] a = {19,23,29};
        int rounds = Integer.getInteger("switch.rounds", 20000);
        for (int n=0;n<rounds;n++) {
            int k=keys[n%keys.length];
            sink=callDense(k,n)+callSparse(k,n)+nested(k,n)+callEffect(k,n)+callGuarded(n%3,a);
        }
        System.gc(); calls=0;
        long sum=0;
        for (int n=0;n<1000;n++) {
            int k=keys[n%keys.length];
            sum+=callDense(k,n)+callSparse(k,n)+nested(k,n)+callEffect(k,n)+callGuarded(n%3,a);
        }
        String bounds=error(1,new int[1]);
        String nullCase=error(0,null);
        System.out.println("SWITCH-INLINE-OK sum="+sum+" calls="+calls+" bounds="+bounds+" null="+nullCase);
    }
}
