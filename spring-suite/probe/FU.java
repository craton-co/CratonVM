import java.util.concurrent.atomic.*;
public class FU {
  volatile int iv; volatile long lv;
  static final AtomicIntegerFieldUpdater<FU> IV = AtomicIntegerFieldUpdater.newUpdater(FU.class, "iv");
  static final AtomicLongFieldUpdater<FU> LV = AtomicLongFieldUpdater.newUpdater(FU.class, "lv");
  static void t(String n, Runnable r){ try { r.run(); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    FU f = new FU();
    t("getAndIncrement", () -> System.out.println("int getAndIncrement: old="+IV.getAndIncrement(f)+" now="+f.iv));   // old 0, now 1
    t("getAndIncrement", () -> System.out.println("int getAndIncrement: old="+IV.getAndIncrement(f)+" now="+f.iv));   // old 1, now 2
    t("addAndGet", () -> System.out.println("int addAndGet(+5): new="+IV.addAndGet(f,5)+" iv="+f.iv));                 // 7
    t("getAndDecrement", () -> System.out.println("int getAndDecrement: old="+IV.getAndDecrement(f)+" now="+f.iv));   // old 7, now 6
    t("long getAndIncrement", () -> System.out.println("long getAndIncrement: old="+LV.getAndIncrement(f)+" now="+f.lv)); // old 0, now 1
    t("long addAndGet", () -> System.out.println("long addAndGet(+10): new="+LV.addAndGet(f,10)+" lv="+f.lv));         // 11
    System.out.println("DONE-FU");
  }
}
