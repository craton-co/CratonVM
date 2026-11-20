use super::proxy_non_public_refusal;

const PACKAGES: &str = "cannot have non-public interfaces in different packages";
const LOADER: &str = "non-public interface is not defined by the given loader";

fn e(
    pkg: &str,
    loader_tag: i32,
    same: Option<bool>,
) -> (String, (Option<String>, i32), Option<bool>) {
    (pkg.to_string(), (None, loader_tag), same)
}

#[test]
fn one_package_of_the_given_loader_is_accepted() {
    assert_eq!(proxy_non_public_refusal(&[]), None);
    assert_eq!(proxy_non_public_refusal(&[e("p", 2, Some(true))]), None);
    assert_eq!(
        proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 2, Some(true))]),
        None
    );
}

#[test]
fn two_packages_or_two_unnamed_modules_are_refused() {
    assert_eq!(
        proxy_non_public_refusal(&[e("p1", 57, Some(true)), e("p2", 57, Some(true))]),
        Some(PACKAGES)
    );
    // Same package name, two loaders: two unnamed modules.
    assert_eq!(
        proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 57, None)]),
        Some(PACKAGES)
    );
}

#[test]
fn a_foreign_defining_loader_is_refused_and_unknown_is_not() {
    assert_eq!(
        proxy_non_public_refusal(&[e("", 2, Some(false))]),
        Some(LOADER)
    );
    assert_eq!(proxy_non_public_refusal(&[e("", 2, None)]), None);
    assert_eq!(
        proxy_non_public_refusal(&[e("p", 2, Some(true)), e("p", 2, Some(false))]),
        Some(LOADER)
    );
}
