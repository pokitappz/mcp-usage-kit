import { writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const targets = await fetch("http://127.0.0.1:9222/json").then((response) => response.json());
const target = targets.find((candidate) => candidate.type === "page");
if (!target) throw new Error("No Chrome page target is available on port 9222");

const socket = new WebSocket(target.webSocketDebuggerUrl);
await new Promise((resolveOpen, reject) => {
  socket.addEventListener("open", resolveOpen, { once: true });
  socket.addEventListener("error", reject, { once: true });
});

let nextId = 1;
const pending = new Map();
const eventWaiters = new Map();

socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.id) {
    const waiter = pending.get(message.id);
    if (!waiter) return;
    pending.delete(message.id);
    if (message.error) waiter.reject(new Error(message.error.message));
    else waiter.resolve(message.result);
    return;
  }
  const waiters = eventWaiters.get(message.method);
  if (!waiters) return;
  eventWaiters.delete(message.method);
  for (const resolveEvent of waiters) resolveEvent(message.params);
});

function command(method, params = {}) {
  const id = nextId++;
  socket.send(JSON.stringify({ id, method, params }));
  return new Promise((resolveCommand, reject) => {
    pending.set(id, { resolve: resolveCommand, reject });
  });
}

function nextEvent(method) {
  return new Promise((resolveEvent) => {
    const waiters = eventWaiters.get(method) ?? [];
    waiters.push(resolveEvent);
    eventWaiters.set(method, waiters);
  });
}

await command("Page.enable");
await command("Runtime.enable");

const pageUrl = pathToFileURL(resolve("site/index.html"));
const results = [];
for (const width of [390, 768, 1440]) {
  for (const theme of ["light", "dark"]) {
    const height = width === 390 ? 844 : 1000;
    await command("Emulation.setDeviceMetricsOverride", {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: width < 768,
      screenWidth: width,
      screenHeight: height,
    });
    const loaded = nextEvent("Page.loadEventFired");
    await command("Page.navigate", { url: `${pageUrl.href}?theme=${theme}` });
    await loaded;

    const review = await command("Runtime.evaluate", {
      returnByValue: true,
      expression: `(() => {
        const root = document.documentElement;
        const overflow = [...document.querySelectorAll("body *")]
          .map((element) => ({ element, rect: element.getBoundingClientRect() }))
          .filter(({ rect }) => rect.width > 0 && (rect.right > innerWidth + 1 || rect.left < -1))
          .map(({ element, rect }) => ({
            element: element.tagName.toLowerCase(),
            className: String(element.className).slice(0, 80),
            left: Math.round(rect.left),
            right: Math.round(rect.right),
          }))
          .slice(0, 10);
        const task = document.querySelector('input[value="task"]');
        task.click();
        task.dispatchEvent(new Event("input", { bubbles: true }));
        const brand = document.querySelector(".site-header .brand");
        const brandRect = brand.getBoundingClientRect();
        return {
          width: innerWidth,
          theme: root.dataset.theme,
          scrollWidth: root.scrollWidth,
          overflow,
          scenario: document.querySelector("#scenario-title").textContent,
          deliveries: document.querySelector("#delivery-total").textContent,
          labels: document.querySelectorAll("label").length,
          controls: document.querySelectorAll("button, a, input").length,
          brand: {
            text: brand.textContent.trim(),
            display: getComputedStyle(brand).display,
            left: Math.round(brandRect.left),
            right: Math.round(brandRect.right),
            top: Math.round(brandRect.top),
          },
        };
      })()`,
    });
    const value = review.result.value;
    if (value.width !== width || value.scrollWidth > width || value.overflow.length > 0) {
      throw new Error(`Responsive overflow at ${width}px ${theme}: ${JSON.stringify(value)}`);
    }
    if (value.theme !== theme || value.scenario !== "Durable task") {
      throw new Error(`Interaction failed at ${width}px ${theme}: ${JSON.stringify(value)}`);
    }
    const screenshot = await command("Page.captureScreenshot", {
      format: "png",
      fromSurface: true,
      captureBeyondViewport: false,
    });
    await writeFile(`/tmp/usagekit-${width}-${theme}.png`, Buffer.from(screenshot.data, "base64"));
    results.push(value);
  }
}

socket.close();
console.log(JSON.stringify(results, null, 2));
