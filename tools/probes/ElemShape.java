// Prices ONE interpreted array element access against a same-shape control
// whose body has the identical bytecode COUNT but touches only locals.
// control : s += i + i;              (iload,iload,iadd,iadd,istore-ish)
// arr     : s += a[i] + a[i];        (same, with two iaload)
// Two arms per round, both orders, min-of-N.
public class ElemShape {
    static int control(int n, int[] a) { int s=0; for (int i=0;i<n;i++) { s += i + i; } return s; }
    static int arr    (int n, int[] a) { int s=0; for (int i=0;i<n;i++) { s += a[i] + a[i]; } return s; }
    static int store  (int n, int[] a) { int s=0; for (int i=0;i<n;i++) { a[i] = i; a[i] = i; } return s; }
    static int fld    (int n, int[] a) { int s=0; for (int i=0;i<n;i++) { s += a.length + a.length; } return s; }
    public static void main(String[] g) {
        int n = g.length>0?Integer.parseInt(g[0]):1000000;
        int r = g.length>1?Integer.parseInt(g[1]):5;
        int[] a = new int[n];
        String[] nm = {"control","arr2load","arr2store","arrlen2"};
        double[] m = new double[nm.length];
        for (int i=0;i<m.length;i++) m[i]=1e18;
        int sink=0; long t;
        for (int p=0;p<r;p++) {
            boolean fwd = (p&1)==0;
            for (int k=0;k<nm.length;k++) {
                int j = fwd?k:nm.length-1-k;
                t=System.nanoTime();
                switch(j){case 0: sink+=control(n,a); break; case 1: sink+=arr(n,a); break;
                          case 2: sink+=store(n,a); break; default: sink+=fld(n,a); break;}
                double d=(System.nanoTime()-t)/(double)n; if(d<m[j]) m[j]=d;
            }
        }
        for (int i=0;i<nm.length;i++) System.out.println(nm[i]+"\t"+m[i]+"\tover_control="+(m[i]-m[0]));
        System.out.println("per iaload  = "+((m[1]-m[0])/2));
        System.out.println("per iastore = "+((m[2]-m[0])/2));
        System.out.println("per arraylength = "+((m[3]-m[0])/2));
        if (sink==42) System.out.println("x");
    }
}
