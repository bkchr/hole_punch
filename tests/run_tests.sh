#!/usr/bin/env bash
set -e

tmp_dir=$(dirname $(mktemp -u))
docker_home=$tmp_dir/hole_punch_test_docker_home
mkdir -p $docker_home

enable_kvm=""

if [ -e /dev/kvm ]; then
  enable_kvm="--device /dev/kvm"
fi

docker run -it -e RUN_UNTIL_ERROR=$RUN_UNTIL_ERROR $enable_kvm --rm -v $docker_home:/root -v $PWD/..:/src -w /src/tests bkchr/hole_punch_tester:latest "./run_in_docker.sh"
