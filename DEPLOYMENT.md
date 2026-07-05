# Ghosty Deployment

Ghosty releases are built by GitHub Actions and published as draft GitHub Releases.
The app also uses Tauri updater signatures so installed clients can auto-update from
the latest GitHub release.

## One-Time Updater Secret Setup

The updater private key and password were generated locally outside the repository:

```text
C:\Users\steel\.tauri\ghosty-updater.key
C:\Users\steel\.tauri\ghosty-updater.key.password.txt
```

Do not commit these files. Add them to GitHub repository secrets from PowerShell:

```powershell
cd B:\Code\Rust\ghosty

gh secret set TAURI_SIGNING_PRIVATE_KEY --body (Get-Content "$env:USERPROFILE\.tauri\ghosty-updater.key" -Raw)
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --body ((Get-Content "$env:USERPROFILE\.tauri\ghosty-updater.key.password.txt" -Raw).Trim())
```

The matching public key is committed in `src-tauri/tauri.conf.json`.

## Release Checklist

1. Update the version in all three places:

```text
package.json
src-tauri/tauri.conf.json
src-tauri/Cargo.toml
```

2. Verify locally:

```powershell
bun run check
bun run build
cargo check --manifest-path src-tauri/Cargo.toml
```

3. Commit and push:

```powershell
git add .
git commit -m "Prepare Ghosty release"
git push
```

4. Create and push a release tag. Tags must start with `app-v`:

```powershell
git tag app-v0.1.0
git push origin app-v0.1.0
```

5. Open GitHub Releases for `steele123/ghosty`.

The workflow creates a draft release named `Ghosty v0.1.0` and uploads the Windows
installer assets, updater signatures, and `latest.json`.

6. Test the draft release assets, then publish the release.

## Auto-Update Flow

Installed Ghosty clients check this updater endpoint:

```text
https://github.com/steele123/ghosty/releases/latest/download/latest.json
```

When a newer signed release exists, Ghosty downloads and installs it, then relaunches.

## Notes

- The updater signature is not the same as Windows code signing.
- Unsigned Windows installers may show SmartScreen warnings.
- If the updater private key or password is lost, existing installations cannot
  verify future updates signed by a new key.
