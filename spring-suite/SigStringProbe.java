import java.lang.reflect.*;

/** Read the raw generic-signature string CratonVM stored on the Method object,
 *  to tell whether the truncation is in the stored signature or the real-JDK
 *  SignatureParser/Reifier running on CratonVM. */
public class SigStringProbe {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        Method m = null;
        for (Method x : c.getDeclaredMethods()) if (x.getName().equals("suspendFun2")) m = x;
        Field f = Method.class.getDeclaredField("signature");
        f.setAccessible(true);
        System.out.println("Method.signature = " + f.get(m));
        // expected (HotSpot/class file):
        // (Ljava/lang/String;Lkotlin/coroutines/Continuation<-Lorg/springframework/core/Producer<+Ljava/lang/Number;>;>;)Ljava/lang/Object;
    }
}
