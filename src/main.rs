mod audio;
mod codec;
mod config;
mod encoder;
mod http;
mod signals;

use audio::Broadcast;
use config::Config;
use std::{
    env,
    io::{self, BufReader},
    net::TcpListener,
    path::PathBuf,
    process::ExitCode,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

const HELP: &str = "linein-radio — ALSA to AAC, MP3 and Ogg/Opus over HTTP

Usage: linein-radio [--config PATH] [--test-tone]
       linein-radio --help | --version

  --config PATH  Read a configuration file (default: ./radio.conf if present)
  --test-tone    Stream a real-time 440 Hz tone instead of capturing ALSA

Configuration uses unquoted key = value lines; # starts a full-line comment.
Defaults:
  title = Line-in Radio
  listen = 0.0.0.0:8000
  alsa_device = hw:1,0
  sample_rate = 48000
  channels = 2
  max_clients = 128
  aac_mount = /stream.aac
  aac_bitrate = 256k
  mp3_mount = /stream.mp3
  mp3_bitrate = 192k
  ogg_mount = /stream.ogg
  opus_bitrate = 128k

The web player is at /. No environment variables configure the server.
Requires Linux and FFmpeg with aac, libmp3lame and libopus encoders.
SIGINT/SIGTERM stop the server and encoder.
";

struct ClientSlot(Arc<AtomicUsize>);
impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn run(config: Config) -> io::Result<()> {
    signals::install()?;
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let (mut encoder, outputs) = encoder::start(&config)?;
    let broadcasts: Arc<[Broadcast; 3]> = Arc::new(std::array::from_fn(|_| Broadcast::default()));
    let (done_tx, done_rx) = mpsc::channel();
    let mut readers = Vec::new();
    for (index, output) in outputs.into_iter().enumerate() {
        let producers = Arc::clone(&broadcasts);
        let codec = config.streams[index].codec;
        let done_tx = done_tx.clone();
        readers.push(
            thread::Builder::new()
                .name(format!("{} encoder", codec.name()))
                .spawn(move || {
                    let producer = &producers[index];
                    let mut output = BufReader::new(output);
                    let result = (|| -> io::Result<()> {
                        producer.set_headers(codec.read_headers(&mut output)?);
                        loop {
                            producer.publish(codec.read_frame(&mut output)?);
                        }
                    })();
                    producer.close();
                    let _ = done_tx.send(io::Error::other(format!(
                        "{} stream ended: {}",
                        codec.name(),
                        result.unwrap_err()
                    )));
                })?,
        );
    }
    drop(done_tx);
    eprintln!(
        "Listening on http://{}/ ({} channels; {})",
        listener.local_addr()?,
        config.channels,
        if config.test_tone {
            "test tone"
        } else {
            &config.device
        }
    );
    for stream in &config.streams {
        eprintln!(
            "  {}: {} ({} bits/s)",
            stream.codec.name(),
            stream.mount,
            stream.bitrate
        );
    }
    let config = Arc::new(config);
    let clients = Arc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let result = loop {
        if signals::stopping() {
            break Ok(());
        }
        match done_rx.try_recv() {
            Ok(error) => break Err(error),
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err(io::Error::other("encoder readers stopped"))
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(index) = broadcasts.iter().position(|b| b.stalled(started)) {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "FFmpeg produced no {} audio for 10 seconds",
                    config.streams[index].codec.name()
                ),
            ));
        }
        match encoder.0.try_wait() {
            Ok(Some(status)) => break Err(io::Error::other(format!("FFmpeg exited: {status}"))),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Only the accept loop increments this counter. Other threads decrement it.
                if clients.load(Ordering::Relaxed) >= config.max_clients {
                    drop(stream);
                    continue;
                }
                clients.fetch_add(1, Ordering::Relaxed);
                let slot = ClientSlot(Arc::clone(&clients));
                let config = Arc::clone(&config);
                let broadcasts = Arc::clone(&broadcasts);
                if let Err(error) =
                    thread::Builder::new()
                        .name("listener".into())
                        .spawn(move || {
                            let _slot = slot;
                            // Broken connections and slow listeners are normal for live radio.
                            let _ = http::serve(stream, &config, &broadcasts);
                        })
                {
                    eprintln!("Could not start listener thread: {error}");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => break Err(error),
        }
    };
    for broadcast in broadcasts.iter() {
        broadcast.close();
    }
    drop(encoder);
    for reader in readers {
        let _ = reader.join();
    }
    result
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.len() == 1 && (args[0] == "--version" || args[0] == "-V") {
        println!("linein-radio {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut path = None;
    let mut test_tone = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--test-tone" && !test_tone {
            test_tone = true;
        } else if arg == "--config" && path.is_none() {
            if let Some(value) = args.next() {
                path = Some(PathBuf::from(value));
            } else {
                eprintln!("--config requires a path");
                return ExitCode::from(2);
            }
        } else {
            eprintln!("Invalid arguments. Use --help for usage.");
            return ExitCode::from(2);
        }
    }
    let config = match Config::load(path.as_deref(), test_tone) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Configuration error: {error}");
            return ExitCode::from(2);
        }
    };
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("linein-radio: {error}");
            ExitCode::FAILURE
        }
    }
}
