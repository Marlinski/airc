# ─── build stage ──────────────────────────────────────────────────────────────
FROM node:22-alpine AS build

WORKDIR /app

COPY package.json package-lock.json ./
RUN npm install

COPY . .
RUN npm run build

# ─── serve stage ──────────────────────────────────────────────────────────────
FROM nginx:1.27-alpine AS serve

# remove default nginx page
RUN rm -rf /usr/share/nginx/html/*

COPY --from=build /app/dist /usr/share/nginx/html

# nginx config: serve SPA, gzip, cache assets
COPY nginx.conf /etc/nginx/conf.d/default.conf

EXPOSE 80

CMD ["nginx", "-g", "daemon off;"]
