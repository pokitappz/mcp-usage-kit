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
const errorHtml = await readFile(resolve(site, "404.html"), "utf8");
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

for (const [name, page] of [["index.html", html], ["404.html", errorHtml]]) {
  const csp = page.match(/<meta\s+[^>]*http-equiv="Content-Security-Policy"[^>]*>/iu)?.[0];
  if (!csp || !csp.includes("default-src 'self'") || /unsafe-(?:inline|eval)/iu.test(csp)) {
    throw new Error(`${name} is missing a restrictive Content Security Policy`);
  }
  if (/<script\b(?![^>]*\bsrc\s*=)[^>]*>/iu.test(page)) {
    throw new Error(`${name} contains an inline script`);
  }
  if (/\son[a-z][\w:-]*\s*=/iu.test(page) || /(?:href|src)\s*=\s*["']javascript:/iu.test(page)) {
    throw new Error(`${name} contains inline executable content`);
  }
  if (/<style\b|\sstyle\s*=/iu.test(page)) {
    throw new Error(`${name} contains inline styling`);
  }
  if (/[\u2013\u2014]/u.test(page)) {
    throw new Error(`${name} contains a forbidden dash character`);
  }
}

console.log(`Checked ${required.length} assets, 2 HTML policies, and ${links.length} links`);
