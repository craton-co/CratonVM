public class CSBench {
  static long sumStr(String s){ long h=0; int n=s.length(); for(int i=0;i<n;i++) h=h*31+s.charAt(i); return h; }
  static long sumCS(CharSequence s){ long h=0; int n=s.length(); for(int i=0;i<n;i++) h=h*31+s.charAt(i); return h; }
  public static void main(String[] a){
    String s="org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor";
    final int C=200000; long acc=0;
    for(int w=0;w<C;w++) acc+=sumStr(s);
    long t0=System.nanoTime(); for(int i=0;i<C;i++) acc+=sumStr(s); long t1=System.nanoTime();
    System.out.println("String-typed   charAt method = "+(t1-t0)/1_000_000+"ms");
    for(int w=0;w<C;w++) acc+=sumCS(s);
    long t2=System.nanoTime(); for(int i=0;i<C;i++) acc+=sumCS(s); long t3=System.nanoTime();
    System.out.println("CharSeq-typed  charAt method = "+(t3-t2)/1_000_000+"ms acc="+acc);
  }
}
