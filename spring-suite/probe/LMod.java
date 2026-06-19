import java.util.*;
public class LMod {
  interface F { void r(); }
  static String s(String x){ return x==null?"<null>":x; }
  public static void main(String[] a){
    Runnable lambda = () -> {};
    Comparator<String> mref = String::compareTo;
    F custom = () -> {};
    for (Object o : new Object[]{lambda, mref, custom}) {
      Class<?> c = o.getClass();
      System.out.printf("%s : mods=0x%x synth=%b hidden=%b nestHost=%s name=%s simple=%s canon=%s%n",
        o==lambda?"lambda":o==mref?"mref":"custom",
        c.getModifiers(), c.isSynthetic(), c.isHidden(), c.getNestHost().getName(),
        c.getName(), c.getSimpleName(), s(c.getCanonicalName()));
    }
  }
}
