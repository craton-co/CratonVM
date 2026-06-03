public class OsrProbe {
    static void vadd(int[] a, int[] b, int[] c, int n) { for (int i=0;i<n;i++) c[i]=a[i]+b[i]; }
    // 3 inline loops, NO internal call
    static long threeLoopsNoCall(int[] a, int[] b, int[] c, int n) {
        for (int i=0;i<n;i++){ a[i]=i; b[i]=2*i; }
        for (int i=0;i<n;i++) c[i]=a[i]+b[i];
        long s=0; for (int i=0;i<n;i++) s+=c[i]; return s;
    }
    // fill loop + CALL + sum loop (mirrors BenchSuite.vectorAdd)
    static long twoLoopsWithCall(int[] a, int[] b, int[] c, int n) {
        for (int i=0;i<n;i++){ a[i]=i; b[i]=2*i; }
        vadd(a,b,c,n);
        long s=0; for (int i=0;i<n;i++) s+=c[i]; return s;
    }
    public static void main(String[] args) {
        String v = args[0]; int n = 1<<Integer.parseInt(args[1]);
        int[] a=new int[n], b=new int[n], c=new int[n];
        long t0=System.currentTimeMillis(); long s;
        if (v.equals("nocall")) s=threeLoopsNoCall(a,b,c,n); else s=twoLoopsWithCall(a,b,c,n);
        System.out.println("RESULT variant="+v+" n="+n+" ms="+(System.currentTimeMillis()-t0)+" checksum="+s);
    }
}
