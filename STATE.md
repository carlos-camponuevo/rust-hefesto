# hefesto — current state

A single static Rust binary that builds images, deploys Docker Swarm stacks and
generates platform documentation, reading everything from an encrypted
`devops-*` repository **in memory**. Version `0.1.0` in `Cargo.toml`; released as
`v0.1.4`. ~4,300 lines across 11 modules, 22 tests passing.

---

## 1. What it does today

| Mode | Command | Behaviour |
|---|---|---|
| Interactive | `hefesto [config]` | Arrow-key navigation: environment → stack → image → service → action |
| Build | `hefesto -build [env/stack[/image\|service]]` | Builds (and pushes) one image, a whole stack, or drops into BUILD-mode menus |
| Deploy | `hefesto -deploy [env/stack[/service]]` | Deploys a whole stack or selected services |
| Runbook | `hefesto -runbook [out-dir]` | Generates Markdown + HTML + PDF platform documentation and mails it |
| Config from URL | `hefesto -git <url>` | Derives all config from an Azure DevOps or GitHub repo URL |

Without `-build`/`-deploy` the mode is chosen by the host gate: **DEPLOY** on the
repository's own host, **BUILD** everywhere else.

### Run sequence

1. Load config (YAML or JSON; the file itself may be `ed.sh`-encrypted).
2. Download the deployment repository as a zip **into RAM** (Azure DevOps or
   GitHub), or read a local directory in test mode (`repo.localPath`).
3. Decrypt every `.enc` entry in memory, prompting for the passphrase (3 attempts;
   the config's own key is reused if it opens the repo too).
4. Execute the requested action; stream output live; capture it for the report.
5. Mail an HTML report, or say explicitly why it didn't.

---

## 2. Architecture

```
main.rs      argument parsing, run sequence, mode selection
config.rs    Config/Repo/MailCfg, git-URL parsing (azdo + github), host gate,
             encrypted-config loading, default config discovery
remote.rs    repo download to RAM: Azure DevOps items?$format=zip, GitHub zipball;
             PAT from env var, else the machine's stored git credentials
vault.rs     MemFs: path -> bytes map; openssl-compatible decryption
             (AES-256-CBC, PBKDF2-HMAC-SHA256, 10k iterations, "Salted__")
nav.rs       interactive navigation, image grouping, orchestration of runs,
             mail routing for build reports
ui.rs        custom crossterm picker: ↑↓ move, →/enter select, ← back, type to
             filter, indented breadcrumb of the path already chosen
build.rs     build.yml schema, legacy build.sh parsing, registry login,
             in-memory build contexts, pty-backed output capture, log cleanup
deploy.rs    compose preparation in memory (env files folded in), stack deploy
runbook.rs   collector (compose + build.yml + stack.md) and renderers (md/html/pdf)
report.rs    shared HTML e-mail components (header, facts, cards, log blocks)
mail.rs      multipart HTML mail, inline logo, attachments, proxy-aware sending
```

### Data flow

- **Repository → RAM.** `MemFs` holds the whole repo as `path -> Vec<u8>`.
  `.enc` entries are replaced by their plaintext in the map; `.enc.mode` files are
  dropped. Nothing is written to disk. A wrong passphrase leaves the map intact so
  the retry works.
- **Build.** The *application* repository is downloaded to RAM, converted to a tar
  stream and piped into `docker build --pull --platform <p> -t <ref> -f <dockerfile> -`.
  Push follows if `push: true`. The image digest is parsed from the push output
  (falling back to the build's `writing image sha256:…`).
- **Deploy.** Each service's `env_file:` list is resolved against the in-memory
  vault and folded into `environment:`, preserving compose precedence (later files
  win; explicit `environment:` wins over all) and escaping `$` as `$$`. The
  self-contained compose is piped to
  `docker stack deploy --resolve-image always --with-registry-auth --detach=false -c - <stack>`.
- **Runbook.** Merges three sources per stack: `docker-compose.yml` (facts),
  `build.yml` (provenance), `stack.md` (prose). Emits Markdown, print-ready HTML
  and — when a Chrome/Chromium binary exists — a PDF with bookmarks.

### Naming and layout conventions

- Stack directory `zauat/admin` → swarm stack `zauat-admin`; a root-level folder
  with its own compose (`system`, `systools`) is itself a stack.
- Environment folder name encodes market and environment: `bruat` → `BR UAT`.
- Services are grouped by **image**, so an image used by ten services builds once.

---

## 3. Configuration

### hefesto config (YAML or JSON, optionally encrypted)

```yaml
repo:
  url: https://dev.azure.com/<org>/<project>/_git/devops-<hostname>   # or org/project/repository
  branch: main
  patEnv: AZDO_PAT
  localPath: /path/to/checkout        # test mode only
excludeFolders: [shared, server, xfiles]
excludeSubfolders: [config, conf]
mail:
  to: [ops@company.com]
  from: noreply@company.com
```

Default lookup order: `hefesto.yml` → `hefesto.yaml` → `hefesto.json`, each also
tried with a `.enc` suffix.

### build.yml (per environment or per stack)

```yaml
destinations:
  ghcr: { host: ghcr.io, namespace: souza-cruz, user: Souza-Cruz }
defaultPlatform: linux/amd64
mailGroups:
  devops: [a@x.com, b@x.com]
repoList:
  - name: Admin Portal
    repoUrl: https://dev.azure.com/<org>/<project>/_git/admin-portal
    branch: master
    image: admin-portal
    tag: br.master.latest
    destination: ghcr
    mailGroup: devops
    # enabled: false      # documented in the runbook, never built
    # platform: linux/arm64
    # dockerfile: DockerfileGitHub
    # repoCloneUrl: …     # accepted, not implemented
```

Resolution order: `<env>/<stack>/build.yml` → `<env>/build.yml` (environment
catalog) → `<env>/<stack>/build.sh` (legacy `repoList` parsing).

### Credentials — never in files

| Purpose | Source |
|---|---|
| Repo access | `AZDO_PAT` / `GITHUB_TOKEN`, else `git credential fill` for the host |
| Registry login | `user:` in the destination (not secret); token from `DOCKER_PAT` / `GHCR_PAT` |
| SMTP | `SMTP_HOST`, `SMTP_USER`, `SMTP_PASS`, optional `SMTP_PORT` (default 587) |
| Repo decryption | prompted interactively, held in `Zeroizing<String>` |
| Mail recipients without a config | `HEFESTO_MAIL_TO=a@x,b@y` |

---

## 4. Decisions settled

**In-memory only.** The repository, its decrypted contents, build contexts and
the prepared compose never touch disk. Build contexts go to docker as a tar on
stdin; compose goes to `docker stack deploy` on stdin. The only files written are
requested outputs (runbook) and a transient 0600 message file in `/dev/shm` when
mailing through a proxy.

**Docker CLI, not the Engine API.** Shelling out to `docker` keeps output
identical to what an operator sees and avoids an API client dependency. A
pseudo-terminal is allocated so docker renders its live progress display; through
a plain pipe it degrades to repeated static lines.

**`--resolve-image always` on deploy.** Without it Swarm keeps the digest already
pinned in the service spec, so redeploying a moved `*.latest` tag silently ships
the previous image.

**`linux/amd64` by default.** The swarm nodes are x86_64. An explicit `--platform`
means a build on an ARM host fails loudly instead of pushing an image the servers
cannot run.

**`DockerfileGitHub` preferred when present.** The plain `Dockerfile` in these
application repos expects artifacts compiled outside docker; the `GitHub` variant
is self-contained, which is what an in-memory context requires. An explicit
`dockerfile:` in `build.yml` always wins.

**Images are the build unit, services are the deploy unit.** Menus group compose
services by image; building an image once covers every service that runs it.

**Host gate.** `devops-<hostname>` may only be deployed on the machine whose short
hostname matches. A guardrail against wrong-box deploys, not a security boundary.

**Commented-out `repoList` entries are disabled builds** and are never imported
from legacy `build.sh`; `enabled: false` expresses the same thing in YAML while
keeping the entry visible in the runbook.

**Documentation lives in `.md`.** `ed.sh` encrypts `.env`, `.sh`, `.yml`, `.yaml`
and `.py` — not `.md` — so `stack.md` and `docs/00-platform.md` stay readable in
the repository browser while build configuration stays encrypted.

**Mail is best-effort and never blocks.** Direct SMTP has a 20-second timeout, the
proxied path 40 seconds. Every run states which happened: mailed, failed with the
reason, or not configured.

**Proxy-aware mail.** SMTP cannot cross an HTTP proxy, so when `HTTPS_PROXY`,
`HTTP_PROXY` or `ALL_PROXY` is set the message is handed to `curl`, which opens a
CONNECT tunnel and performs STARTTLS and authentication inside it. Credentials
reach curl through a config file on stdin, never the command line.

**Report shape.** Environment, stack and status first; a card per image with
source, platform and digest; captured terminal output last on a dark background.
Multipart HTML with a plain-text alternative; the logo travels as an inline
`cid:` attachment because Gmail and Outlook drop `data:` URIs.

**The public repository names no customer.** README, examples and tests use
`<organization>` / `company.com` placeholders. The one functional exception is
`default_namespace() = "camponuevo"`, needed by the legacy `build.sh` fallback.

**Releases.** Built on an x86_64 Ubuntu host from a clean checkout of the pushed
commit, published as a single static musl binary plus its `.sha256`. One release
at a time — older ones are deleted.

---

## 5. Constraints

- **Deploy hosts accept no inbound connections** and reach the outside only
  through an HTTP proxy. Anything requiring an open port or a raw TCP protocol
  (SMTP, Postgres, Redis) will not work from them without a CONNECT tunnel.
- **`docker` must be present** for build and deploy; **`curl`** for proxied mail;
  **Chrome/Chromium** only for the runbook PDF (Markdown and HTML always render).
- **The passphrase is interactive.** There is no unattended path today — a human
  types it, and it is never stored.
- **Ports 80/443 belong to Traefik** on the platform hosts; anything else exposes
  itself as a Traefik router, not by binding those ports.
- **Traefik runs with `forwardedHeaders.insecure=true`**, so IP-based allow lists
  at the Traefik layer are spoofable; network restrictions belong in the NSG.
- **The `system` stack's entrypoint redirect owns port 80** at priority
  MaxInt64-1, which no user router can outrank (user priority is capped at
  MaxInt64-1000).
- Registry namespaces are lowercase; login users are case-sensitive
  (`souza-cruz` vs `Souza-Cruz`).
- Azure DevOps project names arrive percent-encoded in URLs (`Data%20Bridge`).

---

## 6. Tried and rejected

**Docker Engine API via bollard** — rejected for the build/deploy path. Shelling
out to the CLI gives identical output to an operator's own commands and avoids
reimplementing progress rendering.

**An OS ramdisk (`hdiutil`/tmpfs) for the decrypted repo** — rejected. Process
memory is simpler, portable to Windows, and not visible to other processes.

**`inquire::Select` for navigation** — replaced by a custom crossterm picker,
because inquire cannot remap keys and ← for "back" is the whole point.

**Truncating over-long secret names** — rejected. Names that would exceed Swarm's
64-character limit are reported and left unconverted, so nobody silently deploys
a differently-named secret.

**Router priority to escape the port-80 redirect** — rejected after testing:
Traefik caps user-defined priority below the entrypoint redirect's, so the router
is dropped entirely with an error. The supported lever is the redirect's own
`priority` setting.

**`lettre` for mail on proxied hosts** — rejected there (kept for direct hosts).
It cannot open a CONNECT tunnel, so proxied hosts use curl.

**XLSX as a runbook output** — dropped after review; hard to read. Markdown, HTML
and PDF remain.

**Data URIs for the report logo** — rejected; Gmail and Outlook refuse them.

**Committing the devops repositories** — out of scope for this tool and this
assistant: their working trees stay local and their owner commits them.

---

## 7. Known gaps

- **Secrets parity.** The legacy `pull_deploy_ds.sh` converted credential env
  files into Docker secrets plus an entrypoint wrapper. hefesto folds all values
  into `environment:`, which lands them in the service spec — equivalent to the
  plain `pull_deploy.sh`, weaker than the `_ds` variant.
- **No pre-deploy validation**: external networks are not checked for existence,
  duplicate network entries are not rejected, image tags are not verified in the
  registry before rolling.
- **No rollback.**
- **`repoCloneUrl` is parsed but not implemented** — repositories that build from
  a working tree (commerceiq, medusa) still use their `build.sh`.
- **Single target per run.** Batches require repeated invocations, producing one
  report each.
- **No machine-readable output.** Nothing emits JSON for an inventory, a plan or
  a result.
- **No unattended credential path** (see constraints).
- **Linux x86_64 only** in releases; macOS arm64 and Windows are unbuilt.
- **The interactive breadcrumb rendering has not been verified against a real
  terminal session** — it compiles and is wired through every level, but no
  keystroke-driven check has been run.

---

## 8. Where it runs

| Host | Role | Notes |
|---|---|---|
| `euirdocker15.ipremios.com.br` | build machine | x86_64 Ubuntu, Docker 26. `/apps/sysdata/hefesto/{source,repo,hefesto,*.yml}`; `source/` is the GitHub clone, `repo/` the build tree, `sudo ./build.sh` builds and installs to `/usr/local/bin` and the runtime copy. No Chrome, so runbook PDFs are skipped there. |
| Azure platform hosts (`AZCR*`) | deploy targets | Behind the corporate proxy, no inbound SSH from outside, reachable through Citrix + bastion. hefesto is installed from the GitHub release. |
| `server.uat.ar.batmicroservices.com` | retired build box | aarch64; produced the arm64 images that prompted the `--platform` default. |

Source of truth: **github.com/carlos-camponuevo/rust-hefesto** (public, branch
`master`). Brand assets in `docs/brand/`; the logo is compiled into the binary for
runbook covers and report mail.
