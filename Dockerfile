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
RUN cargo build

FROM ubuntu:24.04 AS runner

RUN mkdir -p /moat

WORKDIR /moat

COPY --from=builder /target/debug/moat .

CMD ["./moat", "-h"]