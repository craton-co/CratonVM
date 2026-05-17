package craton.gpu;

/** Loosens specific analyzer rejections so otherwise-ineligible methods can be offloaded. */
public enum AdmissionHint {
    STRICT,
    ALLOW_ALLOCATION,
    ALLOW_DIV_BY_ZERO,
    ALLOW_INTRINSIC_CALLS
}
