# Build the SPA, then serve it with nginx. Defaults to a same-origin relative
# API base URL (/api/v1) — whatever serves this image (locally or on
# demo.fms.bizlution.ai) must reverse-proxy /api/v1 to the real backend, so the
# browser never makes a cross-origin call and there's no CORS to configure on
# the production API. Only pass --build-arg VITE_API_BASE_URL=https://... if
# you specifically want the old cross-origin behavior instead.
FROM node:20-alpine AS build
WORKDIR /app
COPY package.json package-lock.json .npmrc ./
RUN npm ci
COPY . .

# Baked in at build time (Vite inlines VITE_* vars into the JS bundle).
ARG VITE_API_BASE_URL=/api/v1
RUN echo "VITE_API_BASE_URL=${VITE_API_BASE_URL}" > .env.production.local && npm run build

FROM nginx:alpine
COPY --from=build /app/dist /usr/share/nginx/html
COPY docker/nginx.conf /etc/nginx/conf.d/default.conf
EXPOSE 80
