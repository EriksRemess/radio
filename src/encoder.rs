use crate::{codec::Codec, config::Config};
use std::{
    io,
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
};

pub struct Encoder(pub Child);

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub fn start(config: &Config) -> io::Result<(Encoder, Vec<UnixStream>)> {
    let mut command = Command::new("ffmpeg");
    command.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-nostdin",
    ]);
    if config.test_tone {
        command.args(["-re", "-f", "lavfi", "-i"]).arg(format!(
            "sine=frequency=440:sample_rate={}",
            config.sample_rate
        ));
    } else {
        command
            .args([
                "-fflags",
                "+genpts",
                "-f",
                "alsa",
                "-thread_queue_size",
                "8192",
                "-use_wallclock_as_timestamps",
                "1",
                "-i",
            ])
            .arg(&config.device);
    }
    let mut readers = Vec::new();
    let mut writers = Vec::new();
    for stream in &config.streams {
        let (reader, writer) = UnixStream::pair()?;
        command
            .args(["-map", "0:a:0", "-map_metadata", "-1", "-ac"])
            .arg(config.channels.to_string())
            .arg("-ar")
            .arg(
                if stream.codec == Codec::Opus {
                    48000
                } else {
                    config.sample_rate
                }
                .to_string(),
            )
            .args([
                "-af",
                "aresample=async=1:min_hard_comp=0.100:first_pts=0",
                "-b:a",
            ])
            .arg(stream.bitrate.to_string())
            .arg("-metadata")
            .arg(format!("title={}", config.title));
        match stream.codec {
            Codec::Aac => {
                command.args([
                    "-c:a",
                    "aac",
                    "-profile:a",
                    "aac_low",
                    "-write_id3v2",
                    "1",
                    "-f",
                    "adts",
                ]);
            }
            Codec::Mp3 => {
                command.args([
                    "-c:a",
                    "libmp3lame",
                    "-reservoir",
                    "0",
                    "-write_xing",
                    "0",
                    "-id3v2_version",
                    "3",
                    "-write_id3v1",
                    "0",
                    "-f",
                    "mp3",
                ]);
            }
            Codec::Opus => {
                command.args([
                    "-c:a",
                    "libopus",
                    "-application",
                    "audio",
                    "-frame_duration",
                    "20",
                    "-page_duration",
                    "20000",
                    "-f",
                    "ogg",
                ]);
            }
        }
        command
            .args(["-flush_packets", "1"])
            .arg(format!("pipe:{}", writer.as_raw_fd()));
        readers.push(reader);
        writers.push(writer);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // Retain three output descriptors across exec only in the child. Socket pairs
    // avoid extra listening ports, named FIFO files, and competing ALSA captures.
    // SAFETY: the closure only calls async-signal-safe fcntl on owned valid FDs.
    // Writers remain owned by the closure until Command is dropped after spawn.
    unsafe {
        command.pre_exec(move || {
            unsafe extern "C" {
                fn fcntl(fd: i32, command: i32, ...) -> i32;
            }
            for writer in &writers {
                if fcntl(writer.as_raw_fd(), 2, 0) == -1 {
                    // Linux F_SETFD, clear FD_CLOEXEC.
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    Ok((Encoder(child), readers))
}
