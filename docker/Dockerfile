# NeutronSync container: neutronsync serve + neutronsync-web under s6-overlay.
# Build: docker build -f docker/Dockerfile -t neutronsync:dev .

# ----- rust: CLI with the serve feature (glibc 2.35 floor, matches release.yml) -----
FROM ubuntu:22.04 AS rust
ARG DEBIAN_FRONTEND=noninteractive
ARG NEUTRONSYNC_BUILD_SHA=unknown
# rustup-init 1.28.2 (x86_64-unknown-linux-gnu); toolchain pinned (not floating stable).
ARG RUSTUP_VERSION=1.28.2
ARG RUSTUP_SHA256=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
ARG RUST_VERSION=1.97.1
RUN apt-get update && apt-get install -y --no-install-recommends \
	curl ca-certificates build-essential pkg-config \
	&& rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/rustup-init \
	"https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
	&& echo "${RUSTUP_SHA256}  /tmp/rustup-init" | sha256sum -c - \
	&& chmod +x /tmp/rustup-init \
	&& /tmp/rustup-init -y --default-toolchain "${RUST_VERSION}" --profile minimal \
	&& rm /tmp/rustup-init
ENV PATH="/root/.cargo/bin:${PATH}"
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs .build-sha ./
COPY build ./build
COPY src ./src
# Fetch and build with the serve feature only (no GUI libraries).
ENV NEUTRONSYNC_BUILD_SHA=${NEUTRONSYNC_BUILD_SHA}
RUN cargo build --release --features serve --bin neutronsync \
	&& strip target/release/neutronsync

# ----- sidecar: Bun-compiled neutronsync-drive -----
FROM oven/bun:1.4.0 AS sidecar
WORKDIR /src/sidecar
COPY Cargo.toml /src/Cargo.toml
COPY sidecar/package.json sidecar/bun.lock sidecar/tsconfig.json sidecar/legacy-session.json ./
COPY sidecar/src ./src
COPY sidecar/tests ./tests
COPY sidecar/build.sh ./
ENV NEUTRONSYNC_RELEASE_BUILD=1
RUN bun install --frozen-lockfile \
	&& ./build.sh \
	&& bun test tests/file-store.test.ts

# ----- web: NiceGUI app venv -----
FROM python:3.12-slim-bookworm AS web
COPY --from=ghcr.io/astral-sh/uv:0.13.3 /uv /usr/local/bin/uv
WORKDIR /app/web
COPY web/pyproject.toml web/uv.lock ./
COPY web/src ./src
COPY web/README.md ./
RUN uv sync --locked --no-dev \
	&& ln -sf /app/web/.venv/bin/neutronsync-web /usr/local/bin/neutronsync-web-link

# ----- final runtime image -----
FROM python:3.12-slim-bookworm
ARG NEUTRONSYNC_BUILD_SHA=unknown
ARG VERSION=dev
ARG S6_OVERLAY_VERSION=3.2.3.2
ARG S6_NOARCH_SHA256=5379750ed30a84bbd2e2dd74847ba6b5bd29cd0b2e3ea2ec58049b57eb2eda12
ARG S6_X86_64_SHA256=e6befcc96a437a3831386ecfc51808c5d3e939dc5fe3c02ae9284599e8aa2408

LABEL org.opencontainers.image.title="NeutronSync" \
	org.opencontainers.image.description="Headless Proton Drive sync with a web interface" \
	org.opencontainers.image.source="https://github.com/WilhelmZA/protondrive_linux_sync" \
	org.opencontainers.image.url="https://github.com/WilhelmZA/protondrive_linux_sync" \
	org.opencontainers.image.version="${VERSION}" \
	org.opencontainers.image.revision="${NEUTRONSYNC_BUILD_SHA}" \
	org.opencontainers.image.licenses="MIT"

# Apply Debian security updates the base image does not carry yet, then
# ca-certificates, tzdata, gio (libglib2.0-bin), xz-utils only long enough to
# unpack s6-overlay; purge xz-utils afterwards so it is not in the final image.
RUN apt-get update && apt-get upgrade -y --no-install-recommends \
	&& apt-get install -y --no-install-recommends \
	ca-certificates tzdata libglib2.0-bin xz-utils curl \
	&& curl -fsSL -o /tmp/s6-overlay-noarch.tar.xz \
		"https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz" \
	&& curl -fsSL -o /tmp/s6-overlay-x86_64.tar.xz \
		"https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-x86_64.tar.xz" \
	&& echo "${S6_NOARCH_SHA256}  /tmp/s6-overlay-noarch.tar.xz" | sha256sum -c - \
	&& echo "${S6_X86_64_SHA256}  /tmp/s6-overlay-x86_64.tar.xz" | sha256sum -c - \
	&& tar -C / -Jxpf /tmp/s6-overlay-noarch.tar.xz \
	&& tar -C / -Jxpf /tmp/s6-overlay-x86_64.tar.xz \
	&& rm /tmp/s6-overlay-noarch.tar.xz /tmp/s6-overlay-x86_64.tar.xz \
	&& apt-get purge -y xz-utils curl \
	&& apt-get autoremove -y \
	&& rm -rf /var/lib/apt/lists/* \
	&& groupadd -g 911 abc \
	&& useradd -u 911 -g abc -d /config -s /usr/sbin/nologin abc

# --chmod avoids a follow-up RUN that would duplicate the large sidecar layer.
COPY --from=rust --chmod=755 /src/target/release/neutronsync /usr/local/bin/neutronsync
COPY --from=sidecar --chmod=755 /src/sidecar/dist/neutronsync-drive /usr/local/bin/neutronsync-drive
COPY --from=web /app/web /app/web
RUN ln -sf /app/web/.venv/bin/neutronsync-web /usr/local/bin/neutronsync-web

COPY --chmod=644 docker/neutronsync.toml /defaults/neutronsync.toml
RUN chmod 755 /defaults
COPY --chmod=755 docker/rootfs/ /
# Ensure the per-volume trash wrapper wins over /usr/bin/gio.
RUN chmod 755 /usr/local/bin/gio

ENV S6_KILL_GRACETIME=15000 \
	PUID=99 \
	PGID=100 \
	UMASK=002 \
	HOME=/config \
	NEUTRONSYNC_CONFIG=/config/neutronsync.toml \
	XDG_CONFIG_HOME=/config \
	XDG_STATE_HOME=/config/state \
	XDG_DATA_HOME=/config/data \
	XDG_RUNTIME_DIR=/run/neutronsync \
	NEUTRONSYNC_SESSION_STORE=file \
	NEUTRONSYNC_WEB_DATA=/config/web \
	NEUTRONSYNC_WEB_HOST=0.0.0.0 \
	NEUTRONSYNC_WEB_PORT=8080 \
	NEUTRONSYNC_API_URL=http://127.0.0.1:8090 \
	PATH="/app/web/.venv/bin:/usr/local/bin:/command:${PATH}"

VOLUME ["/config", "/data"]
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
	CMD python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8080/healthz', timeout=4)"

ENTRYPOINT ["/init"]
