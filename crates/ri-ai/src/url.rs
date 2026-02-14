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
            let (Some(hi), Some(lo)) = (bytes.next(), bytes.next()) else {
                buf.push(b'%');
                continue;
            };
            match u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                Ok(byte) => buf.push(byte),
                Err(_) => { buf.push(b'%'); buf.push(hi); buf.push(lo); }
            }
        } else if b == b'+' {
            buf.push(b' ');
        } else {
            buf.push(b);
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}
