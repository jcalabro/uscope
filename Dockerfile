FROM fedora:41

RUN dnf update -y && \
    dnf install -y \
        cargo curl git tar xz unzip valgrind \
        libxkbcommon-devel wayland-devel

RUN dnf install -y \
        gcc-c++-14.2.1-3.fc41 \
        clang-19.1.0-1.fc41 \
        golang-1.23.2-2.fc41 \
        rust-1.81.0-6.fc41

ENV PATH="${PATH}:/usr/local/bin/c3"
RUN curl -L -o c3.tar.gz https://github.com/c3lang/c3c/releases/download/latest/c3-linux.tar.gz && \
    tar xzf c3.tar.gz && \
    mv c3 /usr/local/bin

RUN curl -L -o odin.zip https://github.com/odin-lang/Odin/releases/download/dev-2025-01/odin-ubuntu-amd64-dev-2025-01.zip && \
    unzip odin.zip && rm -f odin.zip && \
    tar -xzf dist.tar.gz && rm -rf dist.tar.gz && \
    mv odin-linux-amd64-nightly+2025-01-08 /usr/local/odin && \
    ln -s /usr/local/odin/odin /usr/local/bin/odin

ADD zig_version.txt .
RUN curl -L https://github.com/marler8997/zigup/releases/download/v2025_01_02/zigup-x86_64-linux.tar.gz | tar xz && \
    mv zigup /usr/bin && \
    zigup --index https://machengine.org/zig/index.json fetch 2024.11.0-mach && \
    zigup default $(cat zig_version.txt) && rm zig_version.txt
