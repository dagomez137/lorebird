{
  description = "lorebird — index and browse a maildir with GTK, Lua, and SQLite";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      allSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      forAllSystems = fn:
        nixpkgs.lib.genAttrs allSystems
          (system: fn {
            pkgs = import nixpkgs { inherit system; };
            inherit system;
          });
    in
    {
      devShells = forAllSystems ({ pkgs, system }:
        let
          # The Nix closure ships no usable fontconfig config on macOS, so
          # fontconfig sees a single fallback (DejaVu Sans) and Pango renders
          # the whole UI in it, which looks thin and poorly defined. This
          # self-contained config exposes the macOS system font directories and
          # maps the generic families Pango asks for onto native faces
          # (Helvetica Neue for sans, Menlo for monospace), with slight hinting
          # for crisp text. Applied only on Darwin via the shellHook below.
          macFontsConf = pkgs.writeText "lorebird-fonts.conf" ''
            <?xml version="1.0"?>
            <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
            <fontconfig>
              <dir>/System/Library/Fonts</dir>
              <dir>/System/Library/Fonts/Supplemental</dir>
              <dir>/Library/Fonts</dir>
              <dir>~/Library/Fonts</dir>
              <cachedir prefix="xdg">fontconfig</cachedir>

              <match target="pattern"><test name="family"><string>mono</string></test>
                <edit name="family" mode="assign" binding="same"><string>monospace</string></edit></match>
              <match target="pattern"><test name="family"><string>sans</string></test>
                <edit name="family" mode="assign" binding="same"><string>sans-serif</string></edit></match>

              <alias><family>sans-serif</family>
                <prefer><family>Helvetica Neue</family><family>Helvetica</family></prefer></alias>
              <alias><family>system-ui</family>
                <prefer><family>Helvetica Neue</family><family>Helvetica</family></prefer></alias>
              <alias><family>serif</family>
                <prefer><family>Times New Roman</family><family>Times</family></prefer></alias>
              <alias><family>monospace</family>
                <prefer><family>Menlo</family><family>Monaco</family></prefer></alias>

              <!-- GTK4 on macOS can only use its GL renderer here (the cairo
                   renderer clips lines, and this GTK build has no Vulkan), and
                   the GL path draws unhinted text soft and ill-defined. The
                   macOS system fonts (Helvetica Neue, Menlo) ship good embedded
                   TrueType hints, so use full native hinting (not the
                   autohinter) to grid-fit glyph edges to the pixel for crisp,
                   well-defined text. Grayscale antialiasing (no subpixel). -->
              <match target="font">
                <edit name="antialias" mode="assign"><bool>true</bool></edit>
                <edit name="hinting" mode="assign"><bool>true</bool></edit>
                <edit name="hintstyle" mode="assign"><const>hintfull</const></edit>
                <edit name="autohint" mode="assign"><bool>false</bool></edit>
                <edit name="rgba" mode="assign"><const>none</const></edit>
              </match>
            </fontconfig>
          '';
        in
        {
        default = pkgs.mkShell {
          name = "lorebird-dev";

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            gtk4
            gtksourceview5
            sqlite-interactive
          ];

          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            gdb
          ];

          shellHook = ''
            ${pkgs.lib.optionalString pkgs.stdenv.isDarwin ''
              export FONTCONFIG_FILE=${macFontsConf}
            ''}
            echo "=== lorebird dev shell ==="
            echo "Rust:  $(rustc --version)"
            echo "GTK4:  ${pkgs.gtk4.version}"
            echo "GtkSourceView: ${pkgs.gtksourceview5.version}"
            echo "SQLite: $(sqlite3 --version)"
            echo "Lua:   vendored (mlua)"
          '';
        };
      });

      packages = forAllSystems ({ pkgs, system }:
        let
          # The icon directory must be referenced as a separate store path because
          # buildRustPackage filters the source tree and strips non-Rust files.
          iconDir = ./crates/lorebird-gtk/resources;

          lorebird = pkgs.rustPlatform.buildRustPackage {
            pname = "lorebird";
            version = "0.1.0";
            src = ./.;

            nativeBuildInputs = with pkgs; [
              pkg-config
              glib
              wrapGAppsHook4
              copyDesktopItems
            ];

            buildInputs = with pkgs; [
              gtk4
              gtksourceview5
            ];

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            desktopItems = [
              (pkgs.makeDesktopItem {
                name = "org.lorebird.app";
                exec = "lorebird";
                icon = "org.lorebird.app";
                comment = "Lightweight mail reader for lore.kernel.org";
                desktopName = "lorebird";
                categories = [ "Network" "Email" ];
              })
            ];

            postInstall = ''
              # Install icons into the hicolor icon theme
              for size in 16 32 48 64 128 256; do
                mkdir -p $out/share/icons/hicolor/''${size}x''${size}/apps
                cp ${iconDir}/org.lorebird.app.''${size}.png \
                  $out/share/icons/hicolor/''${size}x''${size}/apps/org.lorebird.app.png
              done
            '';
          };
        in
        {
          default = lorebird;
          lorebird = lorebird;
        });
    };
}
