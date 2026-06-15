public class CanonProbe {
  Object[] arr = new Object[8];
  int ctr;
  static Object mk(int n){ return (n & 7) == 0 ? "z" : Integer.valueOf(n); }
  // field-post-increment as array index, value from a CALL (canonicalize
  // boundary mid-rotated-stack), then the stored ref is read back & used.
  void step(int n){
    arr[ctr++ & 7] = mk(n);            // aload arr; aload this; dup; getfield ctr; dup_x1; ...; putfield; invokestatic mk; aastore
    Object o = arr[(ctr-1) & 7];
    if (o instanceof String) ctr += ((String)o).length();   // getfield/virtual on the slot
  }
  public static void main(String[] a){
    CanonProbe p = new CanonProbe(); long s=0;
    for (int n=0; n<3_000_000; n++){ p.step(n); s += p.ctr; }
    System.out.println("ctr="+p.ctr+" s!=0="+(s!=0)+" CANON_OK");
  }
}
