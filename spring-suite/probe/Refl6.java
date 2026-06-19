import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

// bug-06 family 5: exhaustively diff every Class/Method/Field-returning reflection
// accessor over a wide matrix of receiver types under CratonVM vs HotSpot.
// The failing op is "X.getDeclaredMethod(...)" with X==null, so SOME Class-returning
// accessor yields null in CV where HotSpot returns a value. Print a stable signature
// for every candidate and diff the two VM outputs line-for-line.
public class Refl6 {
  // ---- receiver-type zoo ----
  static class SNested { void f(){} }
  class Inner { void f(){} }                       // non-static member
  interface INested { default void d(){} }
  static abstract class Abs {}
  enum EN { A, B { void x(){ } }; void x(){} }
  @Retention(RetentionPolicy.RUNTIME)
  @interface AN { String v() default "d"; Class<?> c() default Object.class; int[] arr() default {}; }
  record Rec(int a, String b) {}
  static class Gen<X extends Number> { X val; List<X> xs; void gm(X x, Map<String,X> m){} }
  @AN(v="hi", c=String.class, arr={1,2}) static class Annotated {}
  interface IFace { Object op(String s) throws java.io.IOException; }

  static void p(String k, Object v){ System.out.println(k+" = "+v); }
  static String cn(Class<?> c){ return c==null ? "<NULL>" : c.getName(); }
  static String cns(Class<?>[] cs){
    if (cs==null) return "<NULL>";
    StringBuilder b=new StringBuilder("[");
    for (int i=0;i<cs.length;i++){ if(i>0)b.append(","); b.append(cn(cs[i])); }
    return b.append("]").toString();
  }
  // run a thunk, print result or the exception class+message (stable across VMs)
  interface Thunk { Object get() throws Throwable; }
  static void t(String k, Thunk th){
    Object v;
    try { v = th.get(); }
    catch (Throwable e){ v = "THREW "+e.getClass().getName()+": "+e.getMessage(); }
    System.out.println(k+" = "+v);
  }

  // dump every Class-returning accessor for a class mirror
  static void dumpClass(String tag, Class<?> c){
    t(tag+".superclass", () -> cn(c.getSuperclass()));
    t(tag+".componentType", () -> cn(c.getComponentType()));
    t(tag+".declaringClass", () -> cn(c.getDeclaringClass()));
    t(tag+".enclosingClass", () -> cn(c.getEnclosingClass()));
    t(tag+".nestHost", () -> cn(c.getNestHost()));
    t(tag+".enclosingMethod", () -> { Method m=c.getEnclosingMethod(); return m==null?"<NULL>":cn(m.getDeclaringClass())+"#"+m.getName(); });
    t(tag+".enclosingCtor", () -> { Constructor<?> k=c.getEnclosingConstructor(); return k==null?"<NULL>":cn(k.getDeclaringClass()); });
    t(tag+".interfaces", () -> cns(c.getInterfaces()));
    t(tag+".declaredClasses", () -> cns(c.getDeclaredClasses()));
    t(tag+".nestMembers", () -> cns(c.getNestMembers()));
    t(tag+".isAnon/Local/Member/Synth", () -> c.isAnonymousClass()+"/"+c.isLocalClass()+"/"+c.isMemberClass()+"/"+c.isSynthetic());
    t(tag+".canonicalName", () -> c.getCanonicalName());
    t(tag+".simpleName", () -> c.getSimpleName());
    t(tag+".genericSuperclass", () -> { Type ty=c.getGenericSuperclass(); return ty==null?"<NULL>":ty.getTypeName(); });
  }

  public static void main(String[] a) throws Throwable {
    // local + anonymous classes (exercise EnclosingMethod attribute)
    class Local { void f(){} }
    Runnable anon = new Runnable(){ public void run(){} };
    Runnable lambda = () -> {};
    Comparator<String> mref = String::compareTo;

    dumpClass("SNested", SNested.class);
    dumpClass("Inner", Inner.class);
    dumpClass("INested", INested.class);
    dumpClass("Abs", Abs.class);
    dumpClass("EN", EN.class);
    dumpClass("EN.B", EN.B.getClass());
    dumpClass("AN", AN.class);
    dumpClass("Rec", Rec.class);
    dumpClass("Gen", Gen.class);
    dumpClass("Local", Local.class);
    dumpClass("anon", anon.getClass());
    dumpClass("lambda", lambda.getClass());
    dumpClass("mref", mref.getClass());
    dumpClass("int[][]", int[][].class);
    dumpClass("String[][]", String[][].class);
    dumpClass("Annotated", Annotated.class);

    // ---- Method accessors (return-type / param-type / declaring) ----
    Method snf = SNested.class.getDeclaredMethod("f");
    t("snf.returnType", () -> cn(snf.getReturnType()));
    t("snf.declaringClass", () -> cn(snf.getDeclaringClass()));
    Method gm = Gen.class.getDeclaredMethod("gm", Number.class, Map.class);
    t("gm.returnType", () -> cn(gm.getReturnType()));
    t("gm.paramTypes", () -> cns(gm.getParameterTypes()));
    t("gm.genericReturn", () -> gm.getGenericReturnType().getTypeName());
    Method op = IFace.class.getDeclaredMethod("op", String.class);
    t("op.returnType", () -> cn(op.getReturnType()));
    t("op.exceptionTypes", () -> cns(op.getExceptionTypes()));
    // the literal failing op shape: X.getDeclaredMethod off a reflected Class
    t("returnType.getDeclaredMethod", () -> op.getReturnType().getMethod("toString").getName());

    // ---- Field accessors ----
    Field val = Gen.class.getDeclaredField("val");
    t("val.type", () -> cn(val.getType()));
    t("val.declaringClass", () -> cn(val.getDeclaringClass()));
    Field xs = Gen.class.getDeclaredField("xs");
    t("xs.type", () -> cn(xs.getType()));

    // ---- Constructor / Parameter ----
    Constructor<?> rc = Rec.class.getDeclaredConstructor(int.class, String.class);
    t("rec.ctor.paramTypes", () -> cns(rc.getParameterTypes()));
    t("rec.ctor.params[0].type", () -> cn(rc.getParameters()[0].getType()));
    t("rec.components", () -> { RecordComponent[] rcs=Rec.class.getRecordComponents(); StringBuilder b=new StringBuilder(); for(RecordComponent r:rcs){b.append(cn(r.getType())).append("#").append(r.getName()).append(" ");} return b.toString(); });
    t("rec.component.accessor.returnType", () -> cn(Rec.class.getRecordComponents()[0].getAccessor().getReturnType()));

    // ---- Annotation reflection: annotationType() + attribute Class value ----
    AN an = Annotated.class.getAnnotation(AN.class);
    t("an.annotationType", () -> cn(an.annotationType()));
    t("an.c()", () -> cn(an.c()));
    t("an.annotationType.getDeclaredMethod(c)", () -> an.annotationType().getDeclaredMethod("c").getName());
    t("an.v()", () -> an.v());

    // ---- Proxy ----
    InvocationHandler h = (proxy, m, args) -> { if(m.getName().equals("toString")) return "P"; return null; };
    IFace px = (IFace) Proxy.newProxyInstance(Refl6.class.getClassLoader(), new Class<?>[]{IFace.class}, h);
    dumpClass("proxyClass", px.getClass());
    t("proxy.interfaces[0].getDeclaredMethod", () -> px.getClass().getInterfaces()[0].getDeclaredMethod("op", String.class).getName());

    // ---- forName variants ----
    t("forName(String,init,cl)", () -> cn(Class.forName("java.util.ArrayList", false, Refl6.class.getClassLoader())));
    t("getNestHost.getDeclaredMethod", () -> EN.B.getClass().getNestHost().getDeclaredMethod("main", String[].class).getName());

    System.out.println("DONE-REFL6");
  }
}
