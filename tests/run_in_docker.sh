#!/usr/bin/env bash
set -e

ssh_cmd="ssh -i /ssh/id_rsa -o StrictHostKeyChecking=no -p 2222 mininet@localhost"

qemu_args=( -drive file=/vm_image/hole_punch_vm.qcow2 -fsdev local,security_model=passthrough,id=fsdev0,path=$(pwd) -device virtio-9p-pci,id=fs0,fsdev=fsdev0,mount_tag=hostshare -m 1024M -net user,hostfwd=tcp::2222-:22 -net nic -vga none -nographic )

if [ -e /dev/kvm ]; then
  qemu_args+=( -enable-kvm )
fi

qemu-system-x86_64 "${qemu_args[@]}" > /dev/null < /dev/null &

echo "Compiling test runners.."
cd runners
cargo update
cargo install --force --root $(pwd)/..
cd ..
echo "Test runners compiled"

echo "Waiting for qemu to get ready"
while ! $ssh_cmd "exit 0"; do
  sleep 0.1
done
echo "Qemu ready"

# mount the current directory into the vm
$ssh_cmd "sudo mkdir -p /share && sudo mount -t 9p -o trans=virtio,version=9p2000.L,rw hostshare /share"

loop=1
while [ $loop == 1 ]; do
  for test_case in test_cases/*.py; do
    $ssh_cmd "echo hello_travis"
    echo "Executing test_case: $test_case"
    # execute the test case in qemu
    $ssh_cmd "/share/bin/server --listen_port 4567&"
    $ssh_cmd "cd /share && touch test && ls -alh"
    $ssh_cmd "cd /share && sudo python2 $test_case"
  done
  loop=${RUN_UNTIL_ERROR:=0}
done
