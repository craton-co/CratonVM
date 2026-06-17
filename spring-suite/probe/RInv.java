import java.lang.reflect.*;
import java.util.*;
public class RInv {
  static class IH { List<Integer> list; }
  static class WH { List<?> wild; }
  static Object rinvoke(Object recv, String m) throws Exception {
    Method meth = null;
    for (Class<?> c : new Class<?>[]{ParameterizedType.class, WildcardType.class, TypeVariable.class, GenericArrayType.class, Type.class})
      try { meth = c.getMethod(m); break; } catch(Exception e){}
    return meth.invoke(recv);  // REFLECTIVE invoke (what SerializableTypeWrapper does)
  }
  static void show(String n, Object o){ System.out.println(n+" -> class="+(o==null?"null":o.getClass().getName())+(o!=null&&o.getClass().isArray()?" len="+java.lang.reflect.Array.getLength(o):"")); }
  public static void main(String[] a) throws Exception {
    Type li = IH.class.getDeclaredField("list").getGenericType();  // List<Integer>
    show("DIRECT  List<Integer>.getActualTypeArguments", ((ParameterizedType)li).getActualTypeArguments());
    show("REFLECT List<Integer>.getActualTypeArguments", rinvoke(li, "getActualTypeArguments"));
    Type lw = WH.class.getDeclaredField("wild").getGenericType();  // List<?>
    Type wc = ((ParameterizedType)lw).getActualTypeArguments()[0]; // ?
    show("DIRECT  ?.getUpperBounds", ((WildcardType)wc).getUpperBounds());
    show("REFLECT ?.getUpperBounds", rinvoke(wc, "getUpperBounds"));
    show("REFLECT ?.getLowerBounds", rinvoke(wc, "getLowerBounds"));
    System.out.println("DONE-RINV");
  }
}
