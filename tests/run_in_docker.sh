#!/usr/bin/env bash
set -e

pushd runners
nix-shell -E 'with import <nixpkgs> { }; pkgs.mkShell { buildInputs = [ pkgconfig openssl cargo clang ]; LIBCLANG_PATH="${llvmPackages.clang-unwrapped}/lib"; shellHook = "export NIX_CXXSTDLIB_LINK=\"\""; }' --command "cargo build --all"
popd

nix-build test_cases.nix
