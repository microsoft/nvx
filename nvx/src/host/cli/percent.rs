//! How the CLI percent-encodes the text that it puts in a URL: the path segments of its requests
//! to the GitHub REST API and the query values of its CPU profile request link.

/// The punctuation that RFC 3986 leaves unreserved, which a URL carries as it is, as it carries
/// ASCII letters and digits.
const UNRESERVED_PUNCTUATION: &[u8] = b"-._~";

/// Returns `text` percent-encoded for one path segment or query value of a URL: every byte but
/// ASCII letters, digits, and [`UNRESERVED_PUNCTUATION`] becomes `%XX`, in uppercase hexadecimal.
/// That is how `urllib.parse.quote(text, safe="")` encodes a path segment, and how OpenVMM's
/// fallback warning encodes the values of its CPU profile request link.
pub(super) fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED_PUNCTUATION.contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests;
