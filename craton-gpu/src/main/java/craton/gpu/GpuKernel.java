package craton.gpu;

import java.lang.annotation.*;

/** Marks a static method as a candidate for GPU offload and configures its launch shape and admission policy. */
@Retention(RetentionPolicy.CLASS)
@Target(ElementType.METHOD)
public @interface GpuKernel {
    GridShape grid()        default GridShape.ELEMENTWISE;
    int       blockX()      default 0;
    int       blockY()      default 0;
    int       blockZ()      default 0;
    int       sharedBytes() default 0;
    AdmissionHint admit()   default AdmissionHint.STRICT;
}
