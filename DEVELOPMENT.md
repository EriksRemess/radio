# Development

For running and configuring the radio, see [README.md](README.md).

## Build and run

Requires Linux, Rust 1.100+, and FFmpeg with ALSA input and the `aac`, `libmp3lame`,
and `libopus` encoders. There are no Cargo dependencies.

```sh
cargo build --release --offline --locked
cp radio.conf radio.local.conf
./target/release/linein-radio --config radio.local.conf --test-tone
```

Open http://localhost:8000/. `--test-tone` generates audio without a sound device;
omit it to capture line-in. Stop with Ctrl-C. `radio.local.conf` is ignored by Git.

To build and test the container without sound hardware:

```sh
podman build --pull=always -t localhost/linein-radio:dev .
podman run --rm -p 8000:8000 localhost/linein-radio:dev --test-tone
```

## Checks

```sh
cargo fmt --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked -- --include-ignored
node --test tests/player.mjs
```

Tests require FFmpeg, ffprobe, local sockets, and Node.js 18+ for player checks.
Physical line-in capture needs a connected source.

## Implementation

One FFmpeg process captures and encodes all three streams through private pipes.
Rust broadcasts complete frames from bounded buffers and disconnects slow clients.
Encoder failure or 10 seconds without audio stops the server.

AAC includes ID3 title metadata; the web player disables it with `?metadata=0` for browser compatibility.

## GitHub Actions

[The workflow](.github/workflows/container.yml) runs the checks, then builds AMD64
and ARM64 images. Pull requests only test and build. It publishes to
`ghcr.io/eriksremess/radio`:

- `main` → `latest`
- Version tags: `v0.1.0` → `0.1.0`
- Published builds also receive a `sha-<short commit>` tag.

Manual runs publish only from `main` or a `v*` tag. Authentication uses the automatic
`GITHUB_TOKEN` with `packages: write`; no extra secret is needed.

Make the package public for anonymous pulls.
