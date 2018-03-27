FROM ubuntu:16.04

RUN apt-get update -y && apt-get install -y libssl-dev clang libclang-dev gcc qemu

RUN curl https://sh.rustup.rs -sSf | sh

RUN mkdir -p /root/.ssh

COPY vm/ssh.key /root/.ssh/id_rsa

RUN mkdir -p /vm_image/

COPY hole_punch_vm.qcow2 /vm_image/
