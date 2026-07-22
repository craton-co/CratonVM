// Minimal B-J repro: ConcurrentLinkedDeque uses static-final VarHandles for
// lock-free linking. Hammer add/poll under GC pressure; if VarHandles get
// collected, "Stale pointer ... VarHandle.set" + corruption appears.
import java.util.concurrent.ConcurrentLinkedDeque;
public class CldGc {
  public static void main(String[] a){
    ConcurrentLinkedDeque<Object> dq = new ConcurrentLinkedDeque<>();
    long n=0;
    for(int round=0; round<200000; round++){
      // churn garbage to force GC between deque ops
      for(int i=0;i<200;i++){ Object j=new byte[128]; if(j.hashCode()==7) n++; }
      dq.add(new Object());     // linkLast -> VarHandle.set
      dq.add(new Object());
      if(dq.poll()!=null) n++;  // unlink -> VarHandle ops
      if(dq.size()>1000) dq.clear();
    }
    System.out.println("CldGc DONE n="+n+" size="+dq.size());
  }
}
