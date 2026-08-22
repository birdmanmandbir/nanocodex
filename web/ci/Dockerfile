FROM docker.io/library/python:3.11.13-bookworm@sha256:e75be128195ec5b78912d55646e87d4638fc95234302a34472aeb2a474334cb1 AS python311

RUN python -m pip install --disable-pip-version-check \
      "maturin==1.14.1" "mypy==2.3.0"

FROM docker.io/library/python:3.14.0-bookworm@sha256:cbf88a76287c5af549f4ae71a6700e4950b913a38f70a19323a0d85399cf70f1 AS python314

RUN python -m pip install --disable-pip-version-check \
      "maturin==1.14.1" "mypy==2.3.0"

FROM docker.io/library/node@sha256:557e52a0fcb928ee113df7e1fb5d4f60c1341dbda53f55e3d815ca10807efdce AS node

FROM docker.io/library/rust:1.98.0-bookworm@sha256:e70e2eec3d495fd5c8e0be74adda86507dfac7f51a724fbf9813ff59b2b247c7 AS rust

RUN rustup toolchain install 1.97 --profile minimal \
    && rustup component add clippy rustfmt \
    && rustup target add wasm32-unknown-unknown x86_64-unknown-linux-musl \
    && cargo install --locked cargo-deny --version 0.20.2 \
    && cargo install --locked typos-cli --version 1.49.0 \
    && cargo install --locked wasm-bindgen-cli --version 0.2.126 \
    && rm -rf /usr/local/cargo/registry /usr/local/cargo/git

FROM docker.io/cloudflare/sandbox:0.12.1@sha256:ea9b35e61c800eddbc4450fad333e5dd26033a06f7d36624388b0711bef9f8c5

USER root

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:${PATH}

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
      build-essential \
      clang \
      cmake \
      jq \
      libssl-dev \
      musl-tools \
      pkg-config \
      procps \
      util-linux \
    && rm -rf /var/lib/apt/lists/*

# The Sandbox control plane and backup client stay root. Repository code gets
# one otherwise-unused identity so the root supervisor can reap every payload
# process by kernel credential, even after a double-fork or setsid escape.
RUN /usr/sbin/groupadd --gid 10001 nanocodex-ci \
    && /usr/sbin/useradd \
      --uid 10001 \
      --gid 10001 \
      --home-dir /home/nanocodex-ci \
      --create-home \
      --shell /usr/sbin/nologin \
      --no-log-init \
      nanocodex-ci \
    && /bin/chmod 0700 /home/nanocodex-ci \
    && /bin/mkdir -p /workspace \
    && /bin/chown 10001:10001 /workspace \
    && /bin/chmod 0750 /workspace \
    && if [ -f /etc/subuid ]; then /bin/sed -i '/^nanocodex-ci:/d' /etc/subuid; fi \
    && if [ -f /etc/subgid ]; then /bin/sed -i '/^nanocodex-ci:/d' /etc/subgid; fi

COPY --from=python311 /usr/local/ /opt/python/3.11/
COPY --from=python314 /usr/local/ /opt/python/3.14/
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules/npm /usr/local/lib/node_modules/npm
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=rust /usr/local/rustup /usr/local/rustup
COPY log-capture.py /usr/local/bin/nanocodex-ci-log-capture
COPY run-with-bounded-logs.sh /usr/local/bin/nanocodex-ci-run
COPY patch-sandbox-control-server.mjs /usr/local/libexec/nanocodex-patch-sandbox-control-server.mjs
COPY smoke-runner-boundary.sh /usr/local/libexec/nanocodex-ci-boundary-smoke

RUN ln -s /opt/python/3.11/bin/python /usr/local/bin/python3 \
    && ln -sf ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -sf ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && chmod 0755 /usr/local/bin/nanocodex-ci-log-capture /usr/local/bin/nanocodex-ci-run \
    && chmod 0555 \
      /usr/local/libexec/nanocodex-patch-sandbox-control-server.mjs \
      /usr/local/libexec/nanocodex-ci-boundary-smoke \
    && /usr/local/bin/bun /usr/local/libexec/nanocodex-patch-sandbox-control-server.mjs \
    && test "$(/usr/bin/stat -c '%u:%g:%a' /container-server/dist/index.js)" = "0:0:644"

# Exercise the real Linux boundary while the image is built. The first command
# proves the complete credential/capability shape. The second leaves a
# TERM-ignoring setsid child after its leader exits; the supervisor must reap it
# and reject the otherwise-successful step before returning to the backup path.
RUN /usr/local/libexec/nanocodex-ci-boundary-smoke

ENTRYPOINT ["/usr/local/bin/bun", "/container-server/dist/index.js"]
