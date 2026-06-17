{
  lib,
  rustPlatform,
  pkg-config,
  alsa-lib,
  openssl,
}:

rustPlatform.buildRustPackage {
  pname = "dictate-desktop";
  version = "0.2.0";

  src = lib.cleanSource ./.;

  useFetchCargoVendor = true;
  cargoHash = "";

  nativeBuildInputs = [ pkg-config ];

  buildInputs = [
    alsa-lib
    openssl
  ];

  meta = with lib; {
    description = "Voice-to-text dictation daemon with evdev keybind";
    license = licenses.mit;
    mainProgram = "dictate-desktop";
  };
}
