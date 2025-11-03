{pkgs ? import <nixpkgs> {} }:

let rustOverlay = import (builtins.fetchTarball {
  url = "https://github.com/oxalica/rust-overlay/archive/master.tar.gz";
});
myPkgs = import <nixpkgs> {
  overlays = [ rustOverlay ];
};
in pkgs.mkShell {
  buildInputs = [
    myPkgs.rust-bin.stable.latest.default
    pkgs.sqlite
    pkgs.cargo
  ];
}
