{
  description = "Leptos + Fedimint PWA development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };
      in {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            binaryen
            cargo-leptos
            cargo-nextest
            clang
            chromium
            just
            leptosfmt
            nodejs_22
            openssl
            pkg-config
            protobuf
            rust-analyzer
            rustToolchain
            sqlite
            trunk
            wasm-bindgen-cli
          ];

          env = {
            PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
            RUST_BACKTRACE = "1";
          };

          shellHook = ''
            export PATH="$PWD/node_modules/.bin:$PATH"
            export PLAYWRIGHT_BROWSER_EXECUTABLE_PATH="${pkgs.chromium}/bin/chromium"

            cat <<'EOF'
            Leptos + Fedimint dev shell ready.

            Useful commands:
              cargo leptos watch
              trunk serve
              cargo nextest run
              npm install
              npm run playwright:mcp:help
            EOF
          '';
        };
      });
}
