public class StrInstBench {
  final String s;
  StrInstBench(String s){ this.s=s; }
  String build(){ StringBuilder b=new StringBuilder(); int n=s.length(); for(int i=0;i<n;i++){ char c=s.charAt(i); b.append(c=='.'?'/':c);} return b.toString(); }
  int countDots(){ int k=0,n=s.length(); for(int i=0;i<n;i++) if(s.charAt(i)=='.') k++; return k; }
  public static void main(String[] a){
    StrInstBench o=new StrInstBench("org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor");
    String r=null; int dots=0;
    for(int w=0;w<50000;w++){ r=o.build(); dots=o.countDots(); }
    System.out.println("build="+r);
    System.out.println("countDots="+dots);
  }
}
