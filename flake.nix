{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (fromTOML (builtins.readFile ./crates/atra-cli/Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          hostPkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          hostRust = hostPkgs.rust-bin.stable."1.96.0".minimal.override {
            targets = [ "wasm32-unknown-unknown" ];
          };
          static = pkgs.pkgsStatic;
          craneLib = crane.mkLib static;
          architecture =
            {
              x86_64-linux = "x86_64";
              aarch64-linux = "aarch64";
            }
            .${system};
          platform = "${architecture}-linux-static";
          cargoSource = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };
          nixCargoVendor = static.rustPlatform.fetchCargoVendor {
            name = "atra-${version}";
            src = cargoSource;
            # To update after Cargo.lock changes, temporarily use
            # `hash = pkgs.lib.fakeHash;`, run `nix build .#atra`, then copy
            # the `got: sha256-...` value from the hash mismatch below.
            # hash = pkgs.lib.fakeHash;
            hash = "sha256-mNlCwvJwbg5q+PZ0idjAHKaNIZjery/vR8eCbiD+GJQ=";
          };
          cargoVendorDir = pkgs.runCommand "atra-cargo-vendor" { } ''
            mkdir "$out"
            cp -R ${nixCargoVendor}/. "$out/"
            substitute ${nixCargoVendor}/.cargo/config.toml "$out/config.toml" \
              --replace-fail '@vendor@' "$out"
          '';
          webAssets = pkgs.stdenv.mkDerivation {
            pname = "atra-web-assets";
            inherit version;
            src = cargoSource;
            nativeBuildInputs = [
              hostRust
              hostPkgs.binaryen
              hostPkgs.dioxus-cli
              hostPkgs.tailwindcss_4
            ];
            buildPhase = ''
              runHook preBuild
              export CARGO_HOME=$TMPDIR/cargo-home
              mkdir -p "$CARGO_HOME" .cargo
              cp ${cargoVendorDir}/config.toml .cargo/config.toml
              (
                cd crates/atra-web-ui
                dx build --release
              )
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              cp -R target/dx/atra-web-ui/release/web/public "$out"
              runHook postInstall
            '';
          };
          common = {
            pname = "atra";
            inherit version;
            src = cargoSource;
            inherit cargoVendorDir;
            strictDeps = true;
            cargoExtraArgs = "-p atra-cli -p atra-runner -p atra-web --bin atra --bin atra-runner --bin atri --bin atra-web";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ static.openssl ];
            doCheck = false;
          };
          cargoArtifacts = craneLib.buildDepsOnly common;
          binaries = craneLib.buildPackage (
            common
            // {
              inherit cargoArtifacts;
              ATRA_WEB_ASSETS_DIR = webAssets;
            }
            // pkgs.lib.optionalAttrs (self ? rev) {
              ATRA_BUILD_COMMIT = self.rev;
            }
          );
          cli = pkgs.runCommand "atra-cli-${version}" { } ''
            mkdir -p "$out/bin"
            install -m755 ${binaries}/bin/atra "$out/bin/atra"
          '';
          runner = pkgs.runCommand "atra-runner-${version}" { } ''
            mkdir -p "$out/bin"
            install -m755 ${binaries}/bin/atra-runner "$out/bin/atra-runner"
            install -m755 ${binaries}/bin/atri "$out/bin/atri"
          '';
          platformBundle =
            pkgs.runCommand "atra-platform-${platform}"
              {
                nativeBuildInputs = [
                  pkgs.file
                  pkgs.jq
                  pkgs.zip
                  pkgs.zstd
                ];
              }
              ''
                staging=$TMPDIR/staging
                mkdir -p "$staging/bin" "$out"
                install -m755 ${runner}/bin/atra-runner "$staging/bin/atra-runner"
                install -m755 ${runner}/bin/atri "$staging/bin/atri"
                install -m755 ${static.bash}/bin/bash "$staging/bin/bash"
                install -m755 ${static.bubblewrap}/bin/bwrap "$staging/bin/bwrap"
                install -m755 ${static.fd}/bin/fd "$staging/bin/fd"
                install -m755 ${static.jq}/bin/jq "$staging/bin/jq"
                install -m755 ${static.ripgrep}/bin/rg "$staging/bin/rg"
                install -m755 ${static.tmux}/bin/tmux "$staging/bin/tmux"
                ${pkgs.runtimeShell} ${./tools/platform-bundle/package.sh} \
                  "$out/atra-platform-${platform}.zip" \
                  "$staging" \
                  "${platform}"
              '';
          atra = pkgs.runCommand "atra-${version}" { } ''
            mkdir -p "$out/bin" "$out/share"
            install -m755 ${cli}/bin/atra "$out/bin/atra"
            install -m755 ${binaries}/bin/atra-web "$out/bin/atra-web"
            XDG_DATA_HOME="$out/share" ${cli}/bin/atra platform install \
              ${platformBundle}/atra-platform-${platform}.zip
          '';
        in
        {
          inherit atra runner webAssets;
          default = atra;
          platform-bundle = platformBundle;
        }
      );
      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable."1.96.0".minimal.override {
            extensions = [
              "clippy"
              "rust-analyzer"
              "rust-src"
              "rustfmt"
            ];
            targets = [ "wasm32-unknown-unknown" ];
          };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              binaryen
              bubblewrap
              clang
              chromium
              dioxus-cli
              git
              just
              mold
              nodejs
              openssl
              pnpm
              pkg-config
              rustToolchain
              tailwindcss_4
            ];
            PLAYWRIGHT_CHROMIUM_EXECUTABLE = pkgs.lib.getExe pkgs.chromium;
            PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
          };
        }
      );
    };
}
