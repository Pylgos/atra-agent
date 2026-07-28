{
  description = "Atra";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = (builtins.fromTOML (builtins.readFile ./crates/atra-cli/Cargo.toml)).package.version;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
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
            hash = "sha256-iq+HB8mlJzc5P3nmeoU8STMNmZWAzVGDxa2yGAnhF+k=";
          };
          cargoVendorDir = pkgs.runCommand "atra-cargo-vendor" { } ''
            mkdir "$out"
            cp -R ${nixCargoVendor}/. "$out/"
            substitute ${nixCargoVendor}/.cargo/config.toml "$out/config.toml" \
              --replace-fail '@vendor@' "$out"
          '';
          common = {
            pname = "atra";
            inherit version;
            src = cargoSource;
            inherit cargoVendorDir;
            strictDeps = true;
            cargoExtraArgs = "-p atra-cli -p atra-runner --bin atra --bin atra-runner";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ static.openssl ];
            doCheck = false;
          };
          cargoArtifacts = craneLib.buildDepsOnly common;
          binaries = craneLib.buildPackage (
            common
            // {
              inherit cargoArtifacts;
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
                install -m755 ${static.bash}/bin/bash "$staging/bin/bash"
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
            XDG_DATA_HOME="$out/share" ${cli}/bin/atra platform install \
              ${platformBundle}/atra-platform-${platform}.zip
          '';
        in
        {
          inherit atra runner;
          default = atra;
          platform-bundle = platformBundle;
        }
      );
    };
}
