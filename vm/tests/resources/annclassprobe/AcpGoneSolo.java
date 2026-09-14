// A DIRECTLY APPLIED annotation whose own type is unresolvable at runtime, in a
// package this VM fabricates synthetic stand-ins for. Distinct from
// `AcpGoneTarget`, whose top-level annotation is a LOADABLE @Repeatable
// container and only its entries are unresolvable.
//
// `getDeclaredAnnotations()` must either omit it (HotSpot's `AnnotationParser`
// drops an annotation whose type will not resolve) or throw. What it must not
// do is hand back a `Proxy` over the fabricated stand-in: that object is not a
// `java.lang.annotation.Annotation`, and Byte Buddy casts every element of the
// array to one.
@org.jboss.acpprobe.AcpGoneEnterprise("z")
public class AcpGoneSolo {
}
