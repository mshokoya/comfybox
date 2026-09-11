# ComfyBox

ComfyBox is a Rust CLI for installing, discovering, starting, stopping, and safely managing ComfyUI models, dependencies, workflows, and custom nodes.

It is designed for large AI model files where a failed download, wrong target path, or unsafe dependency removal can waste tens of gigabytes.

## Architecture

```text
comfybox-rust/
├── crates/
│   ├── comfybox-core/   # catalog, downloader, inventory, workflow, process, storage
│   └── comfybox-cli/    # clap commands + interactive terminal UX
├── assets/
│   ├── catalog/builtin.toml
│   └── workflows/
├── docs/
└── examples/
```

The model registry is declarative. Reusable files are **artifacts**, installable bundles are **packages**, and workflows reference known artifacts/custom nodes by ID.

## Main capabilities

- Auto-detect ComfyUI installations and let the user browse to one if detection fails.
- Recommend a mounted volume with substantial free space for new ComfyUI installs.
- Install ComfyUI through a temporary directory and publish it to the chosen destination only after clone succeeds.
- Create Python dependency wheelhouses in temporary storage, install from them, then remove temporary files.
- Download very large models with HTTP byte ranges, retries, resumable range metadata, sparse `.part` files, size checks and optional SHA-256 verification.
- Keep resumable model temp state on failure; clean it after a verified successful install.
- Place model temp state beside the final target directory so final publication remains on the same filesystem even when `models/` is a separate mount or symlink target.
- Detect installed, partial, missing, and size-mismatched artifacts.
- Install packages with primary models, shared dependencies, and optional groups.
- Inspect arbitrary ComfyUI workflow JSON and resolve known model filenames to catalog artifacts.
- Refuse to invent URLs for unknown workflow dependencies.
- Reference-aware dependency removal.
- Background ComfyUI start/stop/status with PID validation.
- External TOML/JSON catalog overrides.
- `HF_ENDPOINT` mirror support and `HF_TOKEN` support.
- Full-screen terminal dashboard in addition to deterministic subcommands.

## Build

Rust 1.85+ is recommended.

```bash
cargo build --release
./target/release/comfybox --help
```

Install locally:

```bash
cargo install --path crates/comfybox-cli
```

## Interactive mode

```bash
comfybox
```

The native terminal dashboard is built with Ratatui and Crossterm. It opens when no
subcommand is supplied and provides seven views:

- **Overview** — ComfyUI readiness, server state, artifact health, storage and setup warnings.
- **Models** — installable model packages and their required model, text encoder, VAE, LoRA and custom-node dependencies.
- **Workflows** — bundled workflows with aggregate dependency health and one-key installation.
- **Downloads** — live download manager with queued/active/paused/failed jobs, byte progress, speed, ETA, stop and resumable restart controls.
- **Logs** — persistent lifecycle and error logs; high-frequency byte-progress events are intentionally filtered out.
- **Settings** — tune concurrent files and parallel chunks per file while downloads are running.
- **System** — configured paths, Python/Git availability, `HF_TOKEN`, disk space and managed state.

Use `←`/`→` or `Tab` to change views, `↑`/`↓` or `j`/`k` to select,
`Enter` to install a package/workflow or focus a download. In Downloads, use `x`
to pause, `c` to continue a paused download, `r` to retry a failed download, and
`b`/`Esc` to return a focused transfer to the
background. In Settings, `-`/`+` changes concurrent files and `[`/`]` changes
parallel chunks per file. Lowering the file limit lets existing transfers finish
and holds queued jobs until capacity is available. Global keys
are `s` start/stop, `l` locate, `i` install ComfyUI, `p` install Python
dependencies, `t` securely save or replace the Hugging Face token, `r` refresh
and `q`/`Ctrl-C` quit. Terminals smaller than 72 × 20
show a resize prompt instead of a broken layout.

The queue is saved under the ComfyBox config directory. Closing ComfyBox pauses
in-process transfers; unfinished jobs return as paused on the next launch and can
resume from verified HTTP ranges. “Background” means the transfer continues while
you use another dashboard tab—it is not a detached operating-system service.

## Common commands

```bash
# environment and config
comfybox doctor
comfybox locate
comfybox locate --path /autodl-fs/data/minimax-h3/ComfyUI

# install ComfyUI
comfybox install
comfybox install -d /autodl-fs/data/minimax-h3
comfybox install -d /autodl-fs/data --source gitee
comfybox install -d /data --source github
comfybox python-deps

# models
comfybox models list
comfybox models install minimax-h3-bf16
comfybox models install krea2-bf16
comfybox models install qwen-image-2512-bf16
comfybox models remove minimax-h3-bf16
comfybox models remove minimax-h3-bf16 --with-deps

# changing / broad model families are discoverable without unsafe guessing
comfybox models discover "black-forest-labs BF16"
comfybox models discover SDXL --limit 50
comfybox models discover "Wan Animate"

# workflows
comfybox workflow list
comfybox workflow inspect minimax-h3-reference
comfybox workflow inspect --file ./workflow.json
comfybox workflow install-deps --file ./workflow.json
comfybox workflow remove-deps --file ./workflow.json
comfybox workflow install minimax-h3-reference --with-deps

# ComfyUI lifecycle
comfybox start
comfybox status
comfybox stop

# config
comfybox config show
comfybox config set-hf-endpoint https://hf-mirror.com
comfybox config set-hf-token
comfybox config set-hf-token --from-env
printf '%s\n' "$HF_TOKEN" | comfybox config set-hf-token --stdin
comfybox config clear-hf-token

# cleanup / removal
comfybox temp-clean
comfybox uninstall
```

## AutoDL / Hugging Face mirror

Interactive ComfyUI installation asks whether to clone the official GitHub
repository or the `https://gitee.com/mirrors/comfyui` China mirror before it
selects a destination or creates temporary download directories. For unattended
installs, use `--source gitee` or `--source github`. Failed clones are cleaned and
retried, with HTTP/1.1 used after the first failure.

Before an interactive model, workflow-dependency, or individual-artifact install,
ComfyBox asks whether all Hugging Face requests in that install plan should use
the China mirror (`https://hf-mirror.com`) or official Hugging Face. The selected
endpoint is applied to metadata checks, sequential and ranged downloads, retries,
and resumed chunks. Non-Hugging-Face URLs are left unchanged.

For non-interactive runs, configure the endpoint once or export it:

```bash
comfybox config set-hf-endpoint https://hf-mirror.com
# or
export HF_ENDPOINT=https://hf-mirror.com
comfybox models install minimax-h3-bf16
```

For gated Hugging Face repositories, either use the standard environment variable:

```bash
export HF_TOKEN='hf_...'
```

or press `t` in the dashboard / run `comfybox config set-hf-token`. The prompt is
masked and the value is saved separately from `config.json` and `state.json`. On
Unix, the credential file is created with mode `0600` in a mode `0700`
configuration directory. `--stdin` is available for automation without exposing
the token in the command line or process list.

At startup, ComfyBox resolves credentials in this order: a token saved during the
current dashboard session, `HF_TOKEN`, `HF_TOKEN_PATH`, the ComfyBox credential
file, `HF_HOME/token`, then Hugging Face's standard cache locations. When a saved
or cached token is found, ComfyBox sets `HF_TOKEN` inside its own process before
the async runtime and requested command start. A child process cannot change its
parent shell's environment, so ComfyBox deliberately does not edit shell startup
files; the saved credential is global to future ComfyBox runs instead.

The dashboard explicitly reports a missing token. Public repositories remain
usable; artifacts marked as gated are shown as `TOKEN` until a token is available.
HTTP 401/403 failures distinguish a
missing token from an invalid token or unaccepted repository terms.

## Resumable model downloads

A model download uses a temporary structure beside the final target directory:

```text
ComfyUI/models/<folder>/.comfybox-tmp/<artifact-id>/
├── <filename>.part
└── ranges.json
```

`ranges.json` is the source of truth for completed byte ranges. The sparse `.part` file's logical length is not treated as download progress.

On successful verification the `.part` file is atomically renamed to the final model path and the artifact temp directory is removed. If a ranged transfer fails, completed ranges remain recorded so a later install can resume them.

Catalog paths are always resolved relative to the configured ComfyUI root, so a
successful download is automatically published to its declared folder (for
example `models/diffusion_models`, `models/text_encoders`, `models/vae`,
`models/loras`, or `models/upscale_models`). Interrupted data remains in the
sibling `.comfybox-tmp` directory; it is never mistaken for an installed model.
Servers with no byte-range support are safely retried from the beginning.

## Catalog strategy

The built-in catalog intentionally distinguishes between:

1. **Curated one-click packages** whose ComfyUI paths and dependencies are explicitly mapped.
2. **Discovery-only families** where the ecosystem changes too quickly to safely guess exact files/dependencies.

This is intentional. A repository name is not enough information to decide whether a file belongs in `checkpoints`, `diffusion_models`, `text_encoders`, `vae`, `loras`, or somewhere else.

See `docs/CATALOG.md`.

## Current initial catalog

The initial manifest includes curated entries for MiniMax H3 BF16, Krea 2 Raw/Turbo BF16, Z-Image Base/Turbo BF16, Qwen Image BF16/2512, Qwen Image Edit 2509/2511, Ideogram 4's current Comfy package, SCAIL-2's primary model, SDXL Base/Refiner, plus discovery entries for broader BFL, SDXL, Wan/Wan Animate and LTX families.

The catalog is data. Adding a new package should normally require only manifest changes rather than downloader/process code changes.

## Important safety behavior

- Existing mismatched targets are refused unless `--force` is used.
- A forced replacement is downloaded and verified before the existing file is replaced.
- Shared managed dependencies are retained during uninstall.
- ComfyBox also performs a conservative catalog/filesystem check before removing dependencies used by another installed package.
- Unknown workflow filenames are reported, not guessed.
- A stale PID will not be blindly killed if it no longer resembles the configured ComfyUI process.
- Model downloads keep resumable temp state on network failure.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The workspace is validated with `cargo check`, strict Clippy, and its test suite.

## License

MIT. Upstream models and custom nodes retain their own licenses.

## Create Linux Binary

build ComfyBox:
```bash
    cd /Users/msho/Downloads/comfybox-rust

    cargo zigbuild \
    --release \
    --target x86_64-unknown-linux-gnu.2.17
```

The Linux binary will be:
```bash
    target/x86_64-unknown-linux-gnu/release/comfybox
```

## Download & install

Use this one-liner to download it, make it executable, and install it globally as comfybox:
```bash
curl -fL --retry 5 --retry-delay 3 \
  -o /tmp/comfybox \
  "https://github.com/mshokoya/comfybox/releases/download/x86_64-unknown-linux-gnu/comfybox" \
  && chmod +x /tmp/comfybox \
  && sudo install -m 0755 /tmp/comfybox /usr/local/bin/comfybox \
  && rm -f /tmp/comfybox \
  && comfybox --version
```

If you're logged in as root on AutoDL, you don't need sudo:
```bash
curl -fL --retry 5 --retry-delay 3 \
  -o /tmp/comfybox \
  "https://github.com/mshokoya/comfybox/releases/download/x86_64-unknown-linux-gnu/comfybox" \
  && chmod +x /tmp/comfybox \
  && install -m 0755 /tmp/comfybox /usr/local/bin/comfybox \
  && rm -f /tmp/comfybox
```

Then you can run:

```bash
    comfybox
```
