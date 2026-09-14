import java.lang.annotation.ElementType;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

// A COMPILE-ONLY repeatable annotation: `AcpGone.class` is moved out of the
// probe directory after compilation, so at runtime the type is genuinely
// unresolvable — the `org.apiguardian.api.API` shape whose jar is absent from
// the runtime classpath. Its @Repeatable container (AcpGones) stays present, so
// `AcpGoneTarget.getDeclaredAnnotations()` yields a LOADABLE container whose
// value() array entries have an UNLOADABLE type.
@Repeatable(AcpGones.class)
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
public @interface AcpGone {
    String value();
}
