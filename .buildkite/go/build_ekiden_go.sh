#!/bin/bash

##
# This script builds the go stuff...
# 
# Usage:
# build_rust_runtime.sh [output_bin_path]
#
# output_bin_path - Optional. The path that the final ekiden
#                   binary should be moved to. Useful for moving
#                   the binary to a volume mounted to the docker
#                   host.
##

# Helpful tips on writing build scripts:
# https://buildkite.com/docs/pipelines/writing-build-scripts
set -euxo pipefail

output_bin_path=${1:-NOT_DEFINED}

# Install protobuf
# TODO Check whether this actually needs to happen.
#      Something similar is already in the base image.
go get -d github.com/golang/protobuf/protoc-gen-go
cd $GOPATH/src/github.com/golang/protobuf
git checkout v1.0.0
cd protoc-gen-go
go install

# Download oasis/ekiden repo
# TODO This binary should really be built in the
#      pipeline for the oasislabs/ekiden repo and
#      exposed as an artifact. There is no need to
#      rebuild this if only runtime-ethereum has
#      changed and the go code has not.
mkdir -p ~/.ssh
ssh-keyscan -t rsa github.com >> ~/.ssh/known_hosts
mkdir -p $GOPATH/src/github.com/oasislabs
cd $GOPATH/src/github.com/oasislabs
git clone --depth 1 git@github.com:/oasislabs/ekiden

# Build ekiden/go
cd ekiden/go
dep ensure
go generate ./...
go build -v -o ./ekiden/ekiden ./ekiden
cd $GOPATH/src/github.com/oasislabs/ekiden/go/ekiden
go install

if [ $output_bin_path != "NOT_DEFINED" ]; then
  mv /go/bin/ekiden $output_bin_path
fi