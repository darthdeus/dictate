{ pkgs ? import <nixpkgs> {} }:
let
  python = pkgs.python3.withPackages (p: [ p.pygobject3 p.pycairo ]);
in pkgs.stdenvNoCC.mkDerivation {
  name = "dictate-overlay-runtime";
  dontUnpack = true;
  nativeBuildInputs = [ pkgs.wrapGAppsHook3 pkgs.makeWrapper ];
  buildInputs = [ pkgs.gtk3 pkgs.gobject-introspection ];
  installPhase = ''
    mkdir -p $out/bin
    makeWrapper ${python}/bin/python3 $out/bin/dictate-overlay-python
  '';
}
