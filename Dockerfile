FROM rust:1.96-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/* && useradd -r -u 10001 app && mkdir /data && chown app /data
COPY --from=build /app/target/release/picore /usr/local/bin/picore
USER app
ENV DATABASE_PATH=/data/cache.db BIND_ADDRESS=0.0.0.0:3000
EXPOSE 3000
CMD ["picore"]
