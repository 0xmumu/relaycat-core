pub fn build_ws_url(relay_url: &str, room_id: &str, role: &str) -> String {
    let (without_fragment, fragment) = relay_url
        .split_once('#')
        .map_or((relay_url, ""), |(url, fragment)| (url, fragment));
    let (path_part, query) = without_fragment
        .split_once('?')
        .map_or((without_fragment, ""), |(path, query)| (path, query));
    let mut path = path_part.trim_end_matches('/').to_string();
    if !path.ends_with("/ws") {
        path.push_str("/ws");
    }

    // Encode the values we append: today's room ids are base64url and roles
    // are fixed strings, but building the query by concatenation meant any
    // future value containing `&`, `=` or a space would silently add or
    // rewrite parameters.
    let room_id = encode_query_value(room_id);
    let role = encode_query_value(role);
    let mut url = if query.is_empty() {
        format!("{path}?room_id={room_id}&role={role}")
    } else {
        format!("{path}?{query}&room_id={room_id}&role={role}")
    };
    if !fragment.is_empty() {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_ws_path_when_relay_url_is_host_only() {
        assert_eq!(
            build_ws_url("ws://127.0.0.1:8787", "room-1", "app"),
            "ws://127.0.0.1:8787/ws?room_id=room-1&role=app"
        );
    }

    #[test]
    fn preserves_existing_ws_path() {
        assert_eq!(
            build_ws_url("ws://127.0.0.1:8787/ws", "room-1", "cli"),
            "ws://127.0.0.1:8787/ws?room_id=room-1&role=cli"
        );
    }

    #[test]
    fn preserves_existing_query_parameters() {
        assert_eq!(
            build_ws_url("wss://relay.example.com/ws?token=abc", "room-1", "cli"),
            "wss://relay.example.com/ws?token=abc&room_id=room-1&role=cli"
        );
    }

    #[test]
    fn appends_ws_path_before_existing_query_parameters() {
        assert_eq!(
            build_ws_url("wss://relay.example.com?token=abc", "room-1", "app"),
            "wss://relay.example.com/ws?token=abc&room_id=room-1&role=app"
        );
    }

    #[test]
    fn encodes_appended_query_values() {
        assert_eq!(
            build_ws_url("wss://relay.example.com/ws", "a b&c=d", "cli"),
            "wss://relay.example.com/ws?room_id=a%20b%26c%3Dd&role=cli"
        );
    }
}
