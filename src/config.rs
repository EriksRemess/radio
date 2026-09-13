use crate::codec::{Codec, MP3_BITRATES, MP3_SAMPLE_RATES};
use std::{collections::HashMap, fs::File, io::Read, net::SocketAddr, path::Path};

pub struct StreamConfig {
    pub codec: Codec,
    pub mount: String,
    pub bitrate: u32,
}

pub struct Config {
    pub title: String,
    pub listen: SocketAddr,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub streams: [StreamConfig; 3],
    pub max_clients: usize,
    pub test_tone: bool,
}

impl Config {
    pub fn load(path: Option<&Path>, test_tone: bool) -> Result<Self, String> {
        let explicit = path.is_some();
        let path = path.unwrap_or_else(|| Path::new("radio.conf"));
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if !explicit && error.kind() == std::io::ErrorKind::NotFound => {
                return Self::parse("", test_tone);
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let mut text = String::new();
        file.take(65537)
            .read_to_string(&mut text)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if text.len() > 65536 {
            return Err("configuration file exceeds 64 KiB".into());
        }
        Self::parse(&text, test_tone).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(text: &str, test_tone: bool) -> Result<Self, String> {
        const KEYS: &[&str] = &[
            "title",
            "listen",
            "alsa_device",
            "sample_rate",
            "channels",
            "max_clients",
            "aac_mount",
            "aac_bitrate",
            "mp3_mount",
            "mp3_bitrate",
            "ogg_mount",
            "opus_bitrate",
        ];
        let mut values = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("line {}: expected key = value", index + 1))?;
            let (key, value) = (key.trim(), value.trim());
            if !KEYS.contains(&key) {
                return Err(format!("line {}: unknown setting {key:?}", index + 1));
            }
            if values.insert(key, value).is_some() {
                return Err(format!("line {}: duplicate setting {key}", index + 1));
            }
        }
        let get = |key, default| values.get(key).copied().unwrap_or(default);
        let title = get("title", "Line-in Radio").to_owned();
        if title.is_empty() || title.len() > 512 || title.chars().any(char::is_control) {
            return Err("title must be 1–512 UTF-8 bytes with no control characters".into());
        }
        let listen = get("listen", "0.0.0.0:8000")
            .parse()
            .map_err(|_| "listen must be an IP address and port")?;
        let sample_rate = number(get("sample_rate", "48000"), "sample_rate", 32000, 48000)?;
        if !MP3_SAMPLE_RATES.contains(&sample_rate) {
            return Err(
                "sample_rate must be 32000, 44100 or 48000 for AAC and MP3; Opus uses 48000".into(),
            );
        }
        let channels = number(get("channels", "2"), "channels", 1, 2)? as u8;
        let stream = |codec, mount_key, default_mount, bitrate_key, default_bitrate, min, max| {
            let mount = get(mount_key, default_mount).to_owned();
            if !mount.starts_with('/')
                || mount == "/"
                || mount.starts_with("//")
                || !mount
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b))
                || mount.split('/').any(|part| matches!(part, "." | ".."))
                || mount.len() > 1024
            {
                return Err(format!("{mount_key} must be an ASCII URL path other than /, without percent escapes, query, fragment or . and .. segments"));
            }
            let bitrate = bitrate(get(bitrate_key, default_bitrate), bitrate_key, min, max)?;
            Ok(StreamConfig {
                codec,
                mount,
                bitrate,
            })
        };
        let streams = [
            stream(
                Codec::Aac,
                "aac_mount",
                "/stream.aac",
                "aac_bitrate",
                "256k",
                16,
                512,
            )?,
            stream(
                Codec::Mp3,
                "mp3_mount",
                "/stream.mp3",
                "mp3_bitrate",
                "192k",
                32,
                320,
            )?,
            stream(
                Codec::Opus,
                "ogg_mount",
                "/stream.ogg",
                "opus_bitrate",
                "128k",
                6,
                510,
            )?,
        ];
        for (index, stream) in streams.iter().enumerate() {
            if streams[..index].iter().any(|s| s.mount == stream.mount) {
                return Err("stream mounts must be unique".into());
            }
        }
        if !MP3_BITRATES.contains(&streams[1].bitrate) {
            return Err(
                "mp3_bitrate must be a standard MPEG-1 Layer III bitrate (32k through 320k)".into(),
            );
        }
        // AAC-LC permits 6144 bits per channel in each 1024-sample frame.
        let aac_limit = 6 * sample_rate * u32::from(channels);
        if streams[0].bitrate > aac_limit {
            return Err(format!(
                "aac_bitrate must not exceed {aac_limit} bits/s at {sample_rate} Hz with {channels} channel(s)"
            ));
        }
        if channels == 1 && streams[2].bitrate > 256000 {
            return Err("opus_bitrate must not exceed 256k for mono audio".into());
        }
        let device = get("alsa_device", "hw:1,0").to_owned();
        if device.is_empty() || device.chars().any(char::is_control) {
            return Err("alsa_device must be nonempty with no control characters".into());
        }
        Ok(Self {
            title,
            listen,
            device,
            sample_rate,
            streams,
            test_tone,
            channels,
            max_clients: number(get("max_clients", "128"), "max_clients", 1, 1024)? as usize,
        })
    }
}

fn number(value: &str, key: &str, min: u32, max: u32) -> Result<u32, String> {
    value
        .parse::<u32>()
        .ok()
        .filter(|v| (min..=max).contains(v))
        .ok_or_else(|| format!("{key} must be an integer between {min} and {max}"))
}

fn bitrate(value: &str, key: &str, min: u32, max: u32) -> Result<u32, String> {
    if let Some(kbps) = value.strip_suffix('k') {
        number(kbps, key, min, max).map(|v| v * 1000)
    } else {
        number(value, key, min * 1000, max * 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrates_respect_channel_and_sample_rate_limits() {
        for (channels, rate, max_aac) in [
            (1, 32000, 192000),
            (1, 44100, 264600),
            (1, 48000, 288000),
            (2, 32000, 384000),
        ] {
            let config = format!("channels = {channels}\nsample_rate = {rate}\naac_bitrate = ");
            assert!(Config::parse(&format!("{config}{max_aac}"), true).is_ok());
            assert!(Config::parse(&format!("{config}{}", max_aac + 1), true).is_err());
        }
        assert!(Config::parse("channels = 1\nopus_bitrate = 256k", true).is_ok());
        assert!(Config::parse("channels = 1\nopus_bitrate = 256001", true).is_err());
        assert!(Config::parse("channels = 2\nopus_bitrate = 510k", true).is_ok());
    }

    #[test]
    fn validates_file_before_starting_encoder() {
        for text in [
            "aac_mount = /foo?bar",
            "aac_mount = //foo",
            "aac_mount = /",
            "aac_mount = /a\"><script>.aac",
            "aac_mount = /foo/../live.aac",
            "aac_mount = /./live.aac",
            "aac_mount = /%2e/live.aac",
            "aac_bitrate = 0",
            "aac_bitrate = 99999999999999k",
            "mp3_bitrate = 193k",
            "sample_rate = 47000",
            "channels = 3",
            "max_clients = 0",
            "listen = localhost:8000",
            "typo = 1",
            "channels = 1\nchannels = 2",
            "mp3_mount = /stream.aac",
            "no equals",
            "title =",
            "title = bad\rheader",
            "title = bad\0header",
        ] {
            assert!(Config::parse(text, false).is_err(), "accepted {text:?}");
        }
        let cfg = Config::parse("# Station\nlisten = 127.0.0.1:9000\naac_bitrate = 192000\nopus_bitrate = 96k\nchannels = 1\n", true).unwrap();
        assert_eq!(cfg.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.streams[0].bitrate, 192000);
        assert_eq!(cfg.streams[2].bitrate, 96000);
        assert_eq!(cfg.channels, 1);
        assert_eq!(cfg.title, "Line-in Radio");
        let cfg = Config::parse("title = Ērika Radio & Friends", false).unwrap();
        assert_eq!(cfg.title, "Ērika Radio & Friends");
        assert!(Config::parse(&format!("title = {}", "a".repeat(513)), false).is_err());
    }
}
