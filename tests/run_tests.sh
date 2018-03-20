#!/usr/bin/env bash
set -e

cat Dockerfile | docker build -t hole_punch_test -

docker run -it -v $PWD/..:/src -w /src/tests hole_punch_test sh -c "./run_in_docker.sh"
