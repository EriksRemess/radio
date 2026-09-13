use crate::{
    audio::Broadcast,
    codec::{self, Codec},
    config::Config,
};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

const HEADER_LIMIT: usize = 8192;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
struct Request<'a> {
    head: bool,
    path: &'a str,
    metadata: bool,
}

fn parse_request(bytes: &[u8]) -> Result<Request<'_>, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "400 Bad Request")?;
    if !text.ends_with("\r\n\r\n") {
        return Err("400 Bad Request");
    }
    let mut lines = text[..text.len() - 4].split("\r\n");
    let mut parts = lines.next().ok_or("400 Bad Request")?.split(' ');
    let method = parts.next().ok_or("400 Bad Request")?;
    let path = parts.next().ok_or("400 Bad Request")?;
    let version = parts.next().ok_or("400 Bad Request")?;
    if parts.next().is_some()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || !path.starts_with('/')
        || !path.bytes().all(|b| b.is_ascii_graphic() && b != b'#')
    {
        return Err("400 Bad Request");
    }
    let mut host_seen = false;
    let mut length_seen = false;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or("400 Bad Request")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value
                .bytes()
                .any(|b| (b.is_ascii_control() && b != b'\t') || b == 127)
        {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("host") {
            if host_seen || value.trim().is_empty() {
                return Err("400 Bad Request");
            }
            host_seen = true;
        }
        // There is one request per connection, with no request body or upgrades.
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("400 Bad Request");
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length_seen || value.trim() != "0" {
                return Err("400 Bad Request");
            }
            length_seen = true;
        }
    }
    if version == "HTTP/1.1" && !host_seen {
        return Err("400 Bad Request");
    }
    if !matches!(method, "GET" | "HEAD") {
        return Err("405 Method Not Allowed");
    }
    Ok(Request {
        head: method == "HEAD",
        path: path.split('?').next().unwrap(),
        metadata: !path
            .split_once('?')
            .is_some_and(|(_, query)| query.split('&').any(|part| part == "metadata=0")),
    })
}

fn read_request(stream: &mut TcpStream) -> Result<Vec<u8>, &'static str> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut request = Vec::with_capacity(1024);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("408 Request Timeout");
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|_| "400 Bad Request")?;
        let mut buffer = [0; 1024];
        match stream.read(&mut buffer) {
            Ok(0) => return Err("400 Bad Request"),
            Ok(n) => request.extend_from_slice(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("408 Request Timeout");
            }
            Err(_) => return Err("400 Bad Request"),
        }
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            if end + 4 > HEADER_LIMIT {
                return Err("431 Request Header Fields Too Large");
            }
            request.truncate(end + 4);
            return Ok(request);
        }
        if request.len() >= HEADER_LIMIT {
            return Err("431 Request Header Fields Too Large");
        }
    }
}

fn error_response(stream: &mut TcpStream, status: &str) -> io::Result<()> {
    let allow = if status.starts_with("405") {
        "Allow: GET, HEAD\r\n"
    } else {
        ""
    };
    write!(stream, "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\nCache-Control: no-store\r\n{allow}\r\n")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn player(config: &Config) -> String {
    let options: String = config
        .streams
        .iter()
        .map(|s| {
            // Firefox sniffs a leading ID3 tag as MP3 regardless of audio/aac.
            // Browser AAC starts with ADTS; the normal URL retains its title tag.
            let path = escape_html(&if s.codec == Codec::Aac {
                format!("{}?metadata=0", s.mount)
            } else {
                s.mount.clone()
            });
            format!(
                "<option value=\"{path}\" data-type=\"{}\">{} · {} kb/s</option>",
                s.codec.content_type(),
                s.codec.name(),
                s.bitrate / 1000
            )
        })
        .collect();
    // Substitute only template tokens, never tokens inside configured values.
    include_str!("player.html")
        .split("{{TITLE}}")
        .map(|part| part.replace("{{OPTIONS}}", &options))
        .collect::<Vec<_>>()
        .join(&escape_html(&config.title))
}

pub fn serve(
    mut stream: TcpStream,
    config: &Config,
    broadcasts: &[Broadcast; 3],
) -> io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_nodelay(true)?;
    let bytes = match read_request(&mut stream) {
        Ok(bytes) => bytes,
        Err(status) => return error_response(&mut stream, status),
    };
    let request = match parse_request(&bytes) {
        Ok(request) => request,
        Err(status) => return error_response(&mut stream, status),
    };
    if request.path == "/" {
        let page = player(config);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n", page.len())?;
        if !request.head {
            stream.write_all(page.as_bytes())?;
        }
        return Ok(());
    }
    let Some(index) = config.streams.iter().position(|s| s.mount == request.path) else {
        return error_response(&mut stream, "404 Not Found");
    };
    let broadcast = &broadcasts[index];
    let codec = config.streams[index].codec;
    let Some(mut cursor) = broadcast.subscribe() else {
        return error_response(&mut stream, "503 Service Unavailable");
    };
    // A close-delimited body works with HTTP/1.0 radio players and HTTP/1.1 clients.
    // Titles are carried in cached format headers, without ICY metadata.
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Accel-Buffering: no\r\n\r\n", codec.content_type())?;
    if request.head {
        return Ok(());
    }
    if codec != Codec::Aac || request.metadata {
        for header in broadcast.headers() {
            if codec == Codec::Opus {
                stream.write_all(&codec::ogg_header(&header, cursor > 0))?;
            } else {
                stream.write_all(&header)?;
            }
        }
    }
    let mut sequence = 2;
    while let Some(frame) = broadcast.next_frame(&mut cursor) {
        if codec == Codec::Opus {
            stream.write_all(&codec::ogg_audio(&frame, &mut sequence))?;
        } else {
            stream.write_all(&frame)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_players_and_head_requests() {
        assert_eq!(
            parse_request(b"GET /stream.aac HTTP/1.0\r\n\r\n"),
            Ok(Request {
                head: false,
                path: "/stream.aac",
                metadata: true,
            })
        );
        assert_eq!(parse_request(b"HEAD /stream.aac?nocache=1 HTTP/1.1\r\nHost: localhost\r\nIcy-MetaData: 1\r\n\r\n"), Ok(Request { head: true, path: "/stream.aac", metadata: true }));
        assert_eq!(
            parse_request(b"GET /stream.aac?metadata=0&nocache=1 HTTP/1.0\r\n\r\n"),
            Ok(Request {
                head: false,
                path: "/stream.aac",
                metadata: false
            })
        );
    }

    #[test]
    fn rejects_ambiguous_requests() {
        for request in [
            "GET /stream.aac HTTP/1.1\r\n\r\n",
            "GET /stream.aac HTTP/1.0 extra\r\n\r\n",
            "GET / HTTP/1.0\r\nHost: one\r\nHost: two\r\n\r\n",
            "GET / HTTP/1.0\r\nHost : one\r\n\r\n",
            "GET / HTTP/1.0\r\nContent-Length: 1\r\n\r\n",
            "GET / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET / HTTP/1.0\r\nX-Header: a\nb\r\n\r\n",
            "GET / HTTP/1.0\r\n folded: value\r\n\r\n",
        ] {
            assert_eq!(
                parse_request(request.as_bytes()),
                Err("400 Bad Request"),
                "{request:?}"
            );
        }
        assert_eq!(
            parse_request(b"POST / HTTP/1.0\r\n\r\n"),
            Err("405 Method Not Allowed")
        );
    }
}
