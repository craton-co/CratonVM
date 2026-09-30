import java.lang.invoke.MethodType;
public class MtShrink {
    public static void main(String[] a) {
        MethodType mt = MethodType.methodType(int.class, String.class, long.class);
        System.out.println("before           = " + mt);
        Class<?>[] arr = mt.parameterArray();
        arr[0] = int.class;                       // must not touch mt
        System.out.println("after write      = " + mt);
        System.out.println("paramType0       = " + mt.parameterType(0).getName());
        // and the interned SHARED instance every later caller gets
        System.out.println("fresh lookup     = " + MethodType.methodType(int.class, String.class, long.class));
        System.out.println("DONE");
    }
}
