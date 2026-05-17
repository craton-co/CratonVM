package craton.gpu;

import java.lang.annotation.*;

/** Class-level marker that requests eager warmup compilation of the first N @GpuKernel methods at class load. */
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.TYPE)
public @interface EnableGpuAsync {
    int warmup() default 0;
}
