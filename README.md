# Sendspin Rust CLI

A high-performance Rust CLI audio player that connects to [Music Assistant](https://music-assistant.io/) and plays synchronized audio using the Sendspin protocol.

## Features

- 🎵 **Synchronized Audio Playback** - Time-synced playback across multiple players
- 🔍 **Automatic Server Discovery** - Zero-config setup using mDNS service discovery
- 🎚️ **Volume Control** - Software-based volume scaling (0-100)
- ⏯️ **Playback Control** - Stop, resume, and skip commands
- 🔊 **Cross-Platform Audio** - Uses CPAL for Linux, macOS, and Windows support
- 📦 **Lightweight** - Minimal dependencies, fast startup time
- 🧵 **Multi-threaded** - Separate threads for network and audio output

## Installation

### Pre-built Binaries

Download the latest release for your platform from the [releases page](https://github.com/s3than/sendspin-rs-cli/releases):

- **Linux AMD64**: `sendspin-rs-cli-linux-amd64`
- **Linux ARM64**: `sendspin-rs-cli-linux-arm64`
- **macOS Intel**: `sendspin-rs-cli-darwin-amd64`
- **macOS Apple Silicon**: `sendspin-rs-cli-darwin-arm64`
- **Windows**: `sendspin-rs-cli-windows-amd64.exe`

```bash
# Example installation (Linux AMD64)
wget https://github.com/s3than/sendspin-rs-cli/releases/latest/download/sendspin-rs-cli-linux-amd64
chmod +x sendspin-rs-cli-linux-amd64
sudo mv sendspin-rs-cli-linux-amd64 /usr/local/bin/sendspin-rs-cli

# Verify installation
sendspin-rs-cli --version
```

### Build from Source

Requirements:
- Rust 1.70 or later
- ALSA development libraries (Linux only): `libasound2-dev`

```bash
# Clone the repository
git clone https://github.com/s3than/sendspin-rs-cli.git
cd sendspin-rs-cli

# Build release binary
cargo build --release

# Binary will be at: target/release/sendspin-rs-cli
```

## Usage

### Basic Usage

```bash
# Auto-discover server via mDNS (recommended)
sendspin-rs-cli

# Specify server manually
sendspin-rs-cli --server 192.168.1.100:8927

# Custom player name and volume
sendspin-rs-cli --name "Living Room" --volume 50
```

### Command-line Options

```
Options:
  -s, --server <SERVER>        Server address (host:port). If not specified, uses mDNS discovery
  -n, --name <NAME>            Player name [default: "Sendspin-RS Player"]
      --client-id <CLIENT_ID>  Custom client ID (auto-generated if not specified)
  -v, --volume <VOLUME>        Initial volume (0-100) [default: 30]
  -b, --buffer <BUFFER>        Buffer size in milliseconds [default: 20]
  -h, --help                   Print help
      --version                Print version
```

### Examples

**Auto-discovery (zero-config):**
```bash
sendspin-rs-cli
```

**Specify server address:**
```bash
sendspin-rs-cli --server 192.168.1.100:8927
```

**Custom player configuration:**
```bash
sendspin-rs-cli \
  --name "Bedroom Speaker" \
  --volume 75 \
  --server 192.168.1.100:8927
```

**Enable debug logging:**
```bash
RUST_LOG=debug sendspin-rs-cli
```

## How It Works

### Architecture

```
┌─────────────────┐
│  Music Assistant│
│     Server      │
└────────┬────────┘
         │ WebSocket (Sendspin Protocol)
         │
┌────────▼────────┐
│  sendspin-rs-cli│
│                 │
│  ┌───────────┐  │
│  │  Decoder  │  │
│  └─────┬─────┘  │
│        │        │
│  ┌─────▼─────┐  │
│  │   Queue   │  │
│  └─────┬─────┘  │
│        │        │
│  ┌─────▼─────┐  │
│  │Time Sync  │  │
│  └─────┬─────┘  │
│        │        │
│  ┌─────▼─────┐  │
│  │   CPAL    │  │
│  │  Output   │  │
│  └───────────┘  │
└─────────────────┘
         │
         ▼
    🔊 Speakers
```

### Key Features

1. **mDNS Discovery**: Automatically finds Music Assistant servers on the local network using mDNS (`_sendspin-server._tcp.local.`)

2. **Time Synchronization**: Uses NTP-style clock sync to ensure audio plays at the exact right time across multiple players

3. **Simple Queue**: Audio buffers are decoded and queued with timestamps, then played at the precise moment

4. **Protocol Compatibility**: Includes a compatibility shim to handle protocol differences between the sendspin-rs library and Music Assistant server

## Development

### Running Tests

```bash
# Run all tests
cargo test

# Run with verbose output
cargo test -- --nocapture

# Run specific test
cargo test test_player_creation

# Generate coverage report
cargo tarpaulin --lib --exclude-files 'target/*'
```

Current test coverage: **49.62%** (65/131 lines)

### Building for Different Platforms

The project uses GitHub Actions to build binaries for multiple platforms:

```bash
# Native build (current platform)
cargo build --release

# Cross-compile for ARM64 Linux (requires cross)
cross build --release --target aarch64-unknown-linux-gnu

# Cross-compile for Windows (requires cross)
cross build --release --target x86_64-pc-windows-gnu
```

### Project Structure

```
sendspin-rs-cli/
├── src/
│   ├── main.rs      # Entry point and protocol handling
│   ├── player.rs    # Audio playback and queue management
│   ├── mdns.rs      # mDNS server discovery
│   ├── compat.rs    # Protocol compatibility shim
│   └── lib.rs       # Library exports for testing
├── tests/
│   └── integration_test.rs  # Integration tests
├── Cross.toml       # Cross-compilation configuration
├── rust-toolchain.toml      # Rust toolchain specification
└── .github/
    └── workflows/
        └── build.yml         # CI/CD pipeline
```

## Troubleshooting

### No server found via mDNS

If mDNS discovery fails, manually specify the server address:

```bash
sendspin-rs-cli --server <server-ip>:8927
```

### Audio device errors (Linux)

Make sure ALSA libraries are installed:

```bash
sudo apt-get install libasound2-dev
```

### Permission denied

Ensure the binary has execute permissions:

```bash
chmod +x sendspin-rs-cli
```

## Technical Details

### Supported Audio Formats

- **PCM**: Uncompressed audio (16-bit, 24-bit)
- Sample rates: 44.1kHz, 48kHz, 96kHz, etc.
- Channels: Mono, Stereo, Multi-channel

### Protocol

The player implements the Sendspin protocol for communicating with Music Assistant:

- **Transport**: WebSocket over TCP
- **Serialization**: JSON messages
- **Clock Sync**: NTP-style time synchronization
- **Audio**: Chunked streaming with timestamps

### Dependencies

Major dependencies:
- **sendspin-rs**: Core Sendspin protocol implementation
- **tokio**: Async runtime
- **cpal**: Cross-platform audio I/O
- **tokio-tungstenite**: WebSocket client
- **mdns-sd**: mDNS service discovery
- **clap**: Command-line argument parsing

## Contributing

Contributions are welcome! Please feel free to submit issues or pull requests.

### Development Setup

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/sendspin-rs-cli.git`
3. Create a feature branch: `git checkout -b feature/your-feature`
4. Make your changes and add tests
5. Run tests: `cargo test`
6. Run clippy: `cargo clippy -- -D warnings`
7. Run formatter: `cargo fmt`
8. Commit your changes: `git commit -am 'Add new feature'`
9. Push to the branch: `git push origin feature/your-feature`
10. Create a Pull Request

## License

This project is licensed under the Apache License - see the LICENSE file for details.

## Acknowledgments

- [Music Assistant](https://music-assistant.io/) - The music management system this player connects to
- [sendspin-rs](https://github.com/s3than/sendspin-rs) - Core Sendspin protocol library
- [cpal](https://github.com/RustAudio/cpal) - Cross-platform audio library

## Links

- **GitHub Repository**: https://github.com/s3than/sendspin-rs-cli
- **Music Assistant**: https://music-assistant.io/
- **Sendspin Protocol**: https://github.com/s3than/sendspin-rs
