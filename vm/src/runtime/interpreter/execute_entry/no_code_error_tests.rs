use super::{no_code_error, NoCodeFlags};

/// An unbound native is HotSpot's `UnsatisfiedLinkError` with the method
/// spelled `'ret holder.name(params)'`; an abstract declaration keeps the
/// `AbstractMethodError` text it always had.
#[test]
fn an_unbound_native_is_an_unsatisfied_link_error() {
    let native = NoCodeFlags {
        is_native: true,
        is_static: true,
        is_synchronized: false,
    };
    assert_eq!(
        no_code_error(native, "p/Foo", "bar", "(ILjava/lang/String;)V"),
        (
            "java/lang/UnsatisfiedLinkError",
            "'void p.Foo.bar(int, java.lang.String)'".to_string()
        )
    );
    let abstract_method = NoCodeFlags {
        is_native: false,
        is_static: false,
        is_synchronized: false,
    };
    assert_eq!(
        no_code_error(abstract_method, "p/Foo", "bar", "()I"),
        (
            "java/lang/AbstractMethodError",
            "method p/Foo.bar()I has no Code attribute".to_string()
        )
    );
}
