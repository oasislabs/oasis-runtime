#!/bin/bash

# TODO Update build scripts to be DRY.

#################################################
# This script runs the cChannel-eth contract 
# test suite.
#
# Usage:
# run_celer_tests.sh <src_dir>
#
# src_dir - Absolute or relative path to the
#           directory containing the source code.
#################################################

# Helpful tips on writing build scripts:
# https://buildkite.com/docs/pipelines/writing-build-scripts
set -euxo pipefail

source scripts/utils.sh

# Ensure cleanup on exit.
# cleanup() is defined in scripts/utils.sh
trap 'cleanup' EXIT

####################
# Set up environment
####################
export SGX_MODE="SIM"
export INTEL_SGX_SDK="/opt/sgxsdk"
export EKIDEN_UNSAFE_SKIP_AVR_VERIFY="1"
export RUST_BACKTRACE="1"

########################################
# Add SSH identity so that `cargo build`
# can successfully download dependencies
# from private github repos.
# TODO kill this process when script exits
########################################
eval `ssh-agent -s`
ssh-add

#################################################
# Add github public key to known_hosts.
# This is required because some test scripts
# run `npm install` and at least one dependency
# has its own dependencies that pull from
# GitHub and the /root/.gitconfig file transforms
# https to ssh when pulling from GitHub.
#################################################
ssh-keyscan rsa github.com >> ~/.ssh/known_hosts

#######################################################
# Update the PATH to respect $CARGO_INSTALL_ROOT.
# This allows 'cargo install' to reuse binaries 
# from previous installs as long as the correct
# host directory is mounted on the docker container.
# Huge speed improvements during local dev and testing.
#######################################################
set +u
export PATH=$CARGO_INSTALL_ROOT/bin/:$PATH
set -u

######################################################
# Install ekiden-compue if it is not already installed
######################################################
set +u
cargo_install_root=$(get_cargo_install_root)
echo "cargo_install_root=$cargo_install_root"
set -u

if [ ! -e "$cargo_install_root/bin/ekiden-compute" ]; then
  echo "Installing ekiden-compute."  
  cargo install \
    --git https://github.com/oasislabs/ekiden \
    --branch master \
    --debug \
    ekiden-compute
fi

# Run the celer tests
./scripts/test-dapp.sh celer
