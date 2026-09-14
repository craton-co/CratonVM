import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

// A meta-annotation carried by AcpTag. Spring's AnnotationTypeMappings walks
// exactly this edge (getDeclaredAnnotations() on the ANNOTATION TYPE) and NPEs
// when any returned element's annotationType() is null.
@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.ANNOTATION_TYPE)
public @interface AcpMeta {
    Class<?> owner() default void.class;
}
