// Root-coverage row for docs/architecture/problems/03-jit-frame-gc-roots-are-heuristic.md.
// Four worker threads (exceptions through compiled frames, streams, a monitor,
// reflection, array copies, System.gc()) beside a long main loop that is
// entered by on-stack replacement. Run with CRATONVM_DBG_JIT_ROOTSCAN=1 and
// read the last [bandpath] and [jitroots] lines on stderr.
import java.util.*;
import java.util.concurrent.*;
import java.util.stream.*;
public class JitRootCoverageProbe {
    static final class Box { Object v; Box(Object v){this.v=v;} }
    static volatile Object sink;
    static int thrower(int i){ if((i&1023)==0) throw new IllegalStateException("x"+i); return i; }
    static long exc(int n){ long t=0; for(int i=1;i<n;i++){ try{ t+=thrower(i);}catch(IllegalStateException e){ t+=e.getMessage().length(); } } return t; }
    static long streams(int n){ return IntStream.range(0,n).mapToObj(i->new Box("s"+i)).filter(b->((String)b.v).length()>2).map(b->b.v.hashCode()).collect(Collectors.toList()).size(); }
    static synchronized long locked(List<Box> l,int n){ for(int i=0;i<n;i++){ l.add(new Box(new int[8])); if(l.size()>512) l.clear(); } return l.size(); }
    static long reflect(int n) throws Exception { var m=JitRootCoverageProbe.class.getDeclaredMethod("thrower",int.class); long t=0; for(int i=1;i<n;i++){ if((i&1023)!=0) t+=(Integer)m.invoke(null,i);} return t; }
    static long copy(int n){ Object[] a=new Object[64]; long t=0; for(int i=0;i<n;i++){ a[i&63]=new Box(i); Object[] b=Arrays.copyOf(a,64); System.arraycopy(a,0,b,0,64); t+=b.length; sink=b; } return t; }
    public static void main(String[] args) throws Exception {
        ExecutorService ex=Executors.newFixedThreadPool(4); List<Future<Long>> fs=new ArrayList<>();
        for(int w=0;w<4;w++){ final int id=w; fs.add(ex.submit(()->{ long t=0; List<Box> l=new ArrayList<>(); for(int r=0;r<60;r++){ t+=exc(20000)+streams(4000)+locked(l,4000)+reflect(4000)+copy(20000); if((r+id)%8==0) System.gc(); } return t; })); }
        long t=0; // long-running loop in main: on-stack replacement
        Object[] ring=new Object[1024]; for(int i=0;i<3_000_000;i++){ ring[i&1023]=new Box(new byte[32]); t+=i; }
        for(var f:fs) t+=f.get(); ex.shutdown(); System.out.println("done "+t);
    }
}
