import java.time.*;
import java.util.*;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.LinkedBlockingDeque;

/** Sizes the "registered-native-that-always-yields" penalty across the whole
 *  real_protected_stub_class_common allow-list. Each arm is a cheap method on a
 *  real JDK class; a ~1-2us reading is the lost-inline-cache signature. */
public class ShadowProbe2 {
    static long sink; static Object osink; static boolean bsink;
    public static void main(String[] args){
        int n = args.length>0?Integer.parseInt(args[0]):200000;
        Instant inst = Instant.ofEpochSecond(1700000000L,123456789L);
        Instant inst2 = Instant.ofEpochSecond(1700000009L,1L);
        ReentrantLock lock = new ReentrantLock();
        AtomicBoolean ab = new AtomicBoolean(false);
        EnumSet<Thread.State> es = EnumSet.of(Thread.State.NEW, Thread.State.RUNNABLE);
        LinkedBlockingDeque<String> dq = new LinkedBlockingDeque<>(); dq.add("x");
        StringJoiner sj = new StringJoiner(",");
        Duration dur = Duration.ofSeconds(7,42);
        ArrayList<String> al = new ArrayList<>(); al.add("x");

        for (int pass=0; pass<2; pass++){
            System.out.println("--- pass "+pass+" ---");
            t("CONTROL ArrayList.size",        n, ()->{ for(int i=0;i<n;i++) sink+=al.size(); });
            t("CONTROL Duration.getSeconds",   n, ()->{ for(int i=0;i<n;i++) sink+=dur.getSeconds(); });
            t("Instant.getNano",               n, ()->{ for(int i=0;i<n;i++) sink+=inst.getNano(); });
            t("Instant.now [static]",          n, ()->{ for(int i=0;i<n;i++) osink=Instant.now(); });
            t("Instant.isBefore",              n, ()->{ for(int i=0;i<n;i++) bsink=inst.isBefore(inst2); });
            t("ReentrantLock.lock/unlock",     n, ()->{ for(int i=0;i<n;i++){ lock.lock(); lock.unlock(); } });
            t("ReentrantLock.tryLock",         n, ()->{ for(int i=0;i<n;i++){ if(lock.tryLock()) lock.unlock(); } });
            t("AtomicBoolean.get",             n, ()->{ for(int i=0;i<n;i++) bsink=ab.get(); });
            t("AtomicBoolean.compareAndSet",   n, ()->{ for(int i=0;i<n;i++) bsink=ab.compareAndSet(false,false); });
            t("EnumSet.contains",              n, ()->{ for(int i=0;i<n;i++) bsink=es.contains(Thread.State.NEW); });
            t("LinkedBlockingDeque.peek",      n, ()->{ for(int i=0;i<n;i++) osink=dq.peek(); });
            t("StringJoiner.length",           n, ()->{ for(int i=0;i<n;i++) sink+=sj.length(); });
        }
        System.out.println("sink="+sink+bsink+(osink!=null));
        Runtime.getRuntime().halt(0);
    }
    static void t(String name,int n,Runnable r){
        r.run();
        long t0=System.nanoTime(); r.run(); long d=System.nanoTime()-t0;
        System.out.printf("%-32s %9.1f ns/op%n",name,(double)d/n); System.out.flush();
    }
}
