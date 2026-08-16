# Testing

Run commands from the repository root inside the Nix development environment.

## Initial setup

After changing `flake.nix`, restart the Atra Workspace so the development
environment is recreated. Confirm that the Web Client tools and browser are
available:

```console
rustc --version
dx --version
pnpm --version
test -x "$PLAYWRIGHT_CHROMIUM_EXECUTABLE"
```

Install the pinned JavaScript development dependencies without changing the
lockfile:

```console
pnpm --dir web install --frozen-lockfile
```

Playwright uses the Chromium executable supplied by `flake.nix`.
`PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1` prevents it from downloading a separate
browser into the developer's home directory.

## Focused Web Client checks

Check both native code and the browser target:

```console
cargo check -p atra-web
cargo check -p atra-web-ui --target wasm32-unknown-unknown
cargo test -p atra-web -p atra-web-ui
```

Build the Tailwind CSS, Dioxus application, and self-contained native
executable:

```console
./tools/build-atra-web.sh
```

The script restores the checked-in source stylesheet after Dioxus finishes and
embeds the generated Web assets into `target/release/atra-web`.

## Playwright smoke test

Start the built daemon in one terminal:

```console
./target/release/atra-web --port 32872 serve
```

Run Playwright in another terminal:

```console
ATRA_WEB_URL=http://127.0.0.1:32872 pnpm --dir web test
```

The smoke test launches the Chromium provided by the Nix development
environment. It checks that the embedded Dioxus application loads and that its
desktop and narrow-screen shell is visible.

To inspect a failure interactively:

```console
ATRA_WEB_URL=http://127.0.0.1:32872 pnpm --dir web exec playwright test --headed
```

## Full Rust test suite

Run this once after the focused checks pass:

```console
cargo test --workspace
```

Automated tests must not contact a real model provider. Integration tests must
not recursively invoke Cargo.

## Nix packaging

Build the browser assets and final package independently:

```console
nix build .#webAssets
nix build .#default
```

`result/bin/atra-web` must serve the embedded application without a separate
asset directory.
