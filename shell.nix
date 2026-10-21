# Dev shell for building dictate. transcribe-cpp-sys compiles transcribe.cpp
# (vendored ggml) with CMake and the Vulkan backend, which needs the Vulkan
# headers, glslc (shaderc) and SPIR-V headers at build time.
{ pkgs ? import <nixpkgs> { } }:
pkgs.mkShell {
  packages = with pkgs; [
    cargo
    rustc
    clippy
    rustfmt
    cmake
    pkg-config
    shaderc
    spirv-headers
    vulkan-headers
    vulkan-loader
  ];
}
