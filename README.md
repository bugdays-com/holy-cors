# Holy CORS!

> "Holy CORS!" - what every developer mutters when they hit a CORS error.

A fast, lightweight CORS proxy for developers. Run it locally to bypass CORS restrictions when testing browser-based API tools.

```
    _   _       _          ____  ___  ____  ____  _
   | | | | ___ | |_   _   / ___|/ _ \|  _ \/ ___|| |
   | |_| |/ _ \| | | | | | |   | | | | |_) \___ \| |
   |  _  | (_) | | |_| | | |___| |_| |  _ < ___) |_|
   |_| |_|\___/|_|\__, |  \____|\___/|_| \_\____/(_)
                  |___/
```

## Features

- **Fast** - Built with Rust and Hyper for minimal overhead
- **Secure by default** - Only allows requests from bugdays.com (configurable)
- **Protocol support** - HTTP/1.1, HTTP/2, SSE streaming, gRPC-web, SOAP
- **Easy to use** - Single binary, no configuration required
- **Cross-platform** - macOS, Linux, and Windows

## Installation

### macOS (Homebrew)

```bash
brew install bugdays/tap/holy-cors
```

### Docker

```bash
docker run -p 8080:8080 ghcr.io/bugdays-com/holy-cors
```

### Manual Download

Download the latest binary from [GitHub Releases](https://github.com/bugdays-com/holy-cors/releases).

### Build from Source

```bash
git clone https://github.com/bugdays-com/holy-cors.git
cd holy-cors
cargo build --release
./target/release/holy-cors
```

## Usage

### Basic Usage

```bash
# Start the proxy on default port 8080
holy-cors

# Custom port
holy-cors --port 9000

# Enable verbose logging
holy-cors -v
```

### Allowing Additional Origins

By default, Holy CORS only accepts requests from `bugdays.com`. To allow additional origins:

```bash
# Allow localhost development
holy-cors --allow-origin http://localhost:3000

# Allow multiple origins
holy-cors --allow-origin http://localhost:3000 --allow-origin http://localhost:4321

# Allow ALL origins (development only - be careful!)
holy-cors --allow-all-origins
```

### Making Requests

From your browser or JavaScript code:

```javascript
// Proxy a request to any API
fetch('http://localhost:8080/https://api.github.com/users/octocat')
  .then(r => r.json())
  .then(console.log);

// POST request with body
fetch('http://localhost:8080/https://httpbin.org/post', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ hello: 'world' })
})
  .then(r => r.json())
  .then(console.log);
```

### URL Format

```
http://localhost:8080/{TARGET_URL}
```

Examples:
- `http://localhost:8080/https://api.example.com/data`
- `http://localhost:8080/https://httpbin.org/get?foo=bar`
- `http://localhost:8080/http://internal-api.local/endpoint`

## CLI Reference

```
Holy CORS! A fast CORS proxy for developers

Usage: holy-cors [OPTIONS]

Options:
  -p, --port <PORT>              Port to listen on [default: 8080]
      --allow-origin <ORIGIN>    Additional origins to allow (can be repeated)
      --allow-all-origins        Allow all origins (development mode)
  -v, --verbose                  Enable verbose logging
      --bind <ADDRESS>           Bind address [default: 0.0.0.0]
  -h, --help                     Print help
  -V, --version                  Print version
```

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `HOLY_CORS_PORT` | Port to listen on | `8080` |
| `HOLY_CORS_BIND` | Address to bind to | `0.0.0.0` |
| `HOLY_CORS_ORIGINS` | Comma-separated list of allowed origins | `bugdays.com` |
| `HOLY_CORS_ALLOW_ALL` | Allow all origins | `false` |
| `HOLY_CORS_VERBOSE` | Enable verbose logging | `false` |

## Docker

### Using Docker Compose

```yaml
version: '3.8'
services:
  holy-cors:
    image: ghcr.io/bugdays-com/holy-cors
    ports:
      - "8080:8080"
    environment:
      - HOLY_CORS_ORIGINS=http://localhost:3000
```

### Using Docker Run

```bash
# Basic
docker run -p 8080:8080 ghcr.io/bugdays-com/holy-cors

# With custom origins
docker run -p 8080:8080 \
  -e HOLY_CORS_ORIGINS=http://localhost:3000,http://localhost:4321 \
  ghcr.io/bugdays-com/holy-cors

# Allow all origins
docker run -p 8080:8080 \
  -e HOLY_CORS_ALLOW_ALL=true \
  ghcr.io/bugdays-com/holy-cors
```

## Protocol Support

| Protocol | Support |
|----------|---------|
| HTTP/1.1 | Full |
| HTTP/2 | Full |
| HTTPS | Full |
| SSE (Server-Sent Events) | Full (streaming) |
| gRPC-Web | Full |
| SOAP | Full |
| WebSocket | Experimental |

## Security

Holy CORS is designed to run **locally on your development machine**. It:

- Only allows requests from configured origins (bugdays.com by default)
- Validates URL schemes (only http/https allowed)
- Does not implement rate limiting (it's your machine, your rules)

**Warning**: Using `--allow-all-origins` disables origin checking. Only use this in development environments.

## Contributing

Contributions are welcome! Please open an issue or submit a pull request.

## License

MIT License - see [LICENSE](LICENSE) for details.

---

Built with love by [Bug Days](https://bugdays.com)
