public class InstBench {
  final String s;
  InstBench(String s){ this.s = s; }
  long sum(){ long h=0; int n=s.length(); for(int i=0;i<n;i++) h=h*31+s.charAt(i); return h; }
  public static void main(String[] a){
    InstBench b = new InstBench("org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor");
    final int C=200000; long acc=0;
    for(int w=0;w<C;w++) acc+=b.sum();              // warmup
    long t0=System.nanoTime();
    for(int i=0;i<C;i++) acc+=b.sum();
    long t1=System.nanoTime();
    System.out.println("instance charAt method x"+C+" = "+(t1-t0)/1_000_000+"ms acc="+acc);
  }
}
