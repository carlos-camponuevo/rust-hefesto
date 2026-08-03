# 🔥 hefesto

Forge of the gods — an in-memory build, deploy and documentation tool for
encrypted `devops-*` Docker Swarm repositories.

Everything happens in process memory: the deployment repository is
downloaded as a zip into RAM, its encrypted files are decrypted in RAM
(AES-256-CBC + PBKDF2-SHA256, the same scheme as the repositories'
`ed.sh`), Docker build contexts are streamed from RAM, and compose files
are piped to `docker stack deploy` through stdin. **No plaintext is ever
written to disk.**

A single static binary, no runtime dependencies: `cargo build --release`
produces one file you can copy to any machine of the same architecture.

## Usage

```sh
hefesto [config.yml]                          # interactive; default ./hefesto.yml|.yaml|.json
hefesto -git <url>                            # no config file — derive it from a repo URL
hefesto -build  [env/stack[/image|service]]   # BUILD mode; with a target: build and exit
hefesto -deploy [env/stack[/service]]         # DEPLOY mode; with a target: deploy and exit
hefesto -runbook [out-dir]                    # generate (and mail) the platform runbook
```

Without `-build`/`-deploy` the mode is automatic: **DEPLOY** on the
repository's own host (see *Host gate*), **BUILD** everywhere else.

### Configuration

YAML or JSON (JSON is valid YAML, so both parse):

```yaml
repo:
  url: https://dev.azure.com/<organization>/<project>/_git/devops-<hostname>
  # or: organization / project / repository, plus optional branch, patEnv
excludeFolders: [shared, server, xfiles]      # hidden from navigation
excludeSubfolders: [config, conf]
mail:
  to: [ops@company.com]
  from: noreply@company.com
```

The config file itself may be encrypted with the same `ed.sh` scheme —
hefesto detects the `Salted__` header, prompts for the key, decrypts it in
memory, and reuses that key for the repository so you are asked once.

### Credentials

Never stored in files. Resolution order:

| Purpose | Source |
|---|---|
| Repository access | `AZDO_PAT` / `GITHUB_TOKEN`, else the machine's stored git credentials |
| Registry login | `user:` in the destination (not secret), token from `DOCKER_PAT` / `GHCR_PAT` |
| SMTP | `SMTP_HOST`, `SMTP_USER`, `SMTP_PASS` |
| Repository decryption | prompted interactively, held in zeroized memory |

## Host gate

The repository name binds the deploy target: `devops-<hostname>` may only
be deployed on the machine whose short hostname is `<hostname>`
(case-insensitive). Everywhere else hefesto refuses to deploy and runs in
build-only mode. It is a guardrail against wrong-box deploys, not a
security boundary.

## Build definitions

Each environment (or stack) declares what to build in `build.yml`. If no
`build.yml` exists, hefesto falls back to parsing the legacy `build.sh`
`repoList` entries, so existing stacks work with no new files. Resolution
order: `<env>/<stack>/build.yml` → `<env>/build.yml` (a catalog shared by
the environment's stacks) → `<env>/<stack>/build.sh`.

```yaml
destinations:
  registry:  { host: ghcr.io,   namespace: my-org,  user: My-Org }   # namespace lowercase,
  dockerhub: { host: docker.io, namespace: my-user, user: my-user }  # user case-sensitive
defaultPlatform: linux/amd64        # swarm nodes are x86_64; never build the wrong arch silently
mailGroups:
  platform: [ops@company.com]
repoList:
  - name: Admin Portal              # friendly name for menus and reports
    repoUrl: https://dev.azure.com/<org>/<project>/_git/admin-portal
    branch: master
    image: admin-portal             # defaults to the repository name
    tag: prod.latest
    destination: registry
    mailGroup: platform             # omit to send no mail for this entry
    # enabled: false                # documented in the runbook, never built
    # platform: linux/arm64         # per-entry override
    # dockerfile: Dockerfile        # `DockerfileGitHub` is preferred automatically when present
```

Services sharing one image are built **once**: hefesto groups the compose
services by image, so an image used by ten services is a single build.

## Runbook

`hefesto <config> -runbook` merges three sources into a platform document:

| Source | Contributes |
|---|---|
| `docker-compose.yml` | services, images, networks, volumes, secrets, routing, replicas, ports, env files |
| `build.yml` | source repository, branch, tag, registry |
| `stack.md` (per stack) | human descriptions — the only hand-written part |

It writes **Markdown**, print-ready **HTML** and, when a Chrome/Chromium
binary is available, a **PDF** with bookmarks; then mails them as
attachments if mail is configured. Because the facts are read from the
repository at generation time, the document cannot drift from what is
deployed. Appendices list every published endpoint, the build catalog, and
the services still missing a description.

`stack.md` is plain Markdown with optional front matter; sections are keyed
by service name, or by `image:<name>` for text shared by every service
running that image:

```markdown
---
market: XX
environment: PROD
owner: Platform team
---
# <stack name>

What this stack does.

## <service-name>
What this service does.
```

`ed.sh` encrypts `.env`, `.sh`, `.yml`, `.yaml` and `.py` — **not** `.md` —
so documentation stays readable in the repository browser while build
configuration stays encrypted.

## Development

```sh
cargo test           # includes fixtures verifying openssl-compatible decryption
cargo build --release
./build.sh           # server-side: pull, build in Docker, install to /usr/local/bin
```

Regenerate the decrypt fixture with `tests/make_fixture.sh` (passphrase `forge`).

## Roadmap

- [x] Config (file, URL or encrypted), in-memory repository download, in-memory decryption
- [x] Interactive navigation: environment → stack → image → service, with a host gate
- [x] Builds: `build.yml` / legacy `build.sh`, in-memory contexts, live output, registry push
- [x] Deploys: env files folded into the compose in memory, piped to `docker stack deploy`
- [x] Mailed build/deploy reports with captured logs
- [x] Runbook generation (Markdown, HTML, PDF) with mail delivery
- [ ] `repoCloneUrl`: full clone instead of a zip snapshot, for repositories built from a working tree
- [ ] Cross-platform release pipeline (macOS, Linux, Windows)
