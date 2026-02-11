// Percent-encoding utilities for OAuth URLs.

pub fn encode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{:02X}", b),
    }).collect()
}

pub fn decode(s: &str) -> String {
    let mut buf = Vec::new();
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().unwrap_or(b'0');
            let lo = bytes.next().unwrap_or(b'0');
            if let Ok(byte) = u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                buf.push(byte);
            }
        } else if b == b'+' {
            buf.push(b' ');
        } else {
            buf.push(b);
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}
