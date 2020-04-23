# syntax=docker/dockerfile:experimental
FROM archlinux/base

ENV GHC_VERSION 8.8.3

COPY scripts/build-static-libraries.sh /build-static-libraries.sh
COPY scripts/build-static-libraries-copy-out.sh /build-static-libraries-copy-out.sh
COPY scripts/static-libs /manifests
COPY deps/internal/consensus /build

RUN chmod +x /build-static-libraries.sh
WORKDIR /
RUN --mount=type=ssh ./build-static-libraries.sh
ENTRYPOINT ["./build-static-libraries-copy-out.sh"]