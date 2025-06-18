ARG HTTP_PROXY
ARG HTTPS_PROXY
ARG NO_PROXY

FROM rust:latest AS builder

ENV http_proxy=${HTTP_PROXY}
ENV https_proxy=${HTTPS_PROXY}
ENV no_proxy=${NO_PROXY}

WORKDIR /

RUN apt-get update && apt-get install -y cmake && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY . .
RUN mkdir -p /bin
RUN --mount=type=cache,target=/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
    cargo build && \
    cp /target/debug/moat /bin/


FROM ubuntu:24.04 AS runner

RUN mkdir -p /moat

WORKDIR /moat

RUN apt-get update && apt-get install -y iputils-ping curl && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY --from=builder /bin/moat .

ENTRYPOINT ["./moat"]
CMD ["-h"]