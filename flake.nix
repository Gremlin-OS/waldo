{
  description = "Waldo: operate interactive terminal programs through messages";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";
    # The emulator core. Pinned to the release tag; see DESIGN.md section 9.
    ghostty.url = "github:ghostty-org/ghostty/vt-1.0.1";
  };

  outputs =
    {
      self,
      nixpkgs,
      ghostty,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});

      # Programs the integration tests drive. All from nixpkgs.
      testPrograms = pkgs: [
        pkgs.bashInteractive
        pkgs.coreutils
        pkgs.vim
        pkgs.procps
      ];

      mkWaldo =
        system: pkgs:
        let
          vt = ghostty.packages.${system}.libghostty-vt;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "waldo";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          buildInputs = [
            vt.out
            vt.dev
          ];
          nativeCheckInputs = testPrograms pkgs;
          # The tests start real programs in ptys and are timing sensitive;
          # run them one at a time.
          checkFlags = [ "--test-threads=1" ];
          meta.mainProgram = "waldo-term";
        };

      only =
        pkgs: waldo: name:
        pkgs.runCommand name { meta.mainProgram = name; } ''
          mkdir -p $out/bin
          cp ${waldo}/bin/${name} $out/bin/
        '';
    in
    {
      packages = forAll (
        system: pkgs:
        let
          waldo = mkWaldo system pkgs;
        in
        {
          default = waldo;
          inherit waldo;
          waldo-wrap = only pkgs waldo "waldo-wrap";
          waldo-term = only pkgs waldo "waldo-term";
        }
      );

      # Building the package runs the unit and integration tests.
      checks = forAll (
        system: pkgs: {
          waldo = self.packages.${system}.waldo;
        }
      );

      devShells = forAll (
        system: pkgs:
        let
          vt = ghostty.packages.${system}.libghostty-vt;
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.rust-analyzer
              pkgs.curl
              pkgs.socat
            ]
            ++ testPrograms pkgs;
            buildInputs = [
              vt.out
              vt.dev
            ];
          };
        }
      );
    };
}
