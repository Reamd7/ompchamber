//! Unit tests for the dev-tunnel WS client primitives (SHA-1, handshake URL
//! building, frame codec) and path matching; the route-level and end-to-end
//! dev-tunnel tests live in `routes_tests.rs`.

use super::*;

fn sha1_hex(data: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(sha1(data.as_bytes()))
}

#[test]
fn sha1_known_vectors() {
    // RFC 3174 test vectors (base64-rendered for compactness).
    assert_eq!(sha1_hex("abc"), "qZk+NkcGgWq6PiVxeFDCbJzQ2J0=");
    assert_eq!(sha1_hex(""), "2jmj7l5rSw0yVb/vlWAYkK/YBwk=");
    assert_eq!(
        sha1_hex("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "hJg+RBw70m66rkqh+VEp5eVGcPE="
    );
    // Multi-block input.
    let million_a = "a".repeat(1_000_000);
    assert_eq!(sha1_hex(&million_a), "NKqXPNTE2qT2Husr260nMWU0AW8=");
}

#[test]
fn ws_accept_key_matches_rfc_example() {
    // RFC 6455 §1.3: key "dGhlIHNhbXBsZSBub25jZQ==" → accept
    // "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=".
    assert_eq!(
        ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
}

#[test]
fn builds_ws_urls_from_http_bases() {
    assert_eq!(
        to_websocket_url("http://127.0.0.1:8080", 5173).unwrap(),
        "ws://127.0.0.1:8080/api/dev-tunnel?port=5173"
    );
    assert_eq!(
        to_websocket_url("http://127.0.0.1:8080/", 3000).unwrap(),
        "ws://127.0.0.1:8080/api/dev-tunnel?port=3000"
    );
    assert_eq!(
        to_websocket_url("https://remote.example.com", 5173).unwrap(),
        "wss://remote.example.com/api/dev-tunnel?port=5173"
    );
    // Non-special schemes are rejected before any socket is opened (JS:
    // WHATWG URL silently keeps them, so the client used to crash later).
    assert_eq!(
        to_websocket_url("ompchamber-ui://index", 5173),
        Err("The remote base URL must be http(s); got \"ompchamber-ui\"".to_string())
    );
}

#[test]
fn client_frame_encoding_is_masked_and_parses_back() {
    // A masked client frame must be decodable by the server-side reader.
    let frame = encode_client_frame(0x2, b"hello tunnel");
    assert_eq!(frame[0], 0x82, "FIN + binary opcode");
    assert_eq!(frame[1] & 0x80, 0x80, "mask bit set");
    assert_eq!((frame[1] & 0x7f) as usize, b"hello tunnel".len());

    // Feed it through an unmasking parse (the reader path used by the fake
    // server in routes_tests.rs).
    let mask = [frame[2], frame[3], frame[4], frame[5]];
    let mut payload = frame[6..].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    assert_eq!(payload, b"hello tunnel".to_vec());
}

#[tokio::test]
async fn reads_unmasked_server_frames_with_fragments_and_ping() {
    use tokio::io::AsyncWriteExt;
    let (mut writer, reader) = tokio::io::duplex(4096);
    let mut reader = reader;

    // Fragmented binary message: first frame FIN=0, then a continuation.
    let part1 = [0x02u8, 0x03, b'a', b'b', b'c'];
    let part2 = [0x80u8, 0x02, b'd', b'e'];
    writer.write_all(&part1).await.unwrap();
    writer.write_all(&part2).await.unwrap();
    let frame = read_server_frame(&mut reader).await.unwrap().unwrap();
    match frame {
        ClientFrame::Binary(payload) => assert_eq!(payload, b"abcde".to_vec()),
        other => panic!("expected binary, got {other:?}"),
    }

    // Ping frames surface for the caller to pong.
    let ping = [0x89u8, 0x02, b'p', b'!'];
    writer.write_all(&ping).await.unwrap();
    let frame = read_server_frame(&mut reader).await.unwrap().unwrap();
    match frame {
        ClientFrame::Ping(payload) => assert_eq!(payload, b"p!".to_vec()),
        other => panic!("expected ping, got {other:?}"),
    }

    // Close frame ends the stream.
    let close = [0x88u8, 0x00];
    writer.write_all(&close).await.unwrap();
    let frame = read_server_frame(&mut reader).await.unwrap().unwrap();
    assert!(matches!(frame, ClientFrame::Close));

    // EOF after close.
    drop(writer);
    let frame = read_server_frame(&mut reader).await.unwrap();
    assert!(frame.is_none());
}

#[test]
fn dev_tunnel_path_matching_only_claims_its_path() {
    assert!(is_dev_tunnel_path("/api/dev-tunnel?port=5173"));
    assert!(is_dev_tunnel_path("/api/dev-tunnel"));
    assert!(!is_dev_tunnel_path("/api/terminal/ws"));
    assert!(!is_dev_tunnel_path(""));
    assert!(!is_dev_tunnel_path("not a url"));
}

#[test]
fn parse_requested_port_validates_range_and_path() {
    fn port_of(uri: &str) -> Option<u16> {
        parse_requested_port(&uri.parse().unwrap())
    }

    assert_eq!(port_of("/api/dev-tunnel?port=5173"), Some(5173));
    assert_eq!(port_of("/api/dev-tunnel?port=1"), Some(1));
    assert_eq!(port_of("/api/dev-tunnel?port=65535"), Some(65535));
    assert_eq!(port_of("/api/dev-tunnel?port=0"), None);
    assert_eq!(port_of("/api/dev-tunnel?port=65536"), None);
    assert_eq!(port_of("/api/dev-tunnel?port=abc"), None);
    assert_eq!(port_of("/api/dev-tunnel"), None);
    assert_eq!(port_of("/api/other?port=5173"), None);
}
