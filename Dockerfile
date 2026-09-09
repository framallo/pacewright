# pacewright — daemon (`pacewrightd`) para producción.
#
# Corre el daemon que un servicio vecino (p. ej. el server de CazaFacturas)
# usa por su socket de control `~/.pacewright/pw.sock`: el login de Claude
# (OAuth sin loopback: `anthropic_login_url`/`anthropic_login_submit`), el
# estado (`anthropic_status`) y el encolado de tareas. El socket y la credencial
# (`secrets.json`) viven en `~/.pacewright`, que en compose es un volumen
# compartido con ese vecino.
#
# Trae el CLI `claude`, que es lo que corre `claude_cli/run`. Se autentica con
# el token de `claude setup-token` que el daemon guarda en `secrets.json`
# (método `claude_token_set`) y que el adaptador le pasa al hijo como
# `CLAUDE_CODE_OAUTH_TOKEN`. Es el único token que puede gastar una suscripción
# Max: la API de mensajes lo rechaza, y el OAuth de `anthropic_login` —que sirve
# para esa API— no sirve para esto. Son dos credenciales distintas.
#
# Chrome NO va acá adentro: corre en su propio contenedor y esta imagen lo
# maneja por CDP con `pw-dom`. En el compose de CazaFacturas los dos comparten
# la pila de red del server, que es donde Chrome escucha su 9222 (sólo en
# loopback, aunque se le pida otra cosa).
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

# node:22 y no debian:bookworm-slim: es el MISMO bookworm, más un Node que le
# sirve al CLI `claude`, que pide >= 22 (el `nodejs` de bookworm es 18 y no
# arranca). Traer Node por NodeSource sobre la imagen pelada daba lo mismo con
# más pasos.
FROM node:22-bookworm-slim AS runtime
# Versión exacta a propósito: una ronda desatendida no es lugar para enterarse
# de un cambio de comportamiento del CLI. Subirla es un commit, no un rebuild.
ARG CLAUDE_CODE_VERSION=2.1.266
# ca-certificates: raíces TLS para el OAuth de Claude (api.anthropic.com).
# El uid 10001 coincide a propósito con el del server de CazaFacturas: el socket
# que crea el daemon queda accesible para ese vecino, que corre con ese uid.
# El CLI se instala ANTES del `USER`, porque npm global escribe en /usr/local.
# `claude --version` al final es la prueba de que quedó ejecutable: si no está,
# el build falla acá y no seis semanas después en una ronda desatendida.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g --no-fund --no-audit @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION} \
    && npm cache clean --force \
    && useradd --system --uid 10001 --create-home --home-dir /home/pacewright pacewright \
    && claude --version
COPY --from=builder /src/target/release/pacewrightd /usr/local/bin/pacewrightd
# El ojo de la ronda de autoría: muestra el DOM ya renderizado manejando el
# Chrome de al lado por CDP. Sin esto, Claude sólo ve el HTML inicial —los
# portales de facturación arman su formulario con JavaScript— y no puede
# escribir un locator que exista.
COPY packaging/pw-dom.mjs /usr/local/bin/pw-dom
RUN chmod +x /usr/local/bin/pw-dom
USER pacewright
ENV HOME=/home/pacewright
# El daemon crea ~/.pacewright (socket + secrets.json + pacewright.db) al
# arrancar. En compose ese directorio es un volumen compartido para el socket.
VOLUME ["/home/pacewright/.pacewright"]
ENTRYPOINT ["pacewrightd"]
