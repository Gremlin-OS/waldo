{
  description = "Waldo: operate interactive terminal programs through messages";

  inputs = {
    nixpkgs.url = "https://channels.nixos.org/nixpkgs-unstable/nixexprs.tar.zst";
  };

  outputs =
    {
      self,
      nixpkgs,
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

      # The emulator core, built statically from the tarballs in
      # vendor/libghostty-vt. See DESIGN.md section 9.2.
      mkVt = pkgs: pkgs.callPackage ./nix/libghostty-vt.nix { };

      mkWaldo =
        system: pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "waldo";
          version = "0.1.0";
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./build.rs
              ./PROTOCOL.md
              ./src
              ./tests
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          env.LIBGHOSTTY_VT_LIB_DIR = "${mkVt pkgs}/lib";
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
          # The emulator library by itself, so that CI can build it once and
          # keep it.
          libghostty-vt = mkVt pkgs;
        }
      );

      # Building the package runs the unit and integration tests. The clippy
      # check fails on any warning, and the fmt check on any file that
      # `cargo fmt` would change.
      checks = forAll (
        system: pkgs: {
          waldo = self.packages.${system}.waldo;
          fmt = self.packages.${system}.waldo.overrideAttrs (old: {
            pname = "waldo-fmt";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.rustfmt ];
            buildPhase = ''
              cargo fmt --check
            '';
            doCheck = false;
            installPhase = ''
              touch $out
            '';
          });
          clippy = self.packages.${system}.waldo.overrideAttrs (old: {
            pname = "waldo-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = ''
              cargo clippy --offline --all-targets -- -D warnings
            '';
            doCheck = false;
            installPhase = ''
              touch $out
            '';
          });
        }
      );

      devShells = forAll (
        system: pkgs: {
          default = pkgs.mkShell {
            env.LIBGHOSTTY_VT_LIB_DIR = "${mkVt pkgs}/lib";
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
          };
        }
      );
    };
}
