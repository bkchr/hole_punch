#!/usr/bin/env bash
set -e

tmpdir=$(dirname $(mktemp -u))
enable_kvm=""

if [ -e /dev/kvm ]; then
  enable_kvm="--device /dev/kvm"
fi

docker run -it $enable_kvm --rm -v $tmpdir/hole_punch_test_docker_home:/root -v $PWD/..:/src -w /src/tests bkchr/hole_punch_tester "./run_in_docker.sh"
