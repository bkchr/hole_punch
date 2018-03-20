#!/usr/bin/env bash
set -e

pushd runners
nix-shell -E 'with import <nixpkgs> { }; stdenv.mkDerivation { name = "build"; buildInputs = [ pkgconfig openssl clang ]; LIBCLANG_PATH="${llvmPackages.libclang}/lib"; shellHook = "export NIX_CXXSTDLIB_LINK=\"\""; }' --command "cargo build --all"
popd

nix-build test_cases.nix
