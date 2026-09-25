# Holy CORS

**Browser tools, with local superpowers.**

Holy CORS is the small local network bridge for [Bug Days](https://bugdays.com). It lets browser-based developer tools reach APIs and services on your machine or network while keeping control of the connection on your computer.

```text
    _   _       _          ____  ___  ____  ____  _
   | | | | ___ | |_   _   / ___|/ _ \|  _ \/ ___|| |
   | |_| |/ _ \| | | | | | |   | | | | |_) \___ \| |
   |  _  | (_) | | |_| | | |___| |_| |  _ < ___) |_|
   |_| |_|\___/|_|\__, |  \____|\___/|_| \_\____/(_)
                  |___/
```

## What it does

- **Native gRPC bridge** — translates binary gRPC-Web requests from the Bug Days client into native gRPC over HTTP/2, including response trailers
- **HTTP/S bridge** — proxies REST, SOAP, and other HTTP requests while adding browser-readable CORS response headers
- **DNS diagnostics** — queries the resolver configured for the bridge environment, including private, VPN, split-horizon, and PTR reverse DNS records
- **TLS certificate inspection** — connects to a hostname or IP on any direct-TLS port and returns the complete server-presented certificate chain plus negotiated TLS details
- **Kafka access** — browse records, inspect consumer groups and lag, preview offset resets, and replay messages using Kafka's native protocol
- **Streaming responses** — passes response bodies through as they arrive, including server-streaming gRPC and SSE
- **Safe local default** — listens on `127.0.0.1` and accepts browser requests only from Bug Days origins unless you opt in to more
- **No project configuration** — one binary, one command, no sidecar YAML
- **Cross-platform** — builds for macOS, Linux, and Windows

Holy CORS does not upload your requests to Bug Days. Traffic travels from your browser to the local bridge and then directly to the target you entered.

## Install and run

### macOS with Homebrew

```bash
brew install bugdays-com/tap/holy-cors
holy-cors
```

The fully qualified install trusts only the Holy CORS formula. To update an existing installation, run `brew update && brew upgrade holy-cors`.

### Manual download

Download the latest binary from [GitHub Releases](https://github.com/bugdays-com/holy-cors/releases), then run `holy-cors`.

### Build from source

```bash
git clone https://github.com/bugdays-com/holy-cors.git
cd holy-cors
cargo build --release
./target/release/holy-cors
```

Source builds need a C compiler, CMake, and Perl for the bundled Kafka TLS library. Kafka support does not require a separate proxy or broker-side plugin.

When the bridge is ready, open `http://127.0.0.1:2345/api/v1/capabilities`. Bug Days checks this endpoint automatically and tells you whether the installed version supports the requested feature.

## Use it with DNS and TLS certificate tools

Start `holy-cors`, then open either tool:

- [DNS Lookup and Reverse DNS Checker](https://bugdays.com/dns-lookup) compares public DNS with the system resolver available to Holy CORS. Native installations use the device resolver; Docker uses the container's configured DNS.
- [TLS Certificate Chain Checker](https://bugdays.com/certificate-inspector) connects to HTTPS, Kafka SSL, LDAPS, SMTPS, IMAPS, or another port where TLS starts immediately. It returns the complete chain even when the separate trust check fails.

The versioned JSON endpoints are:

```text
POST /api/v1/dns/query
POST /api/v1/tls/inspect
```

Example DNS request:

```json
{
  "name": "service.internal",
  "types": ["A", "AAAA", "CNAME", "SRV"]
}
```

Enter an IPv4 or IPv6 address to run a PTR reverse lookup. The response also forward-resolves returned hostnames and reports whether one maps back to the original address.

Example TLS request:

```json
{
  "host": "10.20.4.8",
  "port": 9093,
  "serverName": "broker-1.example.internal",
  "timeoutMs": 10000
}
```

`host` selects the TCP destination. `serverName` selects SNI and the identity used by the device trust check, which is useful when a service is reached by IP. STARTTLS-style protocols are not negotiated by this endpoint; it expects TLS to begin immediately after connecting.

## Use it with the Kafka client

Open [Kafka Message Browser](https://bugdays.com/kafka-client/) or [Kafka Consumer Diagnostics](https://bugdays.com/kafka-diagnostics/) after starting Holy CORS. The browser sends Kafka requests to the bridge on your device. The bridge speaks Kafka's native protocol to your bootstrap brokers and follows the broker addresses returned in cluster metadata.

The Kafka API lives under `/api/v1/kafka/`. A `POST /connect` body supplies bootstrap servers and optional TLS, SASL PLAIN, or SASL SCRAM credentials. It returns a random session token that stays in the browser tab; credentials go to the local bridge and target broker, never Bug Days servers. Supported actions are `metadata`, `topic`, `browse`, `groups`, `group`, `reset-preview`, `reset-apply`, `replay`, and `disconnect`. Connections expire after 30 minutes without use. Offset reset previews expire after 5 minutes and are checked again immediately before applying.

Message keys, values, and header values use Base64 in JSON; `null` remains distinct from an empty byte string. Offsets are decimal strings to preserve 64-bit precision in JavaScript. Browse reads manually assigned partitions without joining or committing a group. Replay batches contain at most 200 messages and return broker-acknowledged destination offsets. Keep the browser tab open for a large replay.

Only Kafka bootstrap hosts you configure are contacted. A Kafka connection may still fail if brokers advertise addresses your device cannot reach; connect from the same network or VPN as the target application.

## Use it with the Bug Days gRPC client

1. Start `holy-cors`.
2. Open [bugdays.com/grpc-client](https://bugdays.com/grpc-client).
3. Leave the transport set to **Native gRPC (bridge)**.
4. Enter an HTTP/2 gRPC endpoint such as `http://localhost:50051`.
5. Load the service's `.proto` files, choose a method, and send.

The browser sends a correctly encoded protobuf message in a binary gRPC-Web frame. Holy CORS forwards the same data frame using `application/grpc+proto` over HTTP/2, then converts native trailing metadata into the final gRPC-Web trailer frame. Unary and server-streaming methods are supported. Client-streaming and bidirectional streaming still require a native gRPC client because browser request streaming is not portable.

The bridge mode is explicit and versioned:

```text
X-Holy-Cors-Mode: grpc-native
Content-Type: application/grpc-web+proto
```

Existing gRPC-Web endpoints can still use normal proxy pass-through without that mode header.

## General HTTP usage

The target URL follows the bridge URL:

```text
http://127.0.0.1:2345/{TARGET_URL}
```

Examples:

```javascript
const user = await fetch(
  'http://127.0.0.1:2345/https://api.github.com/users/octocat'
).then(response => response.json());

const response = await fetch(
  'http://127.0.0.1:2345/https://httpbin.org/post',
  {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hello: 'world' })
  }
);
```

Query strings are preserved:

```text
http://127.0.0.1:2345/https://api.example.com/data?limit=25
http://127.0.0.1:2345/http://internal-api.local/health
```

## Allow another web app

Bug Days production and local development origins are included by default. Add other trusted origins explicitly:

```bash
holy-cors --allow-origin http://localhost:3000
holy-cors --allow-origin https://tools.example.com
```

Multiple values can be supplied by repeating the option. For an isolated development environment, you can opt into any origin:

```bash
holy-cors --allow-all-origins
```

Do not use `--allow-all-origins` around untrusted pages. A local browser bridge can reach services that public websites normally cannot.

## CLI reference

```text
Holy CORS! The local API bridge for Bug Days

Usage: holy-cors [OPTIONS]

Options:
  -p, --port <PORT>              Port to listen on [default: 2345]
      --allow-origin <ORIGIN>    Additional allowed origin; may be repeated
      --allow-all-origins        Allow every browser origin
  -v, --verbose                  Enable verbose request logging
      --bind <ADDRESS>           Bind address [default: 127.0.0.1]
  -h, --help                     Print help
  -V, --version                  Print version
```

Environment variables:

| Variable | Purpose | Native default |
|---|---|---|
| `HOLY_CORS_PORT` | Listening port | `2345` |
| `HOLY_CORS_BIND` | Listening address | `127.0.0.1` |
| `HOLY_CORS_ORIGINS` | Comma-separated additional origins | none |
| `HOLY_CORS_ALLOW_ALL` | Allow every origin | `false` |
| `HOLY_CORS_VERBOSE` | Verbose logging | `false` |

## Protocol support

| Protocol or behavior | Support |
|---|---|
| HTTP/1.1 and HTTPS proxying | Supported |
| HTTP/2 upstream connections | Supported |
| Native gRPC unary calls | Supported through bridge mode |
| Native gRPC server streaming | Supported through bridge mode |
| Existing binary gRPC-Web | Supported as pass-through |
| Client and bidirectional gRPC streaming | Not available from browser Fetch |
| SSE response streaming | Supported |
| SOAP over HTTP/S | Supported |
| System DNS and private/VPN DNS lookup | Supported |
| IPv4 and IPv6 PTR reverse DNS with forward confirmation | Supported |
| TLS certificate chain inspection on arbitrary direct-TLS ports | Supported |
| STARTTLS protocol negotiation | Not yet supported |
| WebSocket tunneling | Not yet supported |

## Security model

Holy CORS is intentionally a local developer utility, not a shared production gateway.

- It binds to loopback by default.
- Browser requests are checked against an origin allowlist.
- Private Network Access preflights are approved only after that origin check.
- HTTP proxy targets are limited to `http` and `https`; DNS and TLS endpoints accept validated hostnames, IPs, record types, ports, and bounded timeouts.
- It stores no request data and sends no telemetry.
- It provides no authentication or rate limiting of its own.

Anyone able to execute local programs as your user can already make direct network requests, so requests without a browser `Origin` header remain available to command-line tools. If you bind Holy CORS to a non-loopback address, protect that interface yourself.

## Contributing

Issues and pull requests are welcome. Please include a reproducible target or protocol fixture when reporting proxy behavior.

## License

MIT — see [LICENSE](LICENSE).

Built by [Bug Days](https://bugdays.com).
