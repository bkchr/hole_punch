#!/usr/bin/env bash
set -e

qemu_args=( -drive file=/vm_image/hole_punch_vm.qcow2 -fsdev local,security_model=passthrough,id=fsdev0,path=$(pwd) -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=hostshare -m 1024M -net user,hostfwd=tcp::2222-:22 -net nic -vga none -nographic )

if [ -e /dev/kvm ]; then
  qemu_args+=( -enable-kvm )
fi

echo "${qemu_args[@]}"
qemu-system-x86_64 "${qemu_args[@]}" > /dev/null < /dev/null &

# wait for qemu to get ready
while ! nc -z localhost 2222; do
  sleep 0.1
done

# mount the current directory into the vm
ssh -i /ssh/id_rsa -o StrictHostKeyChecking=no -p 2222 mininet@localhost "sudo mkdir -p /share && sudo mount -t 9p -o trans=virtio,version=9p2000.L,rw hostshare /share"

for test_case in test_cases/*.py; do
  # execute the test case in qemu
  ssh -i /ssh/id_rsa -o StrictHostKeyChecking=no -p 2222 mininet@localhost "cd /share && sudo python2 $test_case"
done
