# Download engine

The downloader is optimized for very large safetensors/model files.

## Ranged downloads

When the server advertises HTTP byte-range support:

1. Determine expected size using the catalog and/or `Content-Length`.
2. Create a same-filesystem temporary directory beside the final target.
3. Create one sparse `.part` file.
4. Split the file into logical byte ranges.
5. Download missing ranges concurrently.
6. Seek/write each completed range into the single `.part` file.
7. Persist `ranges.json` after each completed range.
8. Verify final byte length.
9. Verify SHA-256 when a hash is supplied.
10. Rename the verified file into its final location.
11. Remove the artifact temp directory.

A failed transfer leaves `.part` and `ranges.json` intact so the next run can retry only missing ranges.

## Why progress does not come from `.part` size

Sparse files are pre-sized to the final logical file length. Their reported length can therefore equal the model's full size before any meaningful data has been downloaded. Only `ranges.json` is considered resume progress.

## No range support

If a server does not support byte ranges, ComfyBox falls back to a sequential download. It cannot safely resume arbitrary offsets in that case, so a later retry restarts that artifact.

## Compression

Range requests explicitly request `Accept-Encoding: identity`; compressed transfer encoding can invalidate byte offsets.
