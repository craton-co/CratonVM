// Prices the four interpreted invoke kinds against a common control, plus the
// per-argument slope. Every arm is in one process; arms are run in both orders
// on alternating rounds and reported as min-of-N.
public class Dispatch {
    interface I { int f(int x); }
    static class A implements I { public int f(int x) { return x; } }
    static class B extends A { }            // inherits f

    static int  s0()                       { return 1; }
    static int  s1(int a)                  { return a; }
    static int  s4(int a,int b,int c,int d) { return a; }
    int         v1(int a)                  { return a; }
    private int p1(int a)                  { return a; }

    static int cNone(int n){int s=0;for(int i=0;i<n;i++) s+=i; return s;}
    static int cS0(int n){int s=0;for(int i=0;i<n;i++) s+=s0(); return s;}
    static int cS1(int n){int s=0;for(int i=0;i<n;i++) s+=s1(i); return s;}
    static int cS4(int n){int s=0;for(int i=0;i<n;i++) s+=s4(i,i,i,i); return s;}
    static int cV1(int n, Dispatch o){int s=0;for(int i=0;i<n;i++) s+=o.v1(i); return s;}
    static int cP1(int n, Dispatch o){int s=0;for(int i=0;i<n;i++) s+=o.p1(i); return s;}
    static int cI1(int n, I o){int s=0;for(int i=0;i<n;i++) s+=o.f(i); return s;}
    static int cIn(int n, I o){int s=0;for(int i=0;i<n;i++) s+=o.f(i); return s;} // receiver B: inherited impl

    public static void main(String[] a){
        int n=a.length>0?Integer.parseInt(a[0]):1500000;
        int rounds=a.length>1?Integer.parseInt(a[1]):9;
        Dispatch o=new Dispatch(); I ia=new A(); I ib=new B();
        String[] nm={"nocall","static0","static1","static4","virtual1","special1","iface1","ifaceInherited"};
        double[] m=new double[nm.length]; for(int i=0;i<m.length;i++) m[i]=1e18;
        int sink=0; long t;
        for(int r=0;r<rounds;r++){
            boolean fwd=(r&1)==0;
            for(int k=0;k<nm.length;k++){
                int j = fwd?k:nm.length-1-k;
                t=System.nanoTime();
                switch(j){
                    case 0: sink+=cNone(n); break;
                    case 1: sink+=cS0(n); break;
                    case 2: sink+=cS1(n); break;
                    case 3: sink+=cS4(n); break;
                    case 4: sink+=cV1(n,o); break;
                    case 5: sink+=cP1(n,o); break;
                    case 6: sink+=cI1(n,ia); break;
                    default: sink+=cIn(n,ib); break;
                }
                double d=(System.nanoTime()-t)/(double)n;
                if(d<m[j]) m[j]=d;
            }
        }
        for(int i=0;i<nm.length;i++)
            System.out.println(nm[i]+"\t"+m[i]+"\tdelta="+(m[i]-m[0]));
        System.out.println("per-extra-arg = "+((m[3]-m[1])/4));
        System.out.println("virtual-over-static = "+(m[4]-m[2]));
        System.out.println("iface-over-virtual  = "+(m[6]-m[4]));
        if(sink==42) System.out.println("x");
    }
}
