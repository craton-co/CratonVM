public class OsrProbe2 {
    static int intMultiLoop(int[] a, int[] b, int[] c, int n) {
        for (int i=0;i<n;i++){ a[i]=i; b[i]=2*i; }
        for (int i=0;i<n;i++) c[i]=a[i]+b[i];
        int s=0; for (int i=0;i<n;i++) s+=c[i];
        return s;
    }
    static long longMultiLoop(int[] a, int[] b, int[] c, int n) {
        for (int i=0;i<n;i++){ a[i]=i; b[i]=2*i; }
        for (int i=0;i<n;i++) c[i]=a[i]+b[i];
        long s=0; for (int i=0;i<n;i++) s+=c[i];
        return s;
    }
    public static void main(String[] a0) {
        String v=a0[0]; int n=1<<Integer.parseInt(a0[1]);
        int[] a=new int[n],b=new int[n],c=new int[n];
        long t0=System.currentTimeMillis(); long s;
        if (v.equals("int")) s=intMultiLoop(a,b,c,n); else s=longMultiLoop(a,b,c,n);
        System.out.println("RESULT variant="+v+" n="+n+" ms="+(System.currentTimeMillis()-t0)+" checksum="+s);
    }
}
