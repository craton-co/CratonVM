package craton.gpu;

import java.lang.annotation.*;

/** Forces the analyzer to blacklist a method from GPU offload, overriding any GpuKernel annotation. */
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface GpuExclude {
    String reason() default "";
}
