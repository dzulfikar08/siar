# siar has no runtime dependencies: the image is Bun plus the source.
FROM oven/bun:1-alpine
WORKDIR /app
COPY package.json ./
COPY src ./src
ENV SIAR_DATA_DIR=/data
VOLUME /data
ENTRYPOINT ["bun", "src/index.ts"]
# stdio MCP by default; `serve` for the HTTP server (set SIAR_API_TOKEN, SIAR_VAULT_KEY, SIAR_PUBLIC_URL).
CMD ["mcp"]
