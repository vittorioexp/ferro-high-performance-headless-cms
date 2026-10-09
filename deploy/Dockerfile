FROM rust:1.82-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p ferro-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 ferro && mkdir -p /data/uploads && chown ferro /data/uploads
COPY --from=build /src/target/release/ferro /usr/local/bin/ferro
COPY --from=build /src/admin /usr/share/ferro/admin
# mount your extensions here: -v ./extensions:/usr/share/ferro/extensions:ro
RUN mkdir -p /usr/share/ferro/extensions
USER ferro
ENV FERRO_BIND=0.0.0.0:8055 FERRO_ADMIN_DIR=/usr/share/ferro/admin FERRO_EXTENSIONS_DIR=/usr/share/ferro/extensions
EXPOSE 8055
HEALTHCHECK CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8055"]
ENTRYPOINT ["ferro"]
CMD ["serve"]
