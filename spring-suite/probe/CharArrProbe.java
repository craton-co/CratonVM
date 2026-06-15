import java.lang.annotation.*;
public class CharArrProbe {
  @Retention(RetentionPolicy.RUNTIME) @interface A { char[] chars() default {'a','b'}; boolean[] flags() default {true,false}; }
  @A static class T {}
  public static void main(String[] x) throws Exception {
    A a = T.class.getAnnotation(A.class);
    char[] c = a.chars(); boolean[] f = a.flags();
    System.out.println("chars.class="+((Object)c).getClass().getName()+" len="+c.length+" v="+c[0]+c[1]);
    System.out.println("flags.class="+((Object)f).getClass().getName()+" len="+f.length);
    System.out.println("CHARARR_OK");
  }
}
