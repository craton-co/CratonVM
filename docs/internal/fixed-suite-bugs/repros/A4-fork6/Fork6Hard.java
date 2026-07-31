import java.util.concurrent.*;
// Aggressive A4 variant: deeper/wider recursion (more live subtasks held across
// join() in worker JIT frames), more reps, and intended to run under
// CRATONVM_DBG_GC_STRESS so young GC fires continuously DURING worker execution
// (not just at the per-rep System.gc()). Maximizes the window in which a worker
// holds a forked subtask reference in a JIT-compiled runWorker/doExec frame
// while a peer-triggered STW young sweep runs.
public class Fork6Hard {
    static final ForkJoinPool POOL = ForkJoinPool.commonPool();
    static volatile String[] SINK = new String[1];
    static volatile int ROOT_N = 0;
    static final class StrTask extends RecursiveTask<String> {
        final int lo, hi; StrTask(int lo,int hi){this.lo=lo;this.hi=hi;}
        protected String compute(){
            if(hi-lo<=1) return "["+lo+"]";
            int mid=(lo+hi)>>>1;
            StrTask left=new StrTask(lo,mid); left.fork();
            StrTask right=new StrTask(mid,hi); right.fork();
            String r=right.join(); String l=left.join();
            if(l==null||r==null) throw new IllegalStateException("nullchild["+lo+","+hi+")");
            String res=l+r; if(lo==0&&hi==ROOT_N) SINK[0]=res; return res;
        }
    }
    public static void main(String[] a){
        int N = a.length>0 ? Integer.parseInt(a[0]) : 256;
        int reps = a.length>1 ? Integer.parseInt(a[1]) : 400;
        ROOT_N=N; StringBuilder sb=new StringBuilder();
        for(int i=0;i<N;i++) sb.append("[").append(i).append("]");
        String want=sb.toString();
        for(int rep=0; rep<reps; rep++){
            SINK[0]=null; Object got;
            try { StrTask t=new StrTask(0,N); ForkJoinTask<String> f=POOL.submit(t);
                  System.gc(); got=f.get(); }
            catch(Throwable e){ System.out.println("rep="+rep+" THREW "+e); return; }
            if(!want.equals(got)){ System.out.println("rep="+rep+" FAIL got="+got
                +" sinkOk="+want.equals(SINK[0])); return; }
        }
        System.out.println("ALL-OK reps="+reps+" N="+N);
    }
}
