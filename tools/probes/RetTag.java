// Prices the `areturn` return-tag lookup. Both arms call a leaf with the SAME
// argument list and the same body; only the RETURN DESCRIPTOR differs, which is
// the string the pre-memo code scanned on every reference return.
public class RetTag {
    static Object shortRet(Object a) { return a; }
    static java.util.concurrent.ConcurrentHashMap<String, java.util.List<Integer>>
        longRet(java.util.concurrent.ConcurrentHashMap<String, java.util.List<Integer>> a) { return a; }
    static int intRet(Object a) { return 1; }   // ireturn: never scans

    static int cShort(int n, Object o){int s=0;for(int i=0;i<n;i++) s+=shortRet(o)==null?0:1; return s;}
    static int cLong(int n, java.util.concurrent.ConcurrentHashMap<String, java.util.List<Integer>> o){
        int s=0;for(int i=0;i<n;i++) s+=longRet(o)==null?0:1; return s;}
    static int cInt(int n, Object o){int s=0;for(int i=0;i<n;i++) s+=intRet(o); return s;}

    public static void main(String[] a){
        int n=a.length>0?Integer.parseInt(a[0]):1500000;
        int rounds=a.length>1?Integer.parseInt(a[1]):9;
        Object o=new Object();
        java.util.concurrent.ConcurrentHashMap<String, java.util.List<Integer>> m=new java.util.concurrent.ConcurrentHashMap<>();
        double ms=1e18, ml=1e18, mi=1e18; int sink=0; long t;
        for(int r=0;r<rounds;r++){
            if((r&1)==0){
                t=System.nanoTime(); sink+=cInt(n,o);   mi=Math.min(mi,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=cShort(n,o); ms=Math.min(ms,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=cLong(n,m);  ml=Math.min(ml,(System.nanoTime()-t)/(double)n);
            } else {
                t=System.nanoTime(); sink+=cLong(n,m);  ml=Math.min(ml,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=cShort(n,o); ms=Math.min(ms,(System.nanoTime()-t)/(double)n);
                t=System.nanoTime(); sink+=cInt(n,o);   mi=Math.min(mi,(System.nanoTime()-t)/(double)n);
            }
        }
        System.out.println("ireturn="+mi+"  areturn/short-desc="+ms+"  areturn/long-desc="+ml);
        System.out.println("long-minus-short (the descriptor scan) = "+(ml-ms));
        if(sink==42) System.out.println("x");
    }
}
