#! /bin/bash

#############################################
# Simple wrapper script to call
# promote_docker_image.sh
# with the correct arguments.
# 
# This script is intended to have buildkite
# specific things, like env vars and calling
# the buildkite-agent binary. Keeping this
# separate from the generic script that gets
# called allows us to use and test the generic
# scripts easily on a local dev box.
##############################################

# Helpful tips on writing build scripts:
# https://buildkite.com/docs/pipelines/writing-build-scripts
set -euxo pipefail

####################
# Required arguments
####################
new_image_tag=$1

#################
# Local variables
#################
docker_image_name=oasislabs/ekiden-runtime-ethereum
deployment_image_tag=$(buildkite-agent meta-data \
                       get \
                       "deployment_image_tag"
                     )

# Hardcode a test tag name, just to be safe during development.
# TODO: remove before merging PR
new_image_tag=ci-test-${new_image_tag}
deployment_image_tag=ci-test-${deployment_image_tag}

##############################################
# Add the provided tag to the deployment image
##############################################

.buildkite/docker/promote_docker_image.sh \
  "${docker_image_name}:${deployment_image_tag}" \
  "${docker_image_name}:${new_image_tag}"
