FROM node:22-alpine

WORKDIR /app
COPY package.json server.mjs ./

ENV NODE_ENV=production
ENV PORT=8787
EXPOSE 8787
USER node

HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${PORT}/healthz" >/dev/null || exit 1

CMD ["node", "server.mjs"]
