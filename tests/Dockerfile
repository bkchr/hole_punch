FROM lnl7/nix:1.11.16

RUN nix-env -iA \
 nixpkgs.gcc \
 nixpkgs.openssl \
 nixpkgs.pkgconfig \
 nixpkgs.rustc \
 nixpkgs.cargo 

# We can not install clang and gcc into the same env
RUN nix-shell -p clang llvmPackages.clang-unwrapped --command ""

# Run some simple test, to predownload most of the required packages
RUN nix-build "<nixpkgs/nixos/tests/containers-tmpfs.nix>"
