import java.lang.reflect.*;
import java.util.*;
import java.util.concurrent.Callable;
public class GenRefl<T extends Number & Comparable<T>> {
  Map<String,Integer> m;
  List<String> ls;
  Map<String, List<Integer>> nested;
  List<? extends Number> wild;
  T tvar;
  List<String>[] garr;
  static class Base<X> {}
  static class Sub extends Base<String> implements Comparable<Sub> { public int compareTo(Sub o){return 0;} }
  static void t(String n, Callable<?> c){ try { System.out.println(n+" -> "+c.call()); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a) throws Exception {
    Field m=GenRefl.class.getDeclaredField("m"), nested=GenRefl.class.getDeclaredField("nested"),
          wild=GenRefl.class.getDeclaredField("wild"), garr=GenRefl.class.getDeclaredField("garr");
    t("Field.getGenericType(m)", () -> m.getGenericType());
    t("ParamType.getActualTypeArguments(m)", () -> Arrays.toString(((ParameterizedType)m.getGenericType()).getActualTypeArguments()));
    t("nested actualTypeArgs", () -> Arrays.toString(((ParameterizedType)nested.getGenericType()).getActualTypeArguments()));
    t("nested.inner actualTypeArgs", () -> { ParameterizedType p=(ParameterizedType)nested.getGenericType(); ParameterizedType inner=(ParameterizedType)p.getActualTypeArguments()[1]; return Arrays.toString(inner.getActualTypeArguments()); });
    t("Wildcard.getUpperBounds", () -> { ParameterizedType p=(ParameterizedType)wild.getGenericType(); WildcardType w=(WildcardType)p.getActualTypeArguments()[0]; return Arrays.toString(w.getUpperBounds()); });
    t("GenericArrayType comp", () -> ((GenericArrayType)garr.getGenericType()).getGenericComponentType());
    t("Class.getGenericInterfaces(Sub)", () -> Arrays.toString(Sub.class.getGenericInterfaces()));
    t("Class.getGenericSuperclass(Sub)", () -> Sub.class.getGenericSuperclass());
    t("Super actualTypeArgs(Sub)", () -> Arrays.toString(((ParameterizedType)Sub.class.getGenericSuperclass()).getActualTypeArguments()));
    t("TypeVariable.getBounds(T)", () -> { TypeVariable<?> tv=GenRefl.class.getTypeParameters()[0]; return Arrays.toString(tv.getBounds()); });
    Method mm=GenRefl.class.getDeclaredMethod("t", String.class, Callable.class);
    t("Method.getGenericParameterTypes", () -> Arrays.toString(mm.getGenericParameterTypes()));
    System.out.println("DONE-GENREFL");
  }
}
