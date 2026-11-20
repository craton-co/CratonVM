// Child-eligible (name starts with "AnnProbe", not "*Filtered*") — the filtering
// loader REDEFINES it, so it becomes the defining loader. Used to check that a
// resolvable Class-valued annotation member resolves through the declaring
// class's loader (Spring's MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader).
public class AnnProbeRefType {
}
