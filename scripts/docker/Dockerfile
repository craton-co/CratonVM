# CI-equivalent build environment for scripts/ci-docker.sh.
#
# Mirrors what .github/workflows/ci.yml's ubuntu-latest legs install
# (dtolnay/rust-toolchain@stable with clippy+rustfmt, actions/setup-java@v4
# distribution: temurin java-version: '25') — not a general dev image, and
# not a match for the windows-latest / macos-latest legs, which this local
# runner does not attempt to replicate.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH

# libssl-dev: openssl-sys (native-builtins-crypto's TLS stack) links the
# system OpenSSL rather than vendoring it, and fails to even configure
# without the dev headers + pkg-config finding them. zlib1g-dev, cmake, and
# clang/libclang-dev cover the other -sys crates in Cargo.lock that can also
# reach for a system library or a C toolchain beyond plain gcc (libz-sys,
# libffi-sys, libmimalloc-sys) if their own vendored/bundled build paths
# aren't taken.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential curl ca-certificates git pkg-config wget gnupg \
        libssl-dev zlib1g-dev cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Rust stable, same toolchain selector CI uses (dtolnay/rust-toolchain@stable
# resolves to whatever "stable" is on the day CI runs — this image floats the
# same way, not pinned).
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable \
            --component clippy --component rustfmt \
    && rustup --version

# Temurin JDK 25, via Adoptium's own apt repo (same distribution CI's
# actions/setup-java@v4 installs).
RUN wget -qO - https://packages.adoptium.net/artifactory/api/gpg/key/public \
        | gpg --dearmor -o /usr/share/keyrings/adoptium.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/adoptium.gpg] https://packages.adoptium.net/artifactory/deb $(awk -F= '/^VERSION_CODENAME/{print $2}' /etc/os-release) main" \
        > /etc/apt/sources.list.d/adoptium.list \
    && apt-get update && apt-get install -y --no-install-recommends temurin-25-jdk \
    && rm -rf /var/lib/apt/lists/*

ENV JAVA_HOME=/usr/lib/jvm/temurin-25-jdk-amd64

WORKDIR /workspace
