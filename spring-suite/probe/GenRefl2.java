import java.lang.reflect.*;
import java.util.*;
public class GenRefl2 {
  static class Box<T extends Number & Comparable<T>> { List<T> items; Map<String,T> map; }
  static class Pair<A,B> { A a; B b; }
  static class Use { Box<Integer> b; Pair<String,Long> p; }
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+" OK"); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a) throws Exception {
    // type-variable USE resolved via factory: Box<T>.items = List<T>, get T, call getBounds()
    Field items = Box.class.getDeclaredField("items");
    ParameterizedType lt = (ParameterizedType) items.getGenericType(); // List<T>
    Type arg = lt.getActualTypeArguments()[0];                          // T (TypeVariableImpl via factory)
    System.out.println("arg class = " + arg.getClass().getName());
    t("typevar-USE.getBounds", () -> { Type[] b = ((TypeVariable<?>)arg).getBounds(); System.out.println("  bounds="+Arrays.toString(b)); });
    Field map = Box.class.getDeclaredField("map");
    ParameterizedType mt = (ParameterizedType) map.getGenericType();    // Map<String,T>
    Type targ = mt.getActualTypeArguments()[1];                         // T
    t("map-typevar.getBounds", () -> { Type[] b=((TypeVariable<?>)targ).getBounds(); System.out.println("  bounds="+Arrays.toString(b)); });
    t("typevar.getGenericDeclaration", () -> System.out.println("  decl="+((TypeVariable<?>)arg).getGenericDeclaration()));
    System.out.println("DONE-GENREFL2");
  }
}
