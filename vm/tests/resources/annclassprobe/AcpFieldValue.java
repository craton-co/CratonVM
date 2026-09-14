import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

// Byte Buddy's @FieldValue shape: a Class-valued element whose default is
// `void.class` and which use sites normally OMIT. Byte Buddy's
// TargetMethodAnnotationDrivenBinder$ParameterBinder$ForFieldBinding.bind
// unconditionally calls `declaringType(annotation).represents(void.class)`, so
// a null here NPEs before any user code runs.
@Retention(RetentionPolicy.RUNTIME)
@Target({ElementType.METHOD, ElementType.PARAMETER})
public @interface AcpFieldValue {
    String value();

    Class<?> declaringType() default void.class;
}
