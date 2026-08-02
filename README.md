# OPCOS

Local Devin client: Rust core, Tauri v2 shell, and React frontend.

The RVM host remains Cloud-Dev's existing Node `dev-agent`; OPCOS is a
client-only implementation of its wire protocol.

## Local Linux release build

Releases are built locally; the GitHub Actions workflow is only a lightweight
lint/test signal and is not the publishing path.

1. Install the stable Rust toolchain and the Tauri CLI:

   ```bash
   cargo install tauri-cli --locked
   ```

2. Install the Linux WebKitGTK/AppIndicator development packages required by
   Tauri, then build from the repository root:

   ```bash
   npm --prefix web ci
   npm --prefix web run build
   cargo tauri build --bundles deb,appimage
   ```

3. Normalize the generated filenames and create checksums before publishing:

   ```bash
   mkdir -p artifacts/release
   while IFS= read -r file; do
     cp "$file" "artifacts/release/OPCOS-linux-$(basename "$file")"
   done < <(find target/release/bundle -type f \
     \( -name '*.deb' -o -name '*.AppImage' \))
   (cd artifacts/release && sha256sum OPCOS-linux-* > SHA256SUMS.txt)
   ```

The resulting files in `artifacts/release/` are the release payload. The
bundle configuration is enabled in `src-tauri/tauri.conf.json`; if a host is
missing a packaging dependency, install the package named by the Tauri error
and rerun the same commands.