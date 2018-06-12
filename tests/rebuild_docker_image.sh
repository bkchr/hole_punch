#!/usr/bin/env bash
set -e

if [ -z ${USE_EXISTING_IMAGE+x} ]; then
  echo "Copying vm image from old docker image"
  docker run -it --rm -v $PWD:/host bkchr/hole_punch_tester:latest sh -c "cp /vm_image/hole_punch_vm.qcow2 /host/"
else
  echo "Using existing image"
fi

echo "Creating new docker image"
docker build --no-cache -t bkchr/hole_punch_tester:latest .

echo "Pushing docker image"
docker push bkchr/hole_punch_tester:latest

echo "Removing vm image"
sudo rm hole_punch_vm.qcow2
