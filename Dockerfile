# pacewright — daemon (`pacewrightd`) para producción.
#
# Corre el daemon que un servicio vecino (p. ej. el server de CazaFacturas)
# usa por su socket de control `~/.pacewright/pw.sock`: el login de Claude
# (OAuth sin loopback: `anthropic_login_url`/`anthropic_login_submit`), el
# estado (`anthropic_status`) y el encolado de tareas. El socket y la credencial
# (`secrets.json`) viven en `~/.pacewright`, que en compose es un volumen
# compartido con ese vecino.
#
# Qué NO trae, a propósito: Chrome ni el CLI `claude`. Eso lo necesita la
# AUTORÍA de recetas de verdad (`claude_cli/run` explorando el portal), que es
# la capa siguiente —y el navegador anti-detect va por ahí. Con esta imagen ya
# funciona el login web de Claude en producción y el encolado; correr el
# aprendizaje headless pide agregar `claude` + un Chrome (anti-detect) a esta
# imagen o a un sidecar.
#
# reqwest usa rustls y rusqlite es `bundled`, así que no hace falta openssl ni
# libsqlite: el runtime solo necesita las raíces TLS (`ca-certificates`).

FROM rust:1-slim-bookworm AS builder
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# El workspace entero: `cargo` necesita todos los manifiestos para resolverlo,
# aunque solo compilemos el daemon. `.dockerignore` deja fuera target/.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p pacewright-daemon --bin pacewrightd

FROM debian:bookworm-slim AS runtime
# ca-certificates: raíces TLS para el OAuth de Claude (api.anthropic.com).
# El uid 10001 coincide a propósito con el del server de CazaFacturas: el socket
# que crea el daemon queda accesible para ese vecino, que corre con ese uid.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /home/pacewright pacewright
COPY --from=builder /src/target/release/pacewrightd /usr/local/bin/pacewrightd
USER pacewright
ENV HOME=/home/pacewright
# El daemon crea ~/.pacewright (socket + secrets.json + pacewright.db) al
# arrancar. En compose ese directorio es un volumen compartido para el socket.
VOLUME ["/home/pacewright/.pacewright"]
ENTRYPOINT ["pacewrightd"]
