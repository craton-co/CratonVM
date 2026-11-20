/// Root the object carried by `value`, if any, and return its handle.
///
/// Non-reference `Value`s need no root, so they yield `None` and are replayed
/// verbatim by [`antlr_read_rooted_value`].
#[inline]
fn antlr_root_value(scope: &mut NativeHandleScope<'_>, value: Value) -> Option<NativeHandle> {
    match value {
        Value::Object(Some(obj)) => Some(scope.root(obj)),
        _ => None,
    }
}

/// Re-read a `Value` rooted by [`antlr_root_value`] at its current address.
#[inline]
fn antlr_read_rooted_value(
    scope: &NativeHandleScope<'_>,
    value: Value,
    handle: Option<&NativeHandle>,
) -> Value {
    match handle {
        Some(handle) => Value::Object(Some(scope.get(handle))),
        None => value,
    }
}
