# --- Build the localith binary ---
FROM rust:1-bookworm AS build

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY public ./public
RUN cargo build --release --locked

# --- Runtime: localith + agent-browser + Chrome ---
FROM node:24-bookworm-slim

# agent-browser is distributed through npm. This installs its CLI, then
# Chrome for Testing plus the Linux system libraries Chrome needs.
RUN npm install -g agent-browser@0.41.2 \
    && agent-browser install --with-deps \
    && rm -rf /var/lib/apt/lists/* /root/.npm

COPY --from=build /src/target/release/localith /usr/local/bin/localith

WORKDIR /app

# Chrome cannot use its sandbox as root inside a container.
# HOST=0.0.0.0 makes the server reachable through the published port.
ENV AGENT_BROWSER_ARGS=--no-sandbox \
    HOST=0.0.0.0 \
    PORT=3000

EXPOSE 3000
VOLUME ["/app/reports"]

CMD ["localith", "serve"]
