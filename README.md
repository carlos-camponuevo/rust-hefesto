# 🔥 hefesto

Forge of the gods — in-memory build & deploy tool for the encrypted
`devops-*` docker stack repositories.

Everything happens in process memory: the repo is downloaded from Azure
DevOps as a zip into RAM, `.enc` files are decrypted in RAM (same scheme
as `ed.sh`: AES-256-CBC, PBKDF2-SHA256, 10 000 iterations), and nothing
plaintext ever touches disk.

## Usage

```sh
hefesto [config.json]      # default: ./hefesto.json
hefesto -git <url>         # no config file: derive everything from the repo URL
```

`-git` accepts the browser URL, the HTTPS clone URL, or the SSH clone URL:

```sh
hefesto -git https://dev.azure.com/BatDigitalI/Data%20Bridge/_git/devops-azcrpzanevla04
hefesto -git git@ssh.dev.azure.com:v3/BatDigitalI/Data%20Bridge/devops-azcrpzanevla04
```

Branch, PAT env var (`AZDO_PAT`) and folder exclusions use the defaults
shown below; use a config file when you need to override them.

Config (see `hefesto.example.json`):

```json
{
  "repo": {
    "organization": "BatDigitalI",
    "project": "Data Bridge",
    "repository": "devops-azcrpzanevla04",
    "branch": "main",
    "patEnv": "AZDO_PAT"
  },
  "excludeFolders": ["shared", "server", "xfiles"],
  "excludeSubfolders": ["config", "conf"]
}
```

- `patEnv` names the environment variable holding an Azure DevOps PAT
  (Code → Read). The token itself is never stored in config.
- `repo.localPath` (optional) loads a local checkout instead of
  downloading — for testing only.
- The decrypt key is prompted interactively, held only in zeroized
  memory, and never logged.

## Hostname gate

The repository name binds the deploy target: `devops-<host>` may only
deploy on the machine whose short hostname is `<host>` (case-insensitive).
Anywhere else the tool runs in build-only mode. This is a guardrail
against wrong-box deploys, not a security boundary.

## Roadmap

- [x] M1 — config, in-memory repo download, in-memory `ed.sh`-equivalent
      decrypt (openssl-compatible, unit-tested against real openssl output)
- [x] M2 — interactive navigation: environment → stack → compose services,
      with folder exclusions and the hostname gate
- [ ] M3 — `build.yml` per stack (overrides legacy `build.sh`), in-memory
      docker build via the Docker Engine API (tar context from RAM)
- [ ] M4 — deploy via `docker stack deploy --compose-file -` (stdin),
      registry login from env vars
- [ ] M5 — cross-platform release pipeline (macOS / Linux / Windows)

## Development

```sh
cargo test          # includes openssl-compat decrypt fixtures
cargo build --release
```

Regenerate the decrypt fixture with `tests/make_fixture.sh` (uses the
`openssl` CLI, passphrase `forge`).
