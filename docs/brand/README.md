# Brand kit

| File | Use |
|---|---|
| `hefesto.logo.png` (900px) | horizontal logo — README header, documents, slides |
| `hefesto.logo-small.png` (420px) | inline/e-mail signature size |
| `hefesto.icon.png` (512px) | app icon, avatar, favicon source |
| `hefesto.icon-256.png` (256px) | small icon, GitHub organisation avatar |
| `hefesto.banner.png` (1200px) | the four-step flow: download → decrypt → build → deploy |
| `hefesto.features.png` (1200px) | capability strip: build, deploy, docs, secure, encrypted, in-memory, and the status icons |

Palette: gold `#C8973E` / `#E0B45C` on near-black `#0D0D0D`, circuit accents in blue `#2E7BD6`, forge flame orange `#F07B1D`.

The generated runbook uses `hefesto.logo-small.png` on its cover (embedded in the binary at compile time, see `src/runbook.rs`).
