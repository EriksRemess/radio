//! Real encoder and HTTP checks, opt in with: cargo test --test smoke -- --ignored
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "linein-radio-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Server {
    process: Child,
    address: SocketAddr,
    temp: TempDir,
}

impl Server {
    fn start(overrides: &[(&str, &str)]) -> Self {
        let temp = TempDir::new();
        let log = File::create(temp.0.join("server.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_linein-radio"));
        let mut configuration = "listen = 127.0.0.1:0\n".to_owned();
        for &(key, value) in overrides {
            if key == "PATH" {
                command.env(key, value);
            } else {
                configuration.push_str(&format!("{key} = {value}\n"));
            }
        }
        let config_path = temp.0.join("radio.conf");
        fs::write(&config_path, configuration).unwrap();
        let process = command
            .arg("--test-tone")
            .arg("--config")
            .arg(config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut server = Self {
            process,
            address: "127.0.0.1:0".parse().unwrap(),
            temp,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = server.log();
            if let Some(address) = log
                .lines()
                .find_map(|line| line.strip_prefix("Listening on http://")?.split('/').next())
            {
                server.address = address.parse().unwrap();
                return server;
            }
            assert!(
                server.process.try_wait().unwrap().is_none(),
                "server exited early: {log}"
            );
            assert!(Instant::now() < deadline, "server did not bind: {log}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.temp.0.join("server.log")).unwrap()
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect_timeout(&self.address, Duration::from_secs(3)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
    }

    fn request(&self, request: &[u8]) -> String {
        let mut stream = self.connect();
        stream.write_all(request).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if ["aac", "mp3", "ogg"].iter().all(|extension| {
                self.request(format!("HEAD /stream.{extension} HTTP/1.0\r\n\r\n").as_bytes())
                    .starts_with("HTTP/1.1 200")
            }) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "stream not ready: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn children(&self) -> Vec<String> {
        let pid = self.process.id();
        fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
            .unwrap()
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    fn wait(&mut self, seconds: u64) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "server failed to exit: {}",
                self.log()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, number: i32) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        // SAFETY: the PID belongs to our live child; kill takes integer arguments.
        assert_eq!(unsafe { kill(self.process.id() as i32, number) }, 0);
    }

    fn listener(&self) -> BufReader<TcpStream> {
        let mut reader = self.listener_at("/stream.aac", "audio/aac");
        id3_tag(&mut reader);
        reader
    }

    fn listener_at(&self, path: &str, content_type: &str) -> BufReader<TcpStream> {
        let mut stream = self.connect();
        stream
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nIcy-MetaData: 1\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).unwrap(), 0);
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        assert!(headers.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(headers.contains(&format!("Content-Type: {content_type}\r\n")));
        assert!(!headers.to_lowercase().contains("\r\nicy-"));
        reader
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.process.try_wait().ok().flatten().is_none() {
            // Use the normal shutdown path even when an assertion unwinds.
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            // SAFETY: this is our child process, with a positive Linux PID.
            unsafe {
                kill(self.process.id() as i32, 15);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if self.process.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

fn frames(reader: &mut impl Read, count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|_| {
            let mut header = [0; 7];
            reader.read_exact(&mut header).unwrap();
            assert_eq!(header[0], 255);
            assert_eq!(header[1] & 0xf6, 0xf0);
            let size = ((header[3] as usize & 3) << 11)
                | ((header[4] as usize) << 3)
                | (header[5] as usize >> 5);
            assert!((8..=8191).contains(&size));
            let mut frame = vec![0; size];
            frame[..7].copy_from_slice(&header);
            reader.read_exact(&mut frame[7..]).unwrap();
            frame
        })
        .collect()
}

#[test]
#[ignore = "requires FFmpeg, ffprobe and loopback sockets"]
fn concurrent_and_late_listeners_decode_real_aac() {
    let server = Server::start(&[]);
    server.ready();
    let mut first = server.listener();
    frames(&mut first, 32);
    let mut late = server.listener();
    let (a, b) = thread::scope(|scope| {
        let a = scope.spawn(|| frames(&mut first, 96));
        let b = scope.spawn(|| frames(&mut late, 96));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert_eq!(server.children().len(), 1, "expected one shared encoder");
    assert!(
        a.iter().any(|frame| b.contains(frame)),
        "listeners did not share audio"
    );
    for audio in [a, b] {
        let path = server.temp.0.join("capture.aac");
        fs::write(&path, audio.concat()).unwrap();
        let probe = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-f",
                "aac",
                "-show_entries",
                "stream=codec_name,profile,sample_rate,channels",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(probe.status.success(), "{:?}", probe);
        let info = String::from_utf8(probe.stdout).unwrap();
        for field in [
            "codec_name=aac",
            "profile=LC",
            "sample_rate=48000",
            "channels=2",
        ] {
            assert!(info.lines().any(|line| line == field), "{info}");
        }
        let decoded = Command::new("ffmpeg")
            .args(["-v", "error", "-f", "aac", "-i"])
            .arg(path)
            .args(["-f", "s16le", "pipe:1"])
            .output()
            .unwrap();
        assert!(decoded.status.success());
        assert!(decoded.stderr.is_empty(), "{:?}", decoded.stderr);
        let samples: Vec<_> = decoded
            .stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f64)
            .collect();
        assert!(samples.len() > 48000 * 2);
        let rms = (samples.iter().map(|s| s * s).sum::<f64>() / samples.len() as f64).sqrt();
        assert!(rms > 100.0, "decoded audio was silent");
    }
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets"]
fn http_responses_and_fragmented_request() {
    let server = Server::start(&[]);
    server.ready();
    let response = server.request(b"HEAD /stream.aac HTTP/1.0\r\n\r\n");
    assert!(!response.to_lowercase().contains("\r\nicy-"));
    assert_eq!(response.split_once("\r\n\r\n").unwrap().1, "");
    for (request, status) in [
        ("GET /missing HTTP/1.0\r\n\r\n", 404),
        ("GET /admin HTTP/1.0\r\n\r\n", 404),
        ("POST /stream.aac HTTP/1.0\r\n\r\n", 405),
        ("PUT /stream.mp3 HTTP/1.0\r\n\r\n", 405),
        ("SOURCE /stream.ogg HTTP/1.0\r\n\r\n", 405),
        ("GET /stream.aac HTTP/1.1\r\n\r\n", 400),
    ] {
        assert!(
            server
                .request(request.as_bytes())
                .starts_with(&format!("HTTP/1.1 {status}"))
        );
    }
    // Exactly the limit, so the peer has no unread excess data that could cause a TCP reset.
    let prefix = "GET / HTTP/1.0\r\nX: ";
    let oversized = format!("{prefix}{}", "a".repeat(8192 - prefix.len()));
    assert!(
        server
            .request(oversized.as_bytes())
            .starts_with("HTTP/1.1 431")
    );
    let mut stream = server.connect();
    stream.write_all(b"HEAD /stream.aac HTTP/1.0\r\n").unwrap();
    thread::sleep(Duration::from_millis(20));
    stream.write_all(b"\r\n").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets; takes 5 seconds"]
fn connection_limit_and_total_header_deadline() {
    let server = Server::start(&[("max_clients", "1")]);
    let mut held = server.connect();
    held.write_all(b"GET ").unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(server.connect().read(&mut [0]).unwrap(), 0);
    for _ in 0..3 {
        thread::sleep(Duration::from_secs(1));
        held.write_all(b"a").unwrap();
    }
    let started = Instant::now();
    let mut response = String::new();
    held.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 408"));
    assert!(started.elapsed() < Duration::from_millis(2500));
    server.ready();
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets"]
fn signals_reap_encoder() {
    for signal in [2, 15] {
        let mut server = Server::start(&[]);
        server.ready();
        let children = server.children();
        assert_eq!(children.len(), 1);
        server.signal(signal);
        assert!(server.wait(3).success());
        assert!(
            !Path::new(&format!("/proc/{}", children[0])).exists(),
            "orphan FFmpeg process"
        );
    }
}

#[test]
#[ignore = "requires loopback sockets; takes 10 seconds"]
fn encoder_failure_and_stall_are_fatal() {
    for (script, wait) in [("exit 3", 3), ("exec /bin/sleep 60", 12)] {
        let temp = TempDir::new();
        let fake = temp.0.join("ffmpeg");
        fs::write(&fake, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let mut server = Server::start(&[("PATH", temp.0.to_str().unwrap())]);
        let children = if wait == 12 {
            assert!(
                server
                    .request(b"HEAD /stream.aac HTTP/1.0\r\n\r\n")
                    .starts_with("HTTP/1.1 503")
            );
            server.children()
        } else {
            Vec::new()
        };
        assert_eq!(server.wait(wait).code(), Some(1));
        if wait == 12 {
            assert_eq!(children.len(), 1);
        }
        for child in children {
            assert!(!Path::new(&format!("/proc/{child}")).exists());
        }
    }
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets"]
fn custom_stream_mount_is_used() {
    let server = Server::start(&[
        ("aac_mount", "/live.aac"),
        ("mp3_mount", "/live.mp3"),
        ("ogg_mount", "/live.ogg"),
    ]);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = server.request(b"HEAD /live.aac HTTP/1.0\r\n\r\n");
        if response.starts_with("HTTP/1.1 200") {
            assert!(response.contains("Content-Type: audio/aac\r\n"));
            break;
        }
        assert!(Instant::now() < deadline, "{}", server.log());
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        server
            .request(b"GET /stream.mp3 HTTP/1.0\r\n\r\n")
            .starts_with("HTTP/1.1 404")
    );
    let page = server.request(b"GET / HTTP/1.0\r\n\r\n");
    for mount in ["/live.aac?metadata=0", "/live.mp3", "/live.ogg"] {
        assert!(page.contains(&format!("value=\"{mount}\"")));
    }
}

fn id3_tag(reader: &mut impl Read) -> Vec<u8> {
    let mut header = [0; 10];
    reader.read_exact(&mut header).unwrap();
    assert_eq!(&header[..3], b"ID3");
    let size = header[6..].iter().fold(0, |n, b| (n << 7) | *b as usize);
    assert!(size <= 8192);
    let mut tag = vec![0; 10 + size];
    tag[..10].copy_from_slice(&header);
    reader.read_exact(&mut tag[10..]).unwrap();
    tag
}

fn other_frames(reader: &mut impl Read, format: &str, count: usize) -> Vec<u8> {
    let mut result = if format == "mp3" {
        id3_tag(reader)
    } else {
        Vec::new()
    };
    for sequence in 0..count {
        if format == "mp3" {
            let mut header = [0; 4];
            reader.read_exact(&mut header).unwrap();
            assert_eq!(header[0], 255);
            assert_eq!(header[1] & 0xfe, 0xfa);
            let rates = [44100, 48000, 32000];
            let kbps = [
                0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
            ];
            let size = 144000 * kbps[(header[2] >> 4) as usize]
                / rates[((header[2] >> 2) & 3) as usize]
                + ((header[2] >> 1) & 1) as usize;
            let mut frame = vec![0; size];
            frame[..4].copy_from_slice(&header);
            reader.read_exact(&mut frame[4..]).unwrap();
            result.extend(frame);
        } else {
            let mut header = [0; 27];
            reader.read_exact(&mut header).unwrap();
            assert_eq!(&header[..4], b"OggS");
            assert_eq!(
                u32::from_le_bytes(header[18..22].try_into().unwrap()),
                sequence as u32
            );
            let mut lacing = vec![0; header[26] as usize];
            reader.read_exact(&mut lacing).unwrap();
            let mut payload = vec![0; lacing.iter().map(|v| *v as usize).sum()];
            reader.read_exact(&mut payload).unwrap();
            if sequence == 0 {
                assert!(payload.starts_with(b"OpusHead"));
            }
            if sequence == 1 {
                assert!(payload.starts_with(b"OpusTags"));
            }
            result.extend(header);
            result.extend(lacing);
            result.extend(payload);
        }
    }
    result
}

fn decode(server: &Server, audio: &[u8], format: &str, codec: &str, rate: u32, channels: u8) {
    let path = server.temp.0.join(format!("capture.{format}"));
    fs::write(&path, audio).unwrap();
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,sample_rate,channels:format_tags=title:stream_tags=title",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(probe.status.success(), "{:?}", probe);
    assert!(
        probe.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let info = String::from_utf8(probe.stdout).unwrap();
    for field in [
        format!("codec_name={codec}"),
        format!("sample_rate={rate}"),
        format!("channels={channels}"),
        "TAG:title=Line-in Radio".into(),
    ] {
        assert!(info.lines().any(|line| line == field), "{info}");
    }
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "s16le", "pipe:1"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let samples = output.stdout.as_chunks::<2>().0;
    assert!(samples.len() > rate as usize * channels as usize);
    let rms = (samples
        .iter()
        .map(|b| (i16::from_le_bytes(*b) as f64).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt();
    assert!(rms > 100.0, "decoded {format} audio was silent");
}

#[test]
#[ignore = "requires FFmpeg, ffprobe and loopback sockets; checks late joins after ring rollover"]
fn mp3_and_ogg_decode_before_and_after_ring_rollover() {
    let server = Server::start(&[]);
    server.ready();
    // Wait for every encoder, not only AAC.
    let deadline = Instant::now() + Duration::from_secs(5);
    for path in ["/stream.mp3", "/stream.ogg"] {
        while !server
            .request(format!("HEAD {path} HTTP/1.0\r\n\r\n").as_bytes())
            .starts_with("HTTP/1.1 200")
        {
            assert!(Instant::now() < deadline, "{}", server.log());
            thread::sleep(Duration::from_millis(20));
        }
    }
    for round in 0..2 {
        if round == 1 {
            thread::sleep(Duration::from_secs(7));
        }
        let mut mp3 = server.listener_at("/stream.mp3", "audio/mpeg");
        let mut ogg = server.listener_at("/stream.ogg", "audio/ogg");
        let (mp3, ogg) = thread::scope(|scope| {
            let mp3 = scope.spawn(|| other_frames(&mut mp3, "mp3", 100));
            let ogg = scope.spawn(|| other_frames(&mut ogg, "ogg", 102));
            (mp3.join().unwrap(), ogg.join().unwrap())
        });
        decode(&server, &mp3, "mp3", "mp3", 48000, 2);
        decode(&server, &ogg, "ogg", "opus", 48000, 2);
        assert_eq!(
            server.children().len(),
            1,
            "expected one shared capture process"
        );
    }
}

#[test]
#[ignore = "requires FFmpeg, ffprobe and loopback sockets"]
fn alternate_rates_and_mono_decode_at_bitrate_limits() {
    for (rate, channels, aac_bitrate, opus_bitrate) in [
        (32000, 1, "192k", "256k"),
        (44100, 1, "264600", "256k"),
        (32000, 2, "384k", "510k"),
    ] {
        let server = Server::start(&[
            ("sample_rate", &rate.to_string()),
            ("channels", &channels.to_string()),
            ("aac_bitrate", aac_bitrate),
            ("opus_bitrate", opus_bitrate),
        ]);
        server.ready();
        let mut aac = server.listener_at("/stream.aac", "audio/aac");
        let mut mp3 = server.listener_at("/stream.mp3", "audio/mpeg");
        let mut ogg = server.listener_at("/stream.ogg", "audio/ogg");
        let (aac, mp3, ogg) = thread::scope(|scope| {
            let aac = scope.spawn(|| {
                let mut audio = id3_tag(&mut aac);
                audio.extend(frames(&mut aac, 64).concat());
                audio
            });
            let mp3 = scope.spawn(|| other_frames(&mut mp3, "mp3", 64));
            let ogg = scope.spawn(|| other_frames(&mut ogg, "ogg", 102));
            (
                aac.join().unwrap(),
                mp3.join().unwrap(),
                ogg.join().unwrap(),
            )
        });
        decode(&server, &aac, "aac", "aac", rate, channels);
        decode(&server, &mp3, "mp3", "mp3", rate, channels);
        decode(&server, &ogg, "ogg", "opus", 48000, channels);
        assert!(!server.log().contains("clamping"), "{}", server.log());
    }
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets"]
fn player_get_and_head_use_configured_mounts_safely() {
    let server = Server::start(&[("aac_mount", "/a&b.aac"), ("title", "<Radio> & Friends")]);
    let get = server.request(b"GET / HTTP/1.0\r\n\r\n");
    let (headers, body) = get.split_once("\r\n\r\n").unwrap();
    assert!(headers.contains("Content-Type: text/html; charset=utf-8"));
    assert!(headers.contains(&format!("Content-Length: {}\r\n", body.len())));
    assert!(body.contains("/a&amp;b.aac?metadata=0"));
    assert!(body.contains("<audio id=\"audio\" preload=\"none\" hidden>"));
    assert!(body.contains("id=\"toggle-play\""));
    assert!(body.contains("<title>&lt;Radio&gt; &amp; Friends</title>"));
    assert!(body.contains("<h1>&lt;Radio&gt; &amp; Friends</h1>"));
    let head = server.request(b"HEAD / HTTP/1.0\r\n\r\n");
    assert_eq!(head.split_once("\r\n\r\n").unwrap().1, "");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = server.request(b"HEAD /a&b.aac?metadata=0 HTTP/1.0\r\n\r\n");
        if response.starts_with("HTTP/1.1 200") {
            break;
        }
        assert!(Instant::now() < deadline, "{}", server.log());
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn explicit_missing_config_and_invalid_cli_fail() {
    let temp = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_linein-radio"))
        .arg("--config")
        .arg(temp.0.join("missing.conf"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing.conf"));
    let output = Command::new(env!("CARGO_BIN_EXE_linein-radio"))
        .arg("--config")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
#[ignore = "requires FFmpeg, ffprobe and loopback sockets"]
fn configured_title_reaches_external_demuxers_for_every_format() {
    let title = "Ērika Radio & Friends";
    let server = Server::start(&[("title", title)]);
    server.ready();
    for extension in ["aac", "mp3", "ogg"] {
        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-analyzeduration",
                "1000000",
                "-read_intervals",
                "%+1",
                "-show_entries",
                "format_tags=title:stream_tags=title",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(format!("http://{}/stream.{extension}", server.address))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata = String::from_utf8(output.stdout).unwrap();
        assert!(
            metadata
                .lines()
                .any(|line| line == format!("TAG:title={title}")),
            "{extension}: {metadata}"
        );
    }
}

#[test]
#[ignore = "requires FFmpeg and loopback sockets"]
fn browser_aac_starts_with_adts_while_external_players_receive_id3() {
    let server = Server::start(&[]);
    server.ready();
    let mut browser = server.listener_at("/stream.aac?metadata=0", "audio/aac");
    assert_eq!(frames(&mut browser, 3).len(), 3);
    let mut external = server.listener_at("/stream.aac", "audio/aac");
    assert!(id3_tag(&mut external).starts_with(b"ID3"));
    assert_eq!(frames(&mut external, 3).len(), 3);
    let page = server.request(b"GET / HTTP/1.0\r\n\r\n");
    assert!(page.contains("value=\"/stream.aac?metadata=0\""));
}
