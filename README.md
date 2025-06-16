<p align="center">
    <img src="https://raw.githubusercontent.com/mrcroxx/moat/main/etc/logo/slogan.svg" />
</p>

# moat

S3-compatible Object Store Accelerator.

***Work In Progress ...***

Inspired by [ScopeDB/Percas](https://github.com/scopedb/percas), a distributed persistent cache service optimized for high performance NMVe SSD and provides simple HTTP APIs.

If a HTTP cache service is all your need, please consider it.

## Development

***Moat*** use the way called [cargo-xtask](https://github.com/matklad/cargo-xtask) for development. You only need to setup the rust toolchain to run the tasks.

To install rust toolchain with rustup if you didn't set it up:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then run `cargo x -h` for usage.

E.g.

`cargo x`: Run the default task sult, alias for `cargo x all`.

`cargo x dev up [<service>]`: Start moat cluster development environment.
`cargo x dev up [<service>]`: Start moat cluster development environment.
`cargo x dev clean`: Stop moat cluster development environment if it is running, and clean the volumes, logs and caches. (Build caches will NOT be cleaned.)

> It will be more smoother with the following settings:
>
> ```sh
> alias x="cargo x"
> alias xd="cargo x dev"
> ```

### Develop moat cluster with a proxy to the internet

In some countries and regions, accessing the complete internet requires the use of a proxy. To ensure that the build and run of moat can access the internet normally without affecting communication between them, the following configuration can be implemented:

- `docker-compose.override.yaml`

```yaml
services:
  minio:
    environment:
      - no_proxy=${MOAT_NO_PROXY}
  init:
    environment:
      - no_proxy=${MOAT_NO_PROXY}
  moat-cache-1: &moat
    build:
      args:
        - HTTP_PROXY={MOAT_PROXY}
        - HTTPS_PROXY={MOAT_PROXY}
        - NO_PROXY=${MOAT_NO_PROXY}
    environment:
      - http_proxy={MOAT_PROXY}
      - https_proxy={MOAT_PROXY}
      - no_proxy=${MOAT_NO_PROXY}
  moat-cache-2: *moat
  moat-cache-3: *moat
  moat-cache-4: *moat
  moat-agent-1: *moat
  moat-agent-2: *moat

```

- `.env`

```properties
MOAT_NO_PROXY=localhost,127.0.0.1,minio,moat,moat-cache-1,moat-cache-2,moat-cache-3,moat-cache-4,moat-agent-1,moat-agent-2
MOAT_PROXY=<YOUR PROXY ENDPOINT>
```

Additionally, the docker-daemon may also need a proxy to fetch images. Please refer to the [docker official document](https://docs.docker.com/engine/cli/proxy/).