import java.util.concurrent.atomic.AtomicStampedReference;
public class AsrTest {
  static void c(boolean ok,String l){System.out.println((ok?"OK   ":"FAIL ")+l);}
  public static void main(String[] a){
    String r0="r0",r1="r1";
    AtomicStampedReference<String> asr=new AtomicStampedReference<>(r0,7);
    c(asr.getReference()==r0,"getReference"); c(asr.getStamp()==7,"getStamp");
    int[] h=new int[1]; c(asr.get(h)==r0&&h[0]==7,"get(int[])");
    c(asr.compareAndSet(r0,r1,7,8),"CAS ok"); c(asr.getReference()==r1&&asr.getStamp()==8,"after CAS");
    c(!asr.compareAndSet(r0,"r2",8,9),"CAS wrong-ref fails"); c(asr.getStamp()==8,"stamp kept");
    asr.set(r0,5); c(asr.getReference()==r0&&asr.getStamp()==5,"set");
    c(asr.attemptStamp(r0,11),"attemptStamp"); c(asr.getStamp()==11,"attemptStamp stamp");
  }
}
