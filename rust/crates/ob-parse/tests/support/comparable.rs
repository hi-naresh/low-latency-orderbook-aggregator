//! Which inputs a fast parser and its serde_json oracle must agree on. Dependency-free,
//! so the fuzz targets can include it too.

/// Inputs where the two implementations legitimately differ, by design:
/// backslash escapes (we don't decode them), invalid UTF-8 (we don't validate it), and
/// nesting near the depth limit (serde_json counts depth slightly differently).
pub(crate) fn comparable(msg: &[u8]) -> bool {
    if msg.contains(&b'\\') || std::str::from_utf8(msg).is_err() {
        return false;
    }
    let (mut depth, mut max) = (0_i32, 0_i32);
    for &b in msg {
        match b {
            b'[' | b'{' => {
                depth += 1;
                max = max.max(depth);
            }
            b']' | b'}' => depth -= 1,
            _ => {}
        }
    }
    max <= 100
}
