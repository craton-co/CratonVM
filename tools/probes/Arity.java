// Prices an interpreted invoke and its per-argument increment.
public class Arity {
    static int l0()                                { return 1; }
    static int l1(int a)                           { return a; }
    static int l2(int a,int b)                     { return a; }
    static int l4(int a,int b,int c,int d)         { return a; }
    static int l6(int a,int b,int c,int d,int e,int f) { return a; }
    int v1(int a) { return a; }

    static int c0(int n){int s=0;for(int i=0;i<n;i++) s+=l0(); return s;}
    static int c1(int n){int s=0;for(int i=0;i<n;i++) s+=l1(i); return s;}
    static int c2(int n){int s=0;for(int i=0;i<n;i++) s+=l2(i,i); return s;}
    static int c4(int n){int s=0;for(int i=0;i<n;i++) s+=l4(i,i,i,i); return s;}
    static int c6(int n){int s=0;for(int i=0;i<n;i++) s+=l6(i,i,i,i,i,i); return s;}
    static int cv(int n, Arity o){int s=0;for(int i=0;i<n;i++) s+=o.v1(i); return s;}
    static int cn(int n){int s=0;for(int i=0;i<n;i++) s+=i; return s;}   // control, no call

    public static void main(String[] a){
        int n=a.length>0?Integer.parseInt(a[0]):2000000;
        int rounds=a.length>1?Integer.parseInt(a[1]):9;
        Arity o=new Arity();
        double[] m=new double[7]; for(int i=0;i<7;i++) m[i]=1e18;
        int sink=0; long t;
        for(int r=0;r<rounds;r++){
            boolean fwd=(r&1)==0;
            for(int k=0;k<7;k++){
                int j = fwd?k:6-k;
                t=System.nanoTime();
                switch(j){
                    case 0: sink+=cn(n); break;
                    case 1: sink+=c0(n); break;
                    case 2: sink+=c1(n); break;
                    case 3: sink+=c2(n); break;
                    case 4: sink+=c4(n); break;
                    case 5: sink+=c6(n); break;
                    default: sink+=cv(n,o); break;
                }
                double d=(System.nanoTime()-t)/(double)n;
                if(d<m[j]) m[j]=d;
            }
        }
        String[] nm={"nocall","static0","static1","static2","static4","static6","virtual1"};
        for(int i=0;i<7;i++) System.out.println(nm[i]+"\t"+m[i]+" ns/iter\tdelta="+(m[i]-m[0]));
        System.out.println("per-extra-arg (static4-static0)/4 = "+((m[4]-m[1])/4));
        System.out.println("per-extra-arg (static6-static0)/6 = "+((m[5]-m[1])/6));
        if(sink==42) System.out.println("x");
    }
}
