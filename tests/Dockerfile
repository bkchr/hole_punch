FROM ubuntu:16.04

RUN apt-get update -y && apt-get install -y libssl-dev clang libclang-dev gcc qemu-kvm ssh curl make pkg-config

# Install rust
ENV RUSTUP_HOME=/rust
ENV CARGO_HOME=/rust
RUN curl https://sh.rustup.rs -sSf | sh -s -- --default-toolchain stable -y
ENV PATH=/rust/bin:$PATH
ENV CARGO_HOME=/root/cargo

# Copy the ssh key
RUN mkdir -p /ssh
COPY vm/ssh.key /ssh/id_rsa
RUN chmod 600 /ssh/id_rsa

# Copy the vm image
RUN mkdir -p /vm_image/
COPY hole_punch_vm.qcow2 /vm_image/
