#!/usr/bin/env node

import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const siteRoot = path.join(repositoryRoot, "site");
const host = "127.0.0.1";
const port = Number.parseInt(process.env.OCTOSTORE_SITE_PORT ?? "4173", 10);

const contentTypes = new Map([
  [".css", "text/css; charset=utf-8"],
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".json", "application/json; charset=utf-8"],
  [".md", "text/markdown; charset=utf-8"],
  [".png", "image/png"],
  [".svg", "image/svg+xml"],
  [".txt", "text/plain; charset=utf-8"],
  [".yaml", "application/yaml; charset=utf-8"],
]);

function resolveRequestPath(requestUrl) {
  const pathname = decodeURIComponent(new URL(requestUrl, `http://${host}:${port}`).pathname);
  const relative = pathname.endsWith("/") ? `${pathname}index.html` : pathname;
  const candidate = path.resolve(siteRoot, `.${relative}`);
  if (candidate !== siteRoot && !candidate.startsWith(`${siteRoot}${path.sep}`)) return undefined;
  return candidate;
}

const server = http.createServer((request, response) => {
  const file = resolveRequestPath(request.url ?? "/");
  if (!file || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
    response.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    response.end("Not found\n");
    return;
  }

  response.writeHead(200, {
    "cache-control": "no-store",
    "content-type": contentTypes.get(path.extname(file)) ?? "application/octet-stream",
  });
  fs.createReadStream(file).pipe(response);
});

server.listen(port, host);

for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
