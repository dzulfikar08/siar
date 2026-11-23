# siar is one static binary: build it, then copy it into a small image.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY rust/ .
RUN cargo build --release && cp target/release/siar /siar

FROM alpine:3
# tzdata: best_times and other per-timezone reports read the zone database.
RUN apk add --no-cache tzdata
COPY --from=build /siar /usr/local/bin/siar
ENV SIAR_DATA_DIR=/data
VOLUME /data
ENTRYPOINT ["siar"]
# stdio MCP by default; `serve` for the HTTP server (set SIAR_API_TOKEN, SIAR_VAULT_KEY, SIAR_PUBLIC_URL).
CMD ["mcp"]
