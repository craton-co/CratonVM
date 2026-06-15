public class DupProbe {
  int i; long l; int[] ia = new int[4]; long[] la = new long[4]; Object ref = "x"; Object ref2;
  void inc(){ i += 7; }                 // dup; getfield; iadd; putfield
  void incLong(){ l += 3L; }            // dup; getfield(long); ladd; putfield  (cat-2)
  void arrInt(int k){ ia[k] += 5; }     // dup2(arr,idx); iaload; iadd; iastore
  void arrLong(int k){ la[k] += 2L; }   // dup2(arr,idx); laload; ladd; lastore (cat-2)
  Object chain(){ return ref2 = ref; }  // dup_x1 of ref
  public static void main(String[] a){
    DupProbe d = new DupProbe(); long s=0;
    for (int n=0; n<2_000_000; n++){      // hot → JIT compiles these methods
      d.inc(); d.incLong(); d.arrInt(n&3); d.arrLong(n&3); d.chain();
      s += d.i + d.l + d.ia[n&3] + d.la[n&3] + (d.ref2==d.ref?1:0);
    }
    System.out.println("i="+d.i+" l="+d.l+" ia0="+d.ia[0]+" la0="+d.la[0]+" chk="+(s!=0));
    System.out.println("DUP_OK");
  }
}
