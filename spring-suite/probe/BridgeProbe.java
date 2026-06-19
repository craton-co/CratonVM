import java.lang.reflect.*;
import java.util.*;
import org.springframework.core.BridgeMethodResolver;
import org.springframework.util.ClassUtils;

// Directly drive the suspected family-5 path: BridgeMethodResolver.findBridgedMethod
// walks the super/interface hierarchy and calls type.getDeclaredMethod(name, params)
// via searchForMatch with NO null guard. If Class.getInterfaces()/getAllInterfacesForClass
// yields a null element under CratonVM, this NPEs ("getDeclaredMethod on null").
public class BridgeProbe {
  interface Cmp<T> { int c(T t); }
  static class IntCmp implements Cmp<Integer> { public int c(Integer i){ return i; } }
  interface Repo<T, ID> { T find(ID id); void save(T t); }
  static class StrRepo implements Repo<String, Long> { public String find(Long id){ return ""; } public void save(String s){} }
  static abstract class Base<T> { abstract T make(); T echo(T t){ return t; } }
  static class Concrete extends Base<String> { String make(){ return "x"; } }
  // multi-level generic + interface mix
  interface Svc<A> extends Cmp<A> { A get(); }
  static class StrSvc implements Svc<String> { public int c(String s){ return 0; } public String get(){ return ""; } }
  // generic implementing java.util generic interface (bridge over Object)
  static class MyComparator implements Comparator<String> { public int compare(String a, String b){ return 0; } }

  static void resolveBridges(Class<?> c){
    for (Method m : c.getDeclaredMethods()){
      if (m.isBridge()){
        try {
          Method b = BridgeMethodResolver.findBridgedMethod(m);
          System.out.println("BRIDGE "+c.getSimpleName()+"#"+m.getName()+sig(m)+" -> "+b.getName()+sig(b));
        } catch (Throwable e){
          System.out.println("BRIDGE "+c.getSimpleName()+"#"+m.getName()+sig(m)+" THREW "+e.getClass().getName()+": "+e.getMessage());
        }
      }
    }
  }
  static String sig(Method m){
    StringBuilder b=new StringBuilder("(");
    Class<?>[] ps=m.getParameterTypes();
    for (int i=0;i<ps.length;i++){ if(i>0)b.append(","); b.append(ps[i]==null?"<NULL>":ps[i].getSimpleName()); }
    return b.append(")").toString();
  }
  static void dumpInterfaces(Class<?> c){
    Class<?>[] di = c.getInterfaces();
    StringBuilder b=new StringBuilder();
    for (Class<?> i : di) b.append(i==null?"<NULL>":i.getName()).append(" ");
    System.out.println("getInterfaces "+c.getSimpleName()+" = ["+b.toString().trim()+"]");
    Class<?>[] ai = ClassUtils.getAllInterfacesForClass(c);
    StringBuilder b2=new StringBuilder();
    boolean hasNull=false;
    for (Class<?> i : ai){ if(i==null){hasNull=true; b2.append("<NULL> ");} else b2.append(i.getSimpleName()).append(" "); }
    System.out.println("getAllInterfaces "+c.getSimpleName()+" = ["+b2.toString().trim()+"]"+(hasNull?"  <<< NULL ELEMENT":""));
  }

  public static void main(String[] a){
    Class<?>[] cs = { IntCmp.class, StrRepo.class, Concrete.class, StrSvc.class, MyComparator.class };
    for (Class<?> c : cs){ dumpInterfaces(c); resolveBridges(c); }
    System.out.println("DONE-BRIDGE");
  }
}
