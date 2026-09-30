import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

// The @Repeatable container. JUnit's AnnotationUtils.findRepeatableAnnotations
// walks this array and calls annotationType() on every entry.
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.TYPE, ElementType.METHOD})
public @interface AcpTags {
    AcpTag[] value();
}
