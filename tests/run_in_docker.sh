#!/usr/bin/env bash
set -e

qemu-kvm -drive file=./hole_punch_vm.qcow2 -fsdev local,security_model=passthrough,id=fsdev0,path=$(pwd)/.. -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=hostshare -m 1024M -net user,hostfwd=tcp::2222-:22 -net nic -vga none -nographic > test.log < /dev/null &
