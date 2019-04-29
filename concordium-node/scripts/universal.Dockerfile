FROM archlinux/base
COPY . /build-project
WORKDIR /build-project
COPY ./scripts/init.build.env.sh ./init.build.env.sh
COPY ./scripts/gen_data.sh ./gen_data.sh
COPY ./scripts/start.sh ./start.sh
ENV LD_LIBRARY_PATH=/usr/local/lib
RUN pacman -Sy && \
    pacman -Syyu --noconfirm && \
    pacman -S protobuf cmake clang git libtool rustup make m4 pkgconf autoconf automake \
        file which boost patch libunwind libdwarf elfutils unbound llvm --noconfirm && \
    pacman -Scc --noconfirm && \
    ./init.build.env.sh && \
    # Regular build
    cargo build --features=instrumentation && \
    cp /build-project/target/debug/p2p_client-cli /build-project/target/debug/p2p_bootstrapper-cli /build-project/target/debug/testrunner /build-project/ && \
    cargo clean && \
    # Sanitizer build
    rustup install nightly-2019-03-22 && \
    rustup default nightly-2019-03-22 && \
    rustup component add rust-src --toolchain nightly-2019-03-22-x86_64-unknown-linux-gnu && \
    RUSTFLAGS="-Z sanitizer=address" cargo build --target x86_64-unknown-linux-gnu --features=instrumentation && \
    RUSTFLAGS="-Z sanitizer=address" cargo test --no-run --target x86_64-unknown-linux-gnu --features=instrumentation && \
    mkdir -p sanitized && \
    mv target/x86_64-unknown-linux-gnu/debug/p2p_client-cli sanitized/ && \
    mv target/x86_64-unknown-linux-gnu/debug/p2p_bootstrapper-cli sanitized/ && \
    cp target/x86_64-unknown-linux-gnu/debug/address_sanitizer* sanitized/ && \
    cp target/x86_64-unknown-linux-gnu/debug/p2p_client-* sanitized/ && \
    cargo clean && \
    rustup default stable  && \
    # Clean
    rm -rf ~/.cargo && \
    rm -rf .git deps src benches tests src && \
    rm -rf scripts rustfmt.toml README.md p2p.capnp && \ 
    rm -rf init.build.env.sh .gitmodules .gitlab-ci.yml && \ 
    rm -rf .gitignore .gitattributes .dockerignore dns && \
    rm -rf consensus-sys Cargo.toml Cargo.lock build.rs && \
    rm -rf build-all-docker.sh && \
    chmod +x ./start.sh
EXPOSE 8950
EXPOSE 8888
EXPOSE 9090
EXPOSE 8900
EXPOSE 10000
ENTRYPOINT ./start.sh
