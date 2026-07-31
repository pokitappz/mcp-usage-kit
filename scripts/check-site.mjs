import { readFile, access } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const site = resolve(root, "site");
const required = [
  "index.html",
  "styles.css",
  "app.js",
  "404.html",
  "robots.txt",
  "sitemap.xml",
  "manifest.webmanifest",
  "assets/favicon.svg",
];

await Promise.all(required.map((path) => access(resolve(site, path))));

const html = await readFile(resolve(site, "index.html"), "utf8");
const ids = new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((match) => match[1]));
const links = [...html.matchAll(/\shref="([^"]+)"/g)].map((match) => match[1]);

for (const link of links) {
  if (link.startsWith("#") && !ids.has(link.slice(1))) {
    throw new Error(`Missing anchor target: ${link}`);
  }
}

if (!html.includes('name="description"') || !html.includes('rel="canonical"')) {
  throw new Error("The landing page is missing required search metadata");
}

if (/[\u2013\u2014]/u.test(html)) {
  throw new Error("The landing page contains a forbidden dash character");
}

console.log(`Checked ${required.length} assets and ${links.length} links`);
