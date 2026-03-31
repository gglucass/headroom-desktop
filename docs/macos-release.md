# macOS Release and App Updates

Headroom is set up for outside-the-App-Store macOS distribution with:

- Tauri's official updater plugin
- signed updater artifacts
- user-confirmed install prompts
- Apple code signing and notarization

## Build a signed DMG locally

If your Apple Developer access is ready on your Mac, the fastest local path is:

```bash
npm install
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export TAURI_SIGNING_PRIVATE_KEY="$(cat .secrets/tauri-updater/private.key)"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="your-updater-key-password"
export APPLE_API_ISSUER="your-app-store-connect-issuer-id"
export APPLE_API_KEY="your-app-store-connect-key-id"
export APPLE_API_KEY_PATH="$HOME/.private_keys/AuthKey_ABC123XYZ.p8"
export HEADROOM_UPDATER_PUBLIC_KEY="$(cat .secrets/tauri-updater/public.key)"
export HEADROOM_UPDATER_ENDPOINTS='["https://github.com/<owner>/<repo>/releases/latest/download/latest.json"]'
npm run build:mac:dmg
```

This produces a signed `.dmg` in `src-tauri/target/release/bundle/dmg/` for the current machine architecture.

If you want a universal build, install both Rust macOS targets first and then run:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
TARGET=universal-apple-darwin npm run build:mac:dmg
```

The local helper script sets `CI=true` for Tauri's DMG bundler, validates the required secrets, and supports either:

- `APPLE_API_KEY_PATH` for a local App Store Connect private key file
- `APPLE_API_PRIVATE_KEY_P8` if you prefer storing the key contents directly in an environment variable
- `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` if you want Apple ID notarization instead

## What the app expects

This build reads two compile-time environment variables:

- `HEADROOM_UPDATER_PUBLIC_KEY`
  The public key for verifying Tauri updater signatures.
- `HEADROOM_UPDATER_ENDPOINTS`
  A JSON array or comma-separated list of HTTPS update feed URLs.

Example:

```bash
export HEADROOM_UPDATER_PUBLIC_KEY="$(cat .secrets/tauri-updater/public.key)"
export HEADROOM_UPDATER_ENDPOINTS='["https://github.com/<owner>/<repo>/releases/latest/download/latest.json"]'
```

These values are compiled into the release build. If they are missing, Headroom still runs, but update checks stay disabled for that build.

## Environment variables to set

Required for a signed local DMG in this repo:

- `APPLE_SIGNING_IDENTITY`
  Your Developer ID Application certificate name from Keychain Access, for example `Developer ID Application: Your Name (TEAMID)`.
- `TAURI_SIGNING_PRIVATE_KEY`
  The private updater signing key contents because this repo builds updater artifacts alongside the DMG.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
  The password for that updater signing key.

Required for notarization, choose one mode:

- App Store Connect API mode:
  `APPLE_API_ISSUER`, `APPLE_API_KEY`, and either `APPLE_API_KEY_PATH` or `APPLE_API_PRIVATE_KEY_P8`
- Apple ID mode:
  `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`

Recommended for production builds of Headroom so auto-update stays enabled:

- `HEADROOM_UPDATER_PUBLIC_KEY`
  The public half of the Tauri updater signing keypair.
- `HEADROOM_UPDATER_ENDPOINTS`
  A JSON array or comma-separated list of HTTPS update feed URLs.

Optional, usually only needed outside your own machine:

- `APPLE_CERTIFICATE`
  Base64-encoded `.p12` signing certificate export. Useful for CI or a clean machine without the certificate already installed in your login keychain.
- `APPLE_CERTIFICATE_PASSWORD`
  Password for the exported `.p12` certificate.

## Repository configuration

The GitHub Actions workflow expects these repository settings:

- Repository variable:
  `HEADROOM_UPDATER_PUBLIC_KEY`
- Repository secrets:
  `TAURI_SIGNING_PRIVATE_KEY`
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`
  `APPLE_CERTIFICATE`
  `APPLE_CERTIFICATE_PASSWORD`
  `APPLE_SIGNING_IDENTITY`

For notarization, configure one of these two sets:

- App Store Connect API:
  `APPLE_API_ISSUER`
  `APPLE_API_KEY`
  `APPLE_API_PRIVATE_KEY_P8`
- Apple ID:
  `APPLE_ID`
  `APPLE_PASSWORD`
  `APPLE_TEAM_ID`

## One-time updater key setup

Generate a Tauri updater keypair once and keep the private key in CI secrets:

```bash
npm run tauri signer generate -- -w ~/.tauri/headroom-desktop.key
```

Store:

- the generated public key in `HEADROOM_UPDATER_PUBLIC_KEY` during release builds
- the generated private key in CI as `TAURI_SIGNING_PRIVATE_KEY`
- the private-key password in CI as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`

## Release pipeline

For each mac release:

1. Build with `HEADROOM_UPDATER_PUBLIC_KEY` and `HEADROOM_UPDATER_ENDPOINTS` set.
2. Code-sign the app with your Apple Developer ID Application certificate.
3. Notarize the build with Apple.
4. Publish the signed updater artifacts and `latest.json`.
5. Create or update the GitHub Release that hosts those files.

The app is already configured with `"createUpdaterArtifacts": true`, so Tauri will emit updater-friendly release artifacts during bundling.

## Apple signing and notarization

Use a Developer ID flow, not Mac App Store packaging.

Tauri's macOS distribution docs support two notarization paths:

- App Store Connect API credentials:
  `APPLE_API_ISSUER`, `APPLE_API_KEY`, `APPLE_API_KEY_PATH`
- Apple ID credentials:
  `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`

You also need the signing certificate material used by the macOS bundle build, typically:

- `APPLE_CERTIFICATE`
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_SIGNING_IDENTITY`

## Recommended hosting

For a small app, the simplest setup is:

- GitHub Releases for DMG and updater artifacts
- a stable `latest.json` release asset URL

`latest.json` should follow Tauri's static updater format and include the macOS platform entry, the signed update bundle URL, and the bundle signature.

You can later move the updater feed to S3 or another CDN without changing app code, as long as the published endpoint URL stays valid and the signatures match the embedded public key.

## User experience in Headroom

Headroom does not auto-install updates silently.

Current behavior:

- checks for updates in the background after launch
- lets the user manually check from Settings
- prompts before download/install
- asks the user to restart after install completes

## Recommended next step

Add a release workflow in CI that:

- builds `tauri build` for macOS
- injects the updater env vars above
- signs and notarizes the app
- uploads the updater artifacts plus `latest.json` to the release

Tauri's official GitHub release tooling can generate `latest.json` for you, which is the easiest way to keep the feed and artifacts aligned.

This repo now includes a workflow at `.github/workflows/release-macos.yml`.

It:

- runs on manual dispatch only
- builds both `aarch64-apple-darwin` and `x86_64-apple-darwin`
- signs and notarizes the app
- uploads updater artifacts and `latest.json` to the GitHub Release
