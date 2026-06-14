public class AllocBench {
  public static void main(String[] a){
    String s="org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor";
    int N=200000;
    long t0=System.nanoTime(); int acc=0;
    for(int i=0;i<N;i++){ for(int j=0;j<s.length();j++) acc+=s.charAt(j); }   // no alloc
    long t1=System.nanoTime();
    System.out.println("charAt-loop noAlloc = "+(t1-t0)/1_000_000+"ms acc="+acc);
    long t2=System.nanoTime(); String r=null;
    for(int i=0;i<N;i++){ r=s.substring(0,10); }    // allocates a String each iter
    long t3=System.nanoTime();
    System.out.println("substring alloc x"+N+" = "+(t3-t2)/1_000_000+"ms");
  }
}
