# Catalog design

ComfyBox separates reusable files from installable products.

## Artifact

An artifact is one concrete downloadable file and its intended ComfyUI path.

```toml
[[artifacts]]
id = "example.unet.bf16"
name = "Example UNet BF16"
url = "https://huggingface.co/org/repo/resolve/main/example.safetensors"
relative_path = "models/diffusion_models/example.safetensors"
size_bytes = 123456789
sha256 = "..."
```

`size_bytes` and `sha256` are optional, but strongly recommended for large files.

## Package

A package references artifacts rather than duplicating their metadata.

```toml
[[packages]]
id = "example-model"
name = "Example Model"
family = "Example"
primary_artifact_ids = ["example.unet.bf16"]
dependency_artifact_ids = ["shared.text.encoder", "shared.vae"]
```

This allows the same VAE or text encoder to be shared by multiple packages.

## Discovery-only package

For fast-changing ecosystems, do not guess download paths:

```toml
[[packages]]
id = "example-family"
name = "Example family"
family = "Example"
discover_query = "Example model"
```

A package with no primary artifacts is treated as discovery-only.

## External catalogs

Pass one or more extra files:

```bash
comfybox --catalog ./private.toml models list
comfybox --catalog ./private.toml catalog validate
```

Files in the app config `catalog.d` directory are also loaded automatically. Later catalog entries override earlier entries with the same ID.
