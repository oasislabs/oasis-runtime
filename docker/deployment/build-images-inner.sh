#!/bin/bash -e

# Our development image sets up the PATH in .bashrc. Source that.
PS1='\$'
. ~/.bashrc
set -x

# Abort on unclean packaging area.
if [ -e target/docker-deployment/context ]; then
    cat >&2 <<EOF
Path target/docker-deployment/context already exists. Aborting.
If this was accidentally left over and you don't need anything from
it, you can remove it and try again.
EOF
    exit 1
fi

# Build all Ekiden binaries and resources.
CARGO_TARGET_DIR=target cargo install --force --git https://github.com/oasislabs/ekiden --branch master ekiden-tools
cargo ekiden build-enclave --output-identity --release
(cd gateway && CARGO_TARGET_DIR=../target cargo build --release)

# Package all binaries and resources.
mkdir -p target/docker-deployment/context/bin target/docker-deployment/context/lib target/docker-deployment/context/res
ln target/enclave/runtime-ethereum.so target/docker-deployment/context/lib
ln target/enclave/runtime-ethereum.mrenclave target/docker-deployment/context/res
ln target/release/gateway target/docker-deployment/context/bin
ln docker/deployment/Dockerfile target/docker-deployment/context/Dockerfile
tar cvzhf target/docker-deployment/context.tar.gz -C target/docker-deployment/context .
rm -rf target/docker-deployment/context