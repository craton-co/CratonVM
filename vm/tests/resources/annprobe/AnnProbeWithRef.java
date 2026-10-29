// Annotated with a child-eligible Class value: when loaded via the filtering
// loader, value().getClassLoader() must be that filtering loader (the member is
// resolved through the declaring class's loader, which redefines AnnProbeRefType).
@AnnProbeExampleAnnotation(AnnProbeRefType.class)
public class AnnProbeWithRef {
}
