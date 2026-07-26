import java.util.concurrent.*;
public class Fork6 {
    static final ForkJoinPool POOL = ForkJoinPool.commonPool();
    static volatile String[] SINK = new String[1];
    static volatile int ROOT_N = 0;
    static final class StrTask extends RecursiveTask<String> {
        final int lo, hi; StrTask(int lo,int hi){this.lo=lo;this.hi=hi;}
        protected String compute(){
            if(hi-lo<=1) return "["+lo+"]";
            int mid=(lo+hi)>>>1; StrTask left=new StrTask(lo,mid); left.fork();
            String r=new StrTask(mid,hi).compute(); String l=left.join();
            if(l==null||r==null) throw new IllegalStateException("nullchild["+lo+","+hi+")");
            String res=l+r; if(lo==0&&hi==ROOT_N) SINK[0]=res; return res;
        }
    }
    public static void main(String[] a){
        int N=64; ROOT_N=N; StringBuilder sb=new StringBuilder();
        for(int i=0;i<N;i++) sb.append("[").append(i).append("]");
        String want=sb.toString();
        for(int rep=0; rep<200; rep++){
            SINK[0]=null; Object got;
            try { StrTask t=new StrTask(0,N); ForkJoinTask<String> f=POOL.submit(t);
                  System.gc(); got=f.get(); }
            catch(Throwable e){ System.out.println("rep="+rep+" THREW "+e); return; }
            if(!want.equals(got)){ System.out.println("rep="+rep+" FAIL got="+got
                +" sinkOk="+want.equals(SINK[0])); return; }
        }
        System.out.println("ALL-OK");
    }
}
