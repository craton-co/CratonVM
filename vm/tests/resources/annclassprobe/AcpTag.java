import java.lang.annotation.ElementType;
import java.lang.annotation.Repeatable;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

@AcpMeta(owner = AcpRef.class)
@Repeatable(AcpTags.class)
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.TYPE, ElementType.METHOD})
public @interface AcpTag {
    String name();

    // Class-valued element with a NON-default value at every use site.
    Class<?> type() default void.class;
}
