const root = document.documentElement;
const themeButton = document.querySelector("#theme-toggle");

function preferredTheme() {
  const requested = new URLSearchParams(window.location.search).get("theme");
  if (requested === "light" || requested === "dark") return requested;
  try {
    const saved = localStorage.getItem("mcp-usage-kit-theme");
    if (saved === "light" || saved === "dark") return saved;
  } catch {
    // Storage can be unavailable in privacy modes. The system preference remains usable.
  }
  return matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function setTheme(theme) {
  root.dataset.theme = theme;
  const isDark = theme === "dark";
  themeButton.setAttribute("aria-pressed", String(isDark));
  themeButton.setAttribute("aria-label", `Switch to ${isDark ? "light" : "dark"} theme`);
  document.querySelector('meta[name="theme-color"]').content = isDark ? "#09100c" : "#f4f6f1";
}

setTheme(preferredTheme());
themeButton.addEventListener("click", () => {
  const next = root.dataset.theme === "dark" ? "light" : "dark";
  setTheme(next);
  try {
    localStorage.setItem("mcp-usage-kit-theme", next);
  } catch {
    // A working theme toggle does not depend on persistent storage.
  }
});

const form = document.querySelector("#billing-form");
const monthlyCalls = document.querySelector("#monthly-calls");
const protocolHops = document.querySelector("#protocol-hops");
const units = document.querySelector("#units");
const unitPrice = document.querySelector("#unit-price");
const timeline = document.querySelector("#exchange-timeline");

const scenarios = {
  complete: {
    title: "Direct result",
    defaultHops: 0,
    explanation: "One terminal result produces one billable delivery.",
    steps: () => [{ name: "tools/call", state: "complete", kind: "billed" }],
    billable: 1,
  },
  mrtr: {
    title: "Multi-round-trip call",
    defaultHops: 2,
    explanation: "Every input_required response stays free. Only the final complete result is billed.",
    steps: (hops) => [
      ...Array.from({ length: hops }, () => ({ name: "tools/call", state: "input required", kind: "free" })),
      { name: "tools/call", state: "complete", kind: "billed" },
    ],
    billable: 1,
  },
  task: {
    title: "Durable task",
    defaultHops: 8,
    explanation: "Task creation and progress polls stay free. Completion inherits the originating tool price and bills once.",
    steps: (hops) => [
      { name: "tools/call", state: "task created", kind: "free" },
      ...Array.from({ length: hops }, () => ({ name: "tasks/get", state: "working", kind: "free" })),
      { name: "tasks/get", state: "completed", kind: "billed" },
    ],
    billable: 1,
  },
  error: {
    title: "Failed call",
    defaultHops: 0,
    explanation: "A JSON-RPC error delivers no billable result, so customer usage remains zero.",
    steps: () => [{ name: "tools/call", state: "error", kind: "error" }],
    billable: 0,
  },
};

const integer = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });
const currency = new Intl.NumberFormat("en-US", {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 0,
});

function selectedScenario() {
  return form.elements.scenario.value;
}

function setText(id, value) {
  document.querySelector(`#${id}`).textContent = value;
}

function renderTimeline(steps) {
  timeline.replaceChildren();
  const visibleSteps = steps.length > 12
    ? [...steps.slice(0, 5), { name: `+${steps.length - 10} polls`, state: "condensed", kind: "free" }, ...steps.slice(-5)]
    : steps;

  for (const step of visibleSteps) {
    const item = document.createElement("li");
    item.className = `step-${step.kind}`;
    const name = document.createElement("strong");
    name.textContent = step.name;
    const state = document.createElement("span");
    state.textContent = step.kind === "billed" ? `${step.state} · billed` : `${step.state} · free`;
    item.append(name, state);
    timeline.append(item);
  }
}

function renderDemo() {
  const scenario = scenarios[selectedScenario()];
  const calls = Number(monthlyCalls.value);
  const hops = Number(protocolHops.value);
  const unitCount = Number(units.value);
  const price = Number(unitPrice.value) / 100;
  const steps = scenario.steps(hops);
  const requestsPerCall = steps.length;
  const deliveries = calls * scenario.billable;
  const correctRevenue = deliveries * unitCount * price;
  const requestCountInvoice = calls * requestsPerCall * unitCount * price;
  const delta = Math.max(0, requestCountInvoice - correctRevenue);

  setText("monthly-calls-output", integer.format(calls));
  setText("protocol-hops-output", integer.format(hops));
  setText("units-output", integer.format(unitCount));
  setText("unit-price-output", `$${price.toFixed(2)}`);
  setText("scenario-title", scenario.title);
  setText("request-total", integer.format(calls * requestsPerCall));
  setText("delivery-total", integer.format(deliveries));
  setText("correct-revenue", currency.format(correctRevenue));
  setText("billing-delta", currency.format(delta));
  setText("demo-explanation", scenario.explanation);
  setText("delta-label", delta === 0 ? "Billing mismatch" : "Overbilling prevented");
  renderTimeline(steps);
}

form.addEventListener("input", (event) => {
  if (event.target.name === "scenario") {
    protocolHops.value = String(scenarios[selectedScenario()].defaultHops);
  }
  renderDemo();
});

renderDemo();

const copyButton = document.querySelector("#copy-command");
copyButton.addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(copyButton.dataset.command);
    copyButton.textContent = "Copied";
  } catch {
    copyButton.textContent = "Select command";
  }
  window.setTimeout(() => { copyButton.textContent = "Copy"; }, 1800);
});
