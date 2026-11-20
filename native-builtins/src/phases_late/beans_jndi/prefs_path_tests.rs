use super::{p72_prefs_split_path, PREFS_MAX_NAME_LENGTH};

/// The two malformed shapes `AbstractPreferences` names, with its exact
/// messages. The text matters: it is what a caller catching
/// `IllegalArgumentException` reads, and it was measured against HotSpot 25
/// rather than invented.
#[test]
fn split_path_rejects_the_two_shapes_the_jdk_rejects() {
    assert_eq!(
        p72_prefs_split_path("a/").unwrap_err(),
        "Path ends with slash"
    );
    assert_eq!(
        p72_prefs_split_path("a//b").unwrap_err(),
        "Consecutive slashes in path"
    );
    // `node("//a")` reaches this function as "/a", because the caller has
    // already stripped ONE leading slash to resolve from the root. An
    // empty first segment is therefore a doubled slash, not a leading one.
    assert_eq!(
        p72_prefs_split_path("/a").unwrap_err(),
        "Consecutive slashes in path"
    );
}

/// A chain splits into its segments in order — this is the whole point of
/// the change: `node("x/y/z")` used to produce ONE node named `x/y/z`.
#[test]
fn split_path_yields_one_segment_per_name() {
    assert_eq!(p72_prefs_split_path("x/y/z").unwrap(), vec!["x", "y", "z"]);
    assert_eq!(p72_prefs_split_path("solo").unwrap(), vec!["solo"]);
}

/// 80 is the limit, not one past it. Asserting only the rejection would
/// pass against an off-by-one that rejects every legal 80-char name.
#[test]
fn split_path_bounds_the_name_length_at_the_jdk_limit() {
    let at_limit = "z".repeat(PREFS_MAX_NAME_LENGTH);
    assert!(
        p72_prefs_split_path(&at_limit).is_ok(),
        "a name of exactly MAX_NAME_LENGTH is legal"
    );
    let over = "z".repeat(PREFS_MAX_NAME_LENGTH + 1);
    let message = p72_prefs_split_path(&over).unwrap_err();
    assert!(
        message.ends_with(" too long"),
        "message names the offending node: {message}"
    );
}
