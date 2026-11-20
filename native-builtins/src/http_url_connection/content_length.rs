/// Content length per the real `URLConnection.getContentLengthLong()` contract:
/// the `Content-Length` RESPONSE HEADER when present, else the buffered body
/// size (legacy behaviour, kept for chunked/close-delimited responses). The
/// distinction matters for HEAD responses, which advertise the entity size in
/// the header but carry NO body — reporting the (empty) body made Spring's
/// `AbstractFileResolvingResource.isReadable()/contentLength()` see 0 after a
/// HEAD 200 and call the resource empty/unreadable
/// (ResourceTests.remoteResourceExists: `exists()` true but `isReadable()`
/// false, `contentLength()` 0 instead of 6).
///
/// LAST duplicate wins: `sun.net.www.MessageHeader.findValue` iterates
/// BACKWARDS (`for (int i = nkeys; --i >= 0;)`), so on real JDK a later
/// duplicate header overrides an earlier one. MockWebServer actually emits
/// `Content-Length: 0` (its bodiless default) FOLLOWED BY the test's
/// `addHeader("Content-Length", "6")` on both HotSpot and CratonVM — HotSpot
/// reads 6, so must we.
fn content_length_of(headers: &[(String, String)], body_len: usize) -> i64 {
    let _ = body_len;
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .last()
        .and_then(|(_, v)| v.trim().parse::<i64>().ok())
        // NO header, no length: `getContentLength()` is `-1`, which is how a
        // caller learns the length is unknown and it must read to EOF. The
        // buffered body size used to stand in for it, so a chunked response
        // reported the length this VM happened to have received (5 for the
        // sweep's `abc`+`de`) and a `204 No Content` reported 0 where HotSpot
        // reports -1 — an answer indistinguishable from a real zero-length
        // entity.
        .unwrap_or(-1)
}

pub fn huc_get_content_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Int(n as i32)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, &mut this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len()) as i32
    })
    .unwrap_or(-1);
    Ok(Some(Value::Int(n)))
}

pub fn huc_get_content_length_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = obj_arg(args, 0)?;
    if let Some(url_str) = huc_real_object_url(ctx, &mut this) {
        if url_str.starts_with("http://") || url_str.starts_with("https://") {
            huc_real_perform(ctx, &mut this, &url_str)?;
            let n = content_length_of(&huc_real_headers(ctx, this), huc_real_body(ctx, this).len());
            return Ok(Some(Value::Long(n)));
        }
    }
    if !matches!(ctx.get_field(this, HUC_CONNECTED), Value::Int(1)) {
        ensure_connected(ctx, &mut this)?;
    }
    let n = with_state(ctx, this, |s| {
        content_length_of(&s.response_headers, s.response_body.len())
    })
    .unwrap_or(-1);
    Ok(Some(Value::Long(n)))
}
