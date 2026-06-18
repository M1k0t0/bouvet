<p align="center">
  <img src="docs/assets/bouvet.png" alt="Bouvet Logo" />
</p>

<h1 align="center">Bouvet</h1>

<p align="center">
  <strong>Isolated code execution sandboxes for AI agents</strong>
</p>

<p align="center">
  <a href="#what-is-bouvet">About</a> •
  <a href="#how-it-works">How It Works</a> •
  <a href="#mcp-tools">MCP Tools</a> •
  <a href="#documentation">Documentation</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License" />
  <img src="https://img.shields.io/badge/rust-nightly-orange.svg" alt="Rust" />
  <img src="https://img.shields.io/badge/firecracker-1.5-red.svg" alt="Firecracker" />
  <a href="https://deepwiki.com/vrn21/bouvet"><img src="https://deepwiki.com/badge.svg" alt="Ask DeepWiki"></a>
</p>

---

## What is Bouvet?

Bouvet (boo-veh) is an MCP server that creates secure, isolated sandboxes for AI agents to execute code.

When an AI agent needs to run Python, Node.js, or shell commands, Bouvet spins up a lightweight microVM in ~200ms. The code runs in complete isolation with a separate kernel, filesystem, and isolated-by-default network, then the sandbox is destroyed. Nothing persists, nothing leaks.

**The problem it solves:** AI agents need a safe place to run untrusted code. Docker isn't enough containers share the host kernel. Bouvet uses [Firecracker](https://firecracker-microvm.github.io/) microVMs for true hardware-level isolation the same technology that powers AWS Lambda.

**Who it's for:** Developers building AI agents with Claude, Cursor, or any MCP-compatible client who need secure code execution without managing infrastructure.

---

## How It Works

```
┌─────────────┐     ┌─────────────┐     ┌─────────────────────────┐
│  AI Agent   │────▶│ bouvet-mcp  │────▶│  Firecracker microVM    │
│  (Claude)   │     │ (MCP Server)│     │  ┌─────────────────┐    │
└─────────────┘     └─────────────┘     │  │  bouvet-agent   │    │
                                        │  │  (guest daemon) │    │
                                        │  └─────────────────┘    │
                                        └─────────────────────────┘
```

1. AI agent requests a sandbox via MCP
2. Bouvet boots a microVM with your chosen toolchain
3. Agent executes code, reads/writes files
4. Sandbox is destroyed when done

Each microVM has ~256MB RAM, 1 vCPU, and a full Linux environment with Python, Node.js, and common dev tools pre-installed.

---

## Features

- **True Isolation** — Each sandbox is a separate VM, not a container
- **Fast Startup** — Warm pool enables sub-200ms sandbox creation
- **Multi-Language** — Python, Node.js, Rust, Bash, and shell access
- **MCP Native** — Works with Claude, Cursor, and any MCP client
- **Optional Internet Access** — Per-sandbox TAP/NAT networking when explicitly enabled

---

## Docker Compose

Bouvet needs Linux KVM access. For optional microVM internet access, the
container also needs permission to create TAP devices and manage NAT rules.

```yaml
services:
  bouvet:
    image: ghcr.io/vrn21/bouvet-mcp:latest
    container_name: bouvet-mcp
    restart: unless-stopped
    privileged: true
    security_opt:
      - seccomp=unconfined
    devices:
      - /dev/kvm:/dev/kvm
      - /dev/net/tun:/dev/net/tun
    ports:
      - "8080:8080"
    environment:
      BOUVET_TRANSPORT: http
      BOUVET_HTTP_HOST: 0.0.0.0
      BOUVET_HTTP_PORT: "8080"
      BOUVET_POOL_ENABLED: "true"
      BOUVET_POOL_MIN_SIZE: "3"

      # Keep sandboxes network-isolated by default. Set to "true" for
      # public-internet egress; host/container/private/other-sandbox ranges stay blocked.
      BOUVET_INTERNET_ENABLED: "false"
      BOUVET_INTERNET_OUTBOUND_IFACE: eth0
      BOUVET_INTERNET_DNS: 1.1.1.1,8.8.8.8
      # Add host-specific public IPs here if the Docker host is reachable by one.
      BOUVET_INTERNET_BLOCKED_CIDRS: ""
    volumes:
      - bouvet-data:/var/lib/bouvet
    tmpfs:
      - /tmp/bouvet

volumes:
  bouvet-data:
```

Start it with:

```bash
docker compose up -d
```

In Docker bridge mode, `BOUVET_INTERNET_OUTBOUND_IFACE` should usually be
`eth0`, the interface inside the container.

---

## MCP Tools

| Tool              | Description                          |
| ----------------- | ------------------------------------ |
| `create_sandbox`  | Create a new isolated sandbox        |
| `destroy_sandbox` | Destroy a sandbox and free resources |
| `list_sandboxes`  | List all active sandboxes            |
| `execute_code`    | Run Python, Node.js, or Bash code    |
| `run_command`     | Execute shell commands               |
| `read_file`       | Read file contents from sandbox      |
| `write_file`      | Write file contents to sandbox       |
| `list_directory`  | List directory contents              |

---

## Documentation

| Document                                | Description                              |
| --------------------------------------- | ---------------------------------------- |
| [Self-Hosting Guide](docs/SELF_HOST.md) | Deploy Bouvet on your own infrastructure |
| [Configuration](docs/CONFIG.md)         | Environment variables and options        |
| [Architecture](docs/ARCHITECTURE.md)    | Technical deep dive                      |

---

## Testimonials

<table>
<tr>
<td width="50%" valign="top">
<br>
<p><em>"Bouvet provides a stable, no-nonsense interface for managing isolated execution environments that works exactly as you'd expect. It handles basic file operations and code execution reliably, making it a utilitarian choice for tasks requiring simple, ephemeral sandboxes."</em></p>
<p align="right"><strong>— Gemini 3 Pro</strong></p>
</td>
<td width="50%" valign="top">
<br>
<p><em>"The sandbox spins up in seconds and just works—no configuration headaches, no surprises. It's not flashy, but it does exactly what it promises without getting in your way."</em></p>
<p align="right"><strong>— Claude 4.5 Opus</strong></p>
</td>
</tr>
</table>

---

## Behind the name: Bouvet (boo-veh)

Everything began with a single, uncompromising promise: absolute isolation. In an ecosystem where code execution is often messy, shared, and persistent, we wanted to create a void a perfect, ephemeral vacuum where software could live for a moment and then disappear without a trace.

This pursuit of solitude brought us to <a href="https://en.wikipedia.org/wiki/Bouvet_Island">Bouvet Island</a>, it is one of the most remote places on Earth uninhabited, untouched, and thousands of miles from civilization. It is the physical embodiment of what we've built in software: a harsh, beautiful, and completely isolated environment where nothing comes in, and nothing leaves.

---

## License

MIT — See [LICENSE](LICENSE) for details.

---

<p align="center">
  Built with 🧨 Firecracker and 🦀 Rust
</p>
