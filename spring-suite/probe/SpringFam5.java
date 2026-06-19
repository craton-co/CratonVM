import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;
import org.springframework.core.ResolvableType;
import org.springframework.core.GenericTypeResolver;
import org.springframework.core.MethodParameter;
import org.springframework.core.annotation.*;
import org.springframework.beans.BeanUtils;

// Drive Spring's real generics + annotation-synthesis machinery over fixtures that
// mirror the bug-05/family-5 failing test classes. Any CV-unique exception (esp.
// "getDeclaredMethod on null" or the generics CCE) reproduces the bug. Run under
// both VMs and diff.
public class SpringFam5 {
  static void t(String k, Runnable r){
    try { r.run(); System.out.println(k+" = OK"); }
    catch (Throwable e){ System.out.println(k+" = THREW "+e.getClass().getName()+": "+e.getMessage()); }
  }
  static void v(String k, java.util.concurrent.Callable<?> c){
    try { System.out.println(k+" = "+c.call()); }
    catch (Throwable e){ System.out.println(k+" = THREW "+e.getClass().getName()+": "+e.getMessage()); }
  }

  // ---- generics fixtures (bug-05 family shape) ----
  static class GenericBaseModel<T> {
    T id; List<T> items;
    public T getId(){ return id; } public void setId(T t){ this.id = t; }
    public List<T> getItems(){ return items; } public void setItems(List<T> i){ this.items = i; }
  }
  static class User extends GenericBaseModel<Integer> {}
  static class Holder { List<? extends Number> wild; Map<String,List<Integer>> nested; List<String>[] garr; }

  // ---- annotation synthesis fixtures (@AliasFor) ----
  @Retention(RetentionPolicy.RUNTIME) @interface Base {
    @AliasFor("name") String value() default "";
    @AliasFor("value") String name() default "";
    Class<?> type() default Object.class;
  }
  @Retention(RetentionPolicy.RUNTIME) @Base @interface Composed {
    @AliasFor(annotation = Base.class, attribute = "name") String alias() default "";
  }
  @Composed(alias = "hello") static class AnnTarget {}
  @Base(value = "direct", type = String.class) static class DirectTarget {}

  // ---- event-listener-style generic method (ApplicationListenerMethodAdapter family) ----
  interface Event<T> {}
  static class StringEvent implements Event<String> {}
  static class Listener { public void onEvent(Event<String> e){} }

  public static void main(String[] a) throws Throwable {
    // A) generic hierarchy resolution
    v("GTR.resolveTypeArgument(User,GenericBaseModel)",
      () -> { Class<?> c = GenericTypeResolver.resolveTypeArgument(User.class, GenericBaseModel.class); return c==null?"<NULL>":c.getName(); });
    v("RT.forClass(User).as(Base).getGeneric(0).resolve()",
      () -> { Class<?> c = ResolvableType.forClass(User.class).as(GenericBaseModel.class).getGeneric(0).resolve(); return c==null?"<NULL>":c.getName(); });
    v("RT.forField(items on User).resolveGeneric(0)",
      () -> { Field f = GenericBaseModel.class.getDeclaredField("items"); Class<?> c = ResolvableType.forField(f, User.class).resolveGeneric(0); return c==null?"<NULL>":c.getName(); });
    v("RT.forMethodReturnType(getId on User)",
      () -> { Method m = GenericBaseModel.class.getDeclaredMethod("getId"); return ResolvableType.forMethodReturnType(m, User.class).resolve(); });
    Field wild = Holder.class.getDeclaredField("wild");
    Field nested = Holder.class.getDeclaredField("nested");
    Field garr = Holder.class.getDeclaredField("garr");
    v("RT.forField(wild)", () -> ResolvableType.forField(wild));
    v("RT.forField(nested).getGeneric(1).getGeneric(0).resolve()",
      () -> { Class<?> c = ResolvableType.forField(nested).getGeneric(1).getGeneric(0).resolve(); return c==null?"<NULL>":c.getName(); });
    v("RT.forField(garr).isArray", () -> ResolvableType.forField(garr).isArray());

    // B) BeanUtils.copyProperties through a generic hierarchy (bug-05 deterministic repro)
    t("BeanUtils.copyProperties(User->User)", () -> {
      User src = new User(); src.setId(7); src.setItems(Arrays.asList(1,2,3));
      User dst = new User(); BeanUtils.copyProperties(src, dst);
    });

    // C) annotation synthesis: @AliasFor mirror + meta-annotation
    v("AnnotationUtils.findAnnotation(DirectTarget,Base).value",
      () -> { Base b = AnnotationUtils.findAnnotation(DirectTarget.class, Base.class); return b==null?"<NULL>":b.value()+"/"+b.name()+"/"+b.type().getName(); });
    v("AnnotatedElementUtils.findMergedAnnotation(AnnTarget,Base).name",
      () -> { Base b = AnnotatedElementUtils.findMergedAnnotation(AnnTarget.class, Base.class); return b==null?"<NULL>":(b.value()+"/"+b.name()); });
    v("MergedAnnotations.from(AnnTarget).get(Composed).synthesize.alias",
      () -> { Composed c = MergedAnnotations.from(AnnTarget.class).get(Composed.class).synthesize(); return c.alias(); });
    v("MergedAnnotations.from(DirectTarget).get(Base).getClass(type)",
      () -> { MergedAnnotation<Base> ma = MergedAnnotations.from(DirectTarget.class).get(Base.class); return ma.getClass("type").getName(); });

    // D) event-listener-style generic resolution + method-parameter
    v("RT.forMethodParameter(onEvent,0).getGeneric(0).resolve()",
      () -> { Method m = Listener.class.getDeclaredMethod("onEvent", Event.class); MethodParameter mp = new MethodParameter(m, 0); Class<?> c = ResolvableType.forMethodParameter(mp).getGeneric(0).resolve(); return c==null?"<NULL>":c.getName(); });
    v("RT.forClass(StringEvent).as(Event).getGeneric(0).resolve()",
      () -> { Class<?> c = ResolvableType.forClass(StringEvent.class).as(Event.class).getGeneric(0).resolve(); return c==null?"<NULL>":c.getName(); });

    // E) reflection on a lambda/method-ref via Spring (lambda metadata path)
    Runnable lambda = () -> {};
    Comparator<String> mref = String::compareTo;
    v("RT.forClass(lambda.class).resolve", () -> ResolvableType.forClass(lambda.getClass()).resolve());
    v("AnnotationUtils.findAnnotation(lambda.class,Deprecated)",
      () -> AnnotationUtils.findAnnotation(lambda.getClass(), Deprecated.class));
    v("AnnotatedElementUtils.findMergedAnnotation(mref.class,Base)",
      () -> AnnotatedElementUtils.findMergedAnnotation(mref.getClass(), Base.class));

    System.out.println("DONE-FAM5");
  }
}
