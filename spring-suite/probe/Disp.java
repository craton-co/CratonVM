import java.net.http.HttpClient;
public class Disp {
  // enum with constant-specific method bodies (like RfcUriParser$State) -> each constant is an anon subclass
  enum Op { ADD { int apply(int a,int b){return a+b;} }, MUL { int apply(int a,int b){return a*b;} }; abstract int apply(int a,int b); }
  // abstract class with concrete subclass override
  static abstract class Base { abstract String who(); }
  static class Sub extends Base { String who(){ return "Sub"; } }
  static void t(String n, java.util.concurrent.Callable<?> c){ try { System.out.println(n+" = "+c.call()); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    t("enum ADD.apply(3,4)", () -> Op.ADD.apply(3,4));   // 7
    t("enum MUL.apply(3,4)", () -> Op.MUL.apply(3,4));   // 12
    t("abstract Base->Sub.who()", () -> ((Base)new Sub()).who());  // Sub
    t("HttpClient.newHttpClient().executor()", () -> HttpClient.newHttpClient().executor().isPresent());
  }
}
