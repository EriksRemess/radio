# linein-radio

A small Linux line-in radio server written in Rust, using FFmpeg. Includes a web
player and AAC, MP3, and Ogg/Opus streams.

## Run with Podman

Install Podman with the crun runtime and a Compose provider. Put
[compose.yml](compose.yml) and [radio.conf](radio.conf) in the same directory.
Set your capture device and station
name in `radio.conf`. Use `arecord -l` to list ALSA devices.

```sh
podman compose pull
podman compose up -d --no-build
```

Open **http://localhost:8000/**. Play connects to live audio; Stop disconnects.
Stream URLs:

| Path | Format | Default bitrate |
| --- | --- | --- |
| `/stream.aac` | AAC | 256 kb/s |
| `/stream.mp3` | MP3 | 192 kb/s |
| `/stream.ogg` | Ogg/Opus | 128 kb/s |

The container uses `/dev/snd`; the user running Podman needs access to the capture
device. Set its capture level with your ALSA mixer. Stop an existing `radio`
container before replacing it.

## Configure

Edit [radio.conf](radio.conf), then restart:

```sh
podman compose restart
```

Use unquoted `key = value` lines and full-line `#` comments. Configuration is read
at startup; there are no environment settings.

- `title` sets the player heading and stream metadata for external players.
- `alsa_device` selects the capture device.
- `aac_bitrate`, `mp3_bitrate`, and `opus_bitrate` control quality.

The config is mounted read-only at `/app/radio.conf`. Change the host port in
`compose.yml` if needed. Changing `listen` also requires updating the container
port mapping.

For HTTPS, use nginx or another reverse proxy with response buffering disabled
and a long streaming timeout. The server itself serves plain HTTP.

See [DEVELOPMENT.md](DEVELOPMENT.md) for building, testing, and publishing.
