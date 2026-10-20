import java.io.InputStream;
import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.Supplier;

/**
 * gci i11/cl: the {@code -Xlog:class+load} lines of lambda proxies and of a
 * nestmate hidden class (gch-h4fl-xlog-class-load-residuals items 6 and 7).
 * Run it with {@code -Xlog:class+load=info:stderr} (or {@code =debug}); the
 * class lines go to stderr, stdout stays deterministic for the HotSpot diff.
 *
 * <p>Mode {@code lambda} spins three lambda classes, two of them for the
 * NESTED class {@code Inner} (an {@code invokedynamic} lambda, and one through
 * a direct {@code LambdaMetafactory.metafactory} call) and one for the top
 * class. HotSpot prints each as {@code <lookup>$$Lambda/0x.. source:
 * <lookup>}: twice {@code GciCl11LambdaLoadProbe$Inner}, once
 * {@code GciCl11LambdaLoadProbe} (item 7: the lookup class, not the nest
 * host {@code GciCl11LambdaLoadProbe}).
 *
 * <p>Mode {@code hidden} defines {@code GciCl11Payload}'s bytes as a NESTMATE
 * hidden class through {@code Inner}'s lookup; HotSpot prints
 * {@code GciCl11Payload/0x.. source: GciCl11LambdaLoadProbe$Inner}.
 *
 * <p>stdout (deterministic):
 * <pre>
 * lambda mode
 * inner indy lambda ran
 * inner metafactory lambda ran
 * top lambda ran
 * done
 * </pre>
 * or
 * <pre>
 * hidden mode
 * hidden payload says hello
 * hidden is hidden true
 * done
 * </pre>
 */
public class GciCl11LambdaLoadProbe {

    static final class Inner {
        static Runnable indy() {
            return () -> System.out.println("inner indy lambda ran");
        }

        static void target() {
            System.out.println("inner metafactory lambda ran");
        }

        static Runnable viaMetafactory() throws Throwable {
            MethodHandles.Lookup lookup = MethodHandles.lookup();
            MethodType voidType = MethodType.methodType(void.class);
            MethodHandle impl = lookup.findStatic(Inner.class, "target", voidType);
            CallSite site = LambdaMetafactory.metafactory(
                    lookup,
                    "run",
                    MethodType.methodType(Runnable.class),
                    voidType,
                    impl,
                    voidType);
            return (Runnable) site.getTarget().invoke();
        }

        static Class<?> defineHiddenPayload(byte[] bytes) throws Throwable {
            MethodHandles.Lookup hidden = MethodHandles.lookup()
                    .defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE);
            return hidden.lookupClass();
        }
    }

    public static void main(String[] args) throws Throwable {
        String mode = args.length > 0 ? args[0] : "lambda";
        if (mode.equals("hidden")) {
            System.out.println("hidden mode");
            byte[] bytes;
            try (InputStream in =
                    GciCl11LambdaLoadProbe.class.getResourceAsStream("GciCl11Payload.class")) {
                bytes = in.readAllBytes();
            }
            Class<?> hidden = Inner.defineHiddenPayload(bytes);
            Method hello = hidden.getDeclaredMethod("hello");
            System.out.println("hidden payload says " + hello.invoke(null));
            System.out.println("hidden is hidden " + hidden.isHidden());
        } else {
            System.out.println("lambda mode");
            Inner.indy().run();
            Inner.viaMetafactory().run();
            Supplier<String> top = () -> "top lambda ran";
            System.out.println(top.get());
        }
        System.out.println("done");
    }
}

/** No NestHost / NestMembers attribute: a top-level class of this file. */
class GciCl11Payload {
    public static String hello() {
        return "hello";
    }
}
