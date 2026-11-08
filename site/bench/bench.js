// The benchmark page: draws bench/history.json, one record per Rewind
// build, as small multiples over the builds. tools/bench.py writes the
// file; this only reads it. Plain SVG, no library: a line over a dozen
// points does not need one.

const HISTORY_URL = "history.json";
const SVG_NS = "http://www.w3.org/2000/svg";

// The verdict rule tools/bench.py's compare uses: the medians differ by
// more than this share and the two ranges do not overlap.
const MIN_CHANGE = 0.03;

const MICROSECONDS = 1e6;
const MILLISECONDS = 1e3;

// Chart geometry, in pixels.
const PLOT_HEIGHT = 130;
const PAD = { top: 14, right: 44, bottom: 24, left: 44 };
const DOT_RADIUS = 4;
const TICK_COUNT = 4;
// The least room an x label needs before labels are thinned.
const X_LABEL_ROOM = 34;
const RESIZE_DELAY_MS = 120;

// Releases where something the charts show landed. The numeral is drawn on
// every chart at that build; the list under the charts says what it was.
// Keyed by label, so a record's position is found by lookup.
const COMMIT_URL = "https://github.com/fzakaria/rewindvm/commit/";
const commit = (hash) =>
  `<a href="${COMMIT_URL}${hash}"><code>${hash}</code></a>`;
const MILESTONES = [
  {
    label: "v0.3.0",
    html: `rewind check starts each schedule from the unperturbed run's latest keyframe before the schedule begins, where it booted and ran from the start before (${commit("53cf7ac7")}). A window late in a long build is checked without running up to it again. A short program pays for restoring a keyframe, all of guest memory, where booting cost less, and the unperturbed run now records keyframes, which every other schedule waits for: check of the Nix build, which finds no difference, went from 47 s to 104 s here, where a keyframe costs about 200 ms. 0.12.0 won most of it back.`,
  },
  {
    label: "v0.5.0",
    html: `Restoring a keyframe reads each page of its chain once, not once for every keyframe that names it (${commit("841ab883")}). The late seek takes 15% less time.`,
  },
  {
    label: "v0.10.0",
    html: `replay compares the whole machine at every keyframe of the run as well as its events (${commit("725de7f4")}), which catches a host that ran the guest another way without the guest noticing. It costs a third more wall time and half again the memory; <code>--machine last</code> or <code>none</code> compares less.`,
  },
  {
    label: "v0.11.0",
    html: `Restores read pages on every core (${commit("f7fc2dbf")}) and straight from a keyframe's bytes (${commit("f8dbdad6")}), and guest memory is backed by 2 MiB pages where the host allows (${commit("151744b5")}). The late seek takes a fifth of its time, check of a short program is back where it was before 0.3.0, and a short run is 22% faster. Which commit gave how much is not measured.`,
  },
  {
    label: "v0.12.0",
    html: `Keyframes are spaced by the guest's work instead of wall time (${commit("67dd7d7a")}), which counted each keyframe's own time and gave this machine about 300 for hello, and the page store keeps one compression context where the static build made one for every page (${commit("2c778eb4")}). hello records in 26.6 s against 81.3 s, with 39 keyframes against 291, and check of the Nix build takes 57 s, against 116 s. Hashing and compressing on several threads (${commit("8f714365")}) gave about a tenth more. A replay compares only the pages written since the last match (${commit("515bc488")}) and is back within a second of 0.9.0's. The late seek is a fifth slower, since a seek can now replay up to a quarter second of the reference laptop's work, which takes longer here.`,
  },
  {
    label: "v0.13.0",
    html: `Every context switch is an exit of its own, with a record naming the task that takes the CPU (${commit("08471c00")}), encoded in a few bytes each (${commit("b35d6052")}), and the guest's scheduler tick no longer reads the clock at every switch. A short run makes twice the steps and hello a sixth fewer. A check of the ping-pong program, where every token pass is now an exit, takes 38% more CPU; hello and check of the Nix build measured 8% and 9% faster, within the rounds' spread. The first run in a new home now checks the branch counter against a known answer before trusting it (${commit("2c0a676d")}), which is the whole of the 1.7 s more on echo-cold, and a run keeps 6 MiB more resident.`,
  },
];

// What each metric reads from a sample.
const METRICS = [
  {
    key: "wall",
    label: "Wall time",
    unit: "s",
    get: (s) => s.wall,
    note: "Seconds from starting rewind to its exit.",
  },
  {
    key: "cpu",
    label: "CPU time",
    unit: "s",
    get: (s) => s.user + s.sys,
    note: "User plus system seconds of rewind and every process it started. check runs four VMs at once, so its CPU time is more than its wall time.",
  },
  {
    key: "rss",
    label: "Peak memory",
    unit: "MiB",
    get: (s) => s.rss_mb,
    note: "The peak resident set of the largest process, from GNU time. Guest memory counts once the guest touches it.",
  },
  {
    key: "disk",
    label: "Run size",
    unit: "MiB",
    get: (s) => s.run_mb,
    note: "The run's own directory, its trace and keyframes. The pages keyframes name live in the shared page store, under Short runs and long runs.",
  },
  {
    key: "keyframes",
    label: "Keyframes",
    unit: "",
    get: (s) => s.keyframes,
    note: "Keyframes the run kept. Through 0.11.0 recording took one every quarter second of wall time, so a slower recording kept more. From 0.12.0 it takes one every quarter second of the guest's work, at the same steps on every host, and one where the job exits.",
  },
  {
    key: "steps",
    label: "Steps",
    unit: "",
    get: (s) => s.steps,
    note: "Exits the guest made. A change here is a change to the guest or to what the VM shows it, not to speed. From 0.13.0 every context switch is a step and the scheduler tick no longer reads the clock, so a short run has twice the steps and hello a sixth fewer.",
  },
];

// The metrics the release comparison shows.
const SCORECARD = ["wall", "cpu", "rss", "disk"];

const GROUPS = {
  short: {
    title: "Short runs",
    text: "A run that does almost nothing: what boot and teardown cost, the first time and after.",
  },
  record: {
    title: "Recording",
    text: "Long runs, each doing one kind of work. The gray line is the same work run natively.",
  },
  look: {
    title: "Looking at a run",
    text: "Commands that bring a recorded run back: replaying it whole, and reading a file late in it.",
  },
  explore: {
    title: "Exploring schedules",
    text: "rewind check running a short multithreaded program, and a Nix build, under several schedules at once.",
  },
};

const state = { history: null, metric: "wall", base: null, head: null };

// --- numbers -------------------------------------------------------------

function median(values) {
  if (values.length === 0) {
    return null;
  }
  const sorted = [...values].sort((a, b) => a - b);
  const mid = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[mid] : (sorted[mid - 1] + sorted[mid]) / 2;
}

// A summary of values: median, range and count.
function summary(values) {
  if (values.length === 0) {
    return null;
  }
  return {
    median: median(values),
    min: Math.min(...values),
    max: Math.max(...values),
    n: values.length,
  };
}

function verdict(base, head) {
  if (!base || !head || base.median === 0) {
    return null;
  }
  const change = (head.median - base.median) / base.median;
  if (Math.abs(change) < MIN_CHANGE) {
    return { change, kind: "same" };
  }
  if (change > 0 && head.min > base.max) {
    return { change, kind: "worse" };
  }
  if (change < 0 && head.max < base.min) {
    return { change, kind: "better" };
  }
  return { change, kind: "same" };
}

function fmt(value, unit) {
  if (value === null || value === undefined || Number.isNaN(value)) {
    return "–";
  }
  if (unit === "") {
    return Math.round(value).toLocaleString("en-US");
  }
  const abs = Math.abs(value);
  let digits = 0;
  if (abs < 10) {
    digits = 2;
  } else if (abs < 100) {
    digits = 1;
  }
  const text = value.toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
  return unit === "×" ? `${text}×` : `${text} ${unit}`;
}

function pct(change) {
  const sign = change > 0 ? "+" : "";
  return `${sign}${(change * 100).toFixed(Math.abs(change) < 0.1 ? 1 : 0)}%`;
}

function verdictHtml(v) {
  if (!v) {
    return "";
  }
  if (v.kind === "worse") {
    return `<span class="change worse">▲ ${pct(v.change)} worse</span>`;
  }
  if (v.kind === "better") {
    return `<span class="change better">▼ ${pct(v.change)} better</span>`;
  }
  return `<span class="change same">${pct(v.change)} same</span>`;
}

function escapeHtml(text) {
  return String(text).replace(
    /[&<>"]/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c],
  );
}

// Backticks in the tool's descriptions become code.
function prose(text) {
  return escapeHtml(text).replace(/`([^`]+)`/g, "<code>$1</code>");
}

function shortLabel(label) {
  return label.replace(/^v/, "").replace(/\.0$/, "");
}

// --- reading the history ------------------------------------------------

function samplesOf(record, workload) {
  return (record.workloads[workload] || []).filter((s) => !("error" in s));
}

function errorsOf(record, workload) {
  return (record.workloads[workload] || []).filter((s) => "error" in s);
}

function metricSummary(record, workload, metric) {
  const values = samplesOf(record, workload)
    .map(metric.get)
    .filter((v) => v !== undefined && v !== null);
  return summary(values);
}

function nativeSummary(record, workload) {
  const values = ((record.native || {})[workload] || [])
    .filter((s) => !("error" in s))
    .map((s) => s.wall);
  return summary(values);
}

// A value worked out per round from samples of the same round, so a
// derived figure has a range of its own.
function perRound(record, fn) {
  const rounds = {};
  const add = (kind, name, sample) => {
    if ("error" in sample) {
      return;
    }
    rounds[sample.round] ??= {};
    rounds[sample.round][`${kind}:${name}`] = sample;
  };
  for (const [name, samples] of Object.entries(record.workloads)) {
    samples.forEach((s) => add("run", name, s));
  }
  for (const [name, samples] of Object.entries(record.native || {})) {
    samples.forEach((s) => add("native", name, s));
  }
  const values = [];
  for (const round of Object.values(rounds)) {
    const value = fn(
      (name) => round[`run:${name}`],
      (name) => round[`native:${name}`],
    );
    if (value !== null && value !== undefined && Number.isFinite(value)) {
      values.push(value);
    }
  }
  return summary(values);
}

// --- charts ---------------------------------------------------------------

function el(name, attrs = {}, parent = null) {
  const node = document.createElementNS(SVG_NS, name);
  for (const [k, v] of Object.entries(attrs)) {
    node.setAttribute(k, v);
  }
  if (parent) {
    parent.appendChild(node);
  }
  return node;
}

function niceStep(max) {
  const raw = max / TICK_COUNT;
  const power = 10 ** Math.floor(Math.log10(raw));
  for (const m of [1, 2, 2.5, 5, 10]) {
    if (m * power >= raw) {
      return m * power;
    }
  }
  return 10 * power;
}

const tip = () => document.getElementById("tip");

function showTip(html, event, anchor) {
  const box = tip();
  box.innerHTML = html;
  box.hidden = false;
  const rect = anchor.getBoundingClientRect();
  // Beside the pointer, or under the column when focus came by keyboard.
  const x = event ? event.pageX : rect.left + window.scrollX + rect.width / 2;
  const y = event ? event.pageY : rect.top + window.scrollY + rect.height / 2;
  const width = box.offsetWidth;
  const left = Math.min(
    Math.max(8, x + 14),
    document.documentElement.clientWidth - width - 8,
  );
  box.style.left = `${left}px`;
  box.style.top = `${y + 16}px`;
}

function hideTip() {
  tip().hidden = true;
}

// One chart: a median line over the records with the range as a band,
// an optional gray native line, and a hover column per record.
// spec: { title, sub, unit, points: [{ label, s, failed, tipExtra }], native: [summary|null] }
function drawChart(parent, spec) {
  const card = document.createElement("div");
  card.className = "chart";
  const last = [...spec.points].reverse().find((p) => p.s);
  card.innerHTML = `<div class="head"><h3>${escapeHtml(spec.title)}</h3><span class="now">${
    last ? fmt(last.s.median, spec.unit) : ""
  }</span></div><p class="sub">${escapeHtml(spec.sub || "")}</p>`;
  parent.appendChild(card);

  if (!spec.points.some((p) => p.s)) {
    card.insertAdjacentHTML("beforeend", `<div class="empty">No samples</div>`);
    return;
  }

  const width = Math.max(220, card.clientWidth - 28);
  const height = PLOT_HEIGHT + PAD.top + PAD.bottom;
  const svg = el(
    "svg",
    {
      viewBox: `0 0 ${width} ${height}`,
      height,
      role: "img",
      "aria-label": spec.title,
    },
    card,
  );
  const plotW = width - PAD.left - PAD.right;
  const n = spec.points.length;
  const x = (i) => PAD.left + (n === 1 ? plotW / 2 : (i * plotW) / (n - 1));

  // The y scale includes zero, so a change reads at its true size; a
  // difference that dips below zero takes the scale below it.
  let max = 0;
  let min = 0;
  spec.points.forEach((p) => {
    if (p.s) {
      max = Math.max(max, p.s.max);
      min = Math.min(min, p.s.min);
    }
  });
  (spec.native || []).forEach((s) => s && (max = Math.max(max, s.max)));
  const step = niceStep(max - min || 1);
  const top = Math.ceil((max * 1.05) / step) * step || step;
  const bottom = Math.floor(min / step) * step;
  const y = (v) =>
    PAD.top + PLOT_HEIGHT - ((v - bottom) / (top - bottom)) * PLOT_HEIGHT;

  // Grid and y ticks.
  for (let v = bottom; v <= top + step / 2; v += step) {
    el(
      "line",
      {
        class: "grid-line",
        x1: PAD.left,
        x2: width - PAD.right,
        y1: y(v),
        y2: y(v),
      },
      svg,
    );
    const label = el(
      "text",
      { class: "tick", x: PAD.left - 6, y: y(v) + 4, "text-anchor": "end" },
      svg,
    );
    label.textContent =
      spec.unit === "×"
        ? `${+v.toFixed(2)}×`
        : (+v.toFixed(3)).toLocaleString("en-US");
  }

  // x labels, thinned to what fits, the last always shown.
  const every = Math.max(1, Math.ceil((n * X_LABEL_ROOM) / plotW));
  spec.points.forEach((p, i) => {
    if (i % every !== 0 && i !== n - 1) {
      return;
    }
    if (i !== n - 1 && n - 1 - i < every) {
      return;
    }
    const t = el(
      "text",
      { class: "tick", x: x(i), y: height - 6, "text-anchor": "middle" },
      svg,
    );
    t.textContent = shortLabel(p.label);
  });

  // Milestones.
  MILESTONES.forEach((m, k) => {
    const i = spec.points.findIndex((p) => p.label === m.label);
    if (i < 0) {
      return;
    }
    const g = el("g", { class: "milestone" }, svg);
    el(
      "line",
      { x1: x(i), x2: x(i), y1: PAD.top, y2: PAD.top + PLOT_HEIGHT },
      g,
    );
    const t = el("text", { x: x(i) + 3, y: PAD.top - 3 }, g);
    t.textContent = String(k + 1);
  });

  // The native line, where the workload has one.
  if (spec.native && spec.native.some(Boolean)) {
    const pts = spec.native
      .map((s, i) => (s ? `${x(i)},${y(s.median)}` : null))
      .filter(Boolean);
    el("polyline", { class: "native", points: pts.join(" ") }, svg);
    const lastNative = [...spec.native].reverse().find(Boolean);
    const t = el(
      "text",
      { class: "native-label", x: x(n - 1) + 6, y: y(lastNative.median) + 4 },
      svg,
    );
    t.textContent = "native";
  }

  // The band and the median line, broken where a record has no samples.
  const runs = [];
  let current = [];
  spec.points.forEach((p, i) => {
    if (!p.s) {
      if (current.length) {
        runs.push(current);
      }
      current = [];
      return;
    }
    current.push([i, p.s]);
  });
  if (current.length) {
    runs.push(current);
  }
  for (const r of runs) {
    const upper = r.map(([i, s]) => `${x(i)},${y(s.max)}`);
    const lower = r.map(([i, s]) => `${x(i)},${y(s.min)}`).reverse();
    if (r.length > 1) {
      el(
        "polygon",
        { class: "band", points: [...upper, ...lower].join(" ") },
        svg,
      );
    }
    el(
      "polyline",
      {
        class: "median",
        points: r.map(([i, s]) => `${x(i)},${y(s.median)}`).join(" "),
      },
      svg,
    );
  }
  spec.points.forEach((p, i) => {
    if (p.s) {
      el(
        "circle",
        { class: "dot", cx: x(i), cy: y(p.s.median), r: DOT_RADIUS },
        svg,
      );
      return;
    }
    if (p.failed) {
      el(
        "circle",
        { class: "dot failed", cx: x(i), cy: y(0), r: DOT_RADIUS },
        svg,
      );
    }
  });

  // Hover and focus: a column per record, as wide as the gap between.
  const crosshair = el(
    "line",
    { class: "crosshair", y1: PAD.top, y2: PAD.top + PLOT_HEIGHT },
    svg,
  );
  const colW = n === 1 ? plotW : plotW / (n - 1);
  spec.points.forEach((p, i) => {
    const hit = el(
      "rect",
      {
        class: "hit",
        x: x(i) - colW / 2,
        y: 0,
        width: colW,
        height,
        tabindex: 0,
        "aria-label": `${p.label}`,
      },
      svg,
    );
    const show = (event) => {
      crosshair.setAttribute("x1", x(i));
      crosshair.setAttribute("x2", x(i));
      crosshair.style.opacity = 1;
      showTip(tipHtml(spec, i), event, hit);
    };
    const hide = () => {
      crosshair.style.opacity = 0;
      hideTip();
    };
    hit.addEventListener("mousemove", show);
    hit.addEventListener("focus", () => show(null));
    hit.addEventListener("mouseleave", hide);
    hit.addEventListener("blur", hide);
  });
}

function tipHtml(spec, i) {
  const p = spec.points[i];
  const lines = [
    `<b>${escapeHtml(p.label)}</b>${p.date ? ` · ${p.date.slice(0, 10)}` : ""}`,
  ];
  if (!p.s) {
    lines.push(
      p.failed
        ? `failed: ${escapeHtml(p.failed.slice(0, 160))}`
        : "not measured",
    );
    return lines.join("<br>");
  }
  lines.push(`median <b>${fmt(p.s.median, spec.unit)}</b>`);
  if (p.s.n > 1) {
    lines.push(
      `range ${fmt(p.s.min, spec.unit)} – ${fmt(p.s.max, spec.unit)}, ${p.s.n} rounds`,
    );
  }
  const prev = spec.points
    .slice(0, i)
    .reverse()
    .find((q) => q.s);
  if (prev) {
    lines.push(
      `against ${escapeHtml(prev.label)}: ${verdictHtml(verdict(prev.s, p.s))}`,
    );
  }
  const native = spec.native && spec.native[i];
  if (native) {
    lines.push(
      `native ${fmt(native.median, spec.unit)}, ${(p.s.median / native.median).toFixed(1)}× in the VM`,
    );
  }
  if (p.failed) {
    lines.push(`some rounds failed: ${escapeHtml(p.failed.slice(0, 120))}`);
  }
  return lines.join("<br>");
}

// --- sections -------------------------------------------------------------

function workloads() {
  return state.history.workloads;
}

function records() {
  return state.history.records;
}

function metricByKey(key) {
  return METRICS.find((m) => m.key === key);
}

function renderMachine() {
  const recs = records();
  const machines = [...new Set(recs.map((r) => r.machine.host))];
  const m = recs[recs.length - 1].machine;
  const loads = recs
    .flatMap((r) =>
      Object.values(r.workloads)
        .flat()
        .map((s) => s.load),
    )
    .filter(Boolean);
  const rounds = Math.max(...recs.map((r) => r.rounds));
  document.getElementById("machine").textContent =
    `${recs.length} builds on ${machines.join(", ")}: ${m.cpu}, ${m.cores} threads, ${m.memory_gib} GiB, Linux ${m.kernel}. ` +
    `Up to ${rounds} rounds each. The machine is shared: its load average ran ${Math.min(...loads).toFixed(0)} to ${Math.max(...loads).toFixed(0)} while these were measured.`;
}

function renderScorecard() {
  const recs = records();
  const base = recs.find((r) => r.label === state.base);
  const head = recs.find((r) => r.label === state.head);
  const metrics = SCORECARD.map(metricByKey);
  let html = `<thead><tr><th>Workload</th>${metrics
    .map((m) => `<th class="num">${m.label}</th>`)
    .join("")}</tr></thead><tbody>`;
  for (const w of workloads()) {
    html += `<tr><th>${escapeHtml(w.title)}<small>${escapeHtml(w.name)}</small></th>`;
    for (const m of metrics) {
      const b = metricSummary(base, w.name, m);
      const h = metricSummary(head, w.name, m);
      if (!h) {
        html += `<td class="num">${errorsOf(head, w.name).length ? '<span class="worse">failed</span>' : "–"}</td>`;
        continue;
      }
      html += `<td class="num">${fmt(h.median, m.unit)}${verdictHtml(verdict(b, h))}</td>`;
    }
    html += "</tr>";
  }
  document.getElementById("scorecard").innerHTML = `${html}</tbody>`;
}

function renderSelects() {
  const labels = records().map((r) => r.label);
  for (const id of ["base", "head"]) {
    const select = document.getElementById(id);
    select.innerHTML = labels
      .map((l) => `<option>${escapeHtml(l)}</option>`)
      .join("");
    select.value = state[id];
    select.onchange = () => {
      state[id] = select.value;
      renderScorecard();
    };
  }
}

function renderMetricTabs() {
  const box = document.getElementById("metrics");
  box.innerHTML = "";
  for (const m of METRICS) {
    const b = document.createElement("button");
    b.type = "button";
    b.role = "tab";
    b.textContent = m.label;
    b.setAttribute("aria-selected", String(m.key === state.metric));
    b.onclick = () => {
      state.metric = m.key;
      renderMetricTabs();
      renderGroups();
      renderRecords();
    };
    box.appendChild(b);
  }
  document.getElementById("metric-note").textContent = metricByKey(
    state.metric,
  ).note;
}

function renderGroups() {
  const metric = metricByKey(state.metric);
  const box = document.getElementById("groups");
  box.innerHTML = "";
  const withNative = metric.key === "wall";
  for (const [key, group] of Object.entries(GROUPS)) {
    const members = workloads().filter((w) => w.group === key);
    const charts = members.filter((w) =>
      records().some((r) => metricSummary(r, w.name, metric)),
    );
    if (charts.length === 0) {
      continue;
    }
    const section = document.createElement("div");
    section.className = "group";
    section.innerHTML = `<h3>${group.title}</h3><p>${group.text}</p>`;
    const grid = document.createElement("div");
    grid.className = "grid";
    section.appendChild(grid);
    box.appendChild(section);
    for (const w of charts) {
      drawChart(grid, {
        title: w.title,
        sub: w.name,
        unit: metric.unit,
        points: records().map((r) => ({
          label: r.label,
          date: r.date,
          s: metricSummary(r, w.name, metric),
          failed: errorsOf(r, w.name)[0]?.error,
        })),
        native:
          withNative && w.native
            ? records().map((r) => nativeSummary(r, w.name))
            : null,
      });
    }
  }
}

// Figures worked out from several workloads, round by round.
const DERIVED = [
  {
    title: "Computation in the VM",
    sub: "compute wall ÷ native",
    unit: "×",
    fn: (run, native) =>
      run("compute") &&
      native("compute") &&
      run("compute").wall / native("compute").wall,
  },
  {
    title: "Processes in the VM",
    sub: "spawn wall ÷ native",
    unit: "×",
    fn: (run, native) =>
      run("spawn") &&
      native("spawn") &&
      run("spawn").wall / native("spawn").wall,
  },
  {
    title: "Memory writes in the VM",
    sub: "untar wall ÷ native",
    unit: "×",
    fn: (run, native) =>
      run("untar") &&
      native("untar") &&
      run("untar").wall / native("untar").wall,
  },
  {
    title: "Cost of a step",
    sub: "Nix build without keyframes, less a short run, per step",
    unit: "µs",
    fn: (run) =>
      run("hello-nokf") &&
      run("echo") &&
      ((run("hello-nokf").wall - run("echo").wall) / run("hello-nokf").steps) *
        MICROSECONDS,
  },
  {
    title: "Keyframes in a Nix build",
    sub: "hello wall ÷ hello-nokf wall",
    unit: "×",
    fn: (run) =>
      run("hello") &&
      run("hello-nokf") &&
      run("hello").wall / run("hello-nokf").wall,
  },
  {
    title: "Cost of a keyframe",
    sub: "hello less hello-nokf, per keyframe",
    unit: "ms",
    fn: (run) =>
      run("hello") &&
      run("hello-nokf") &&
      run("hello").keyframes &&
      ((run("hello").wall - run("hello-nokf").wall) / run("hello").keyframes) *
        MILLISECONDS,
  },
  {
    title: "First run's extra",
    sub: "echo-cold less echo: packing the root, a new store, and from 0.13.0 the branch counter's self-test",
    unit: "s",
    fn: (run) =>
      run("echo-cold") &&
      run("echo") &&
      run("echo-cold").wall - run("echo").wall,
  },
  {
    title: "First Nix build's extra",
    sub: "hello-cold less hello: packing the build's inputs",
    unit: "s",
    fn: (run) =>
      run("hello-cold") &&
      run("hello") &&
      run("hello-cold").wall - run("hello").wall,
  },
];

function renderDerived() {
  const grid = document.getElementById("derived");
  grid.innerHTML = "";
  for (const d of DERIVED) {
    drawChart(grid, {
      title: d.title,
      sub: d.sub,
      unit: d.unit,
      points: records().map((r) => ({
        label: r.label,
        date: r.date,
        s: perRound(r, d.fn),
      })),
    });
  }

  // What a round leaves in REWIND_HOME, every workload's runs together.
  drawChart(grid, {
    title: "REWIND_HOME after a round",
    sub: "page store, images and runs",
    unit: "MiB",
    points: records().map((r) => ({
      label: r.label,
      date: r.date,
      s: summary((r.disk || []).map((d) => d.store + d.images + d.runs)),
    })),
  });
}

function renderMilestones() {
  const list = document.getElementById("milestones");
  list.innerHTML = MILESTONES.map(
    (m) =>
      `<li><span class="rel">${escapeHtml(m.label)}</span>: ${m.html}</li>`,
  ).join("");
}

function renderRecords() {
  const metric = metricByKey(state.metric);
  const ws = workloads();
  let html = `<thead><tr><th>Build</th><th>Date</th>${ws
    .map((w) => `<th class="num">${escapeHtml(w.name)}</th>`)
    .join("")}</tr></thead><tbody>`;
  for (const r of records()) {
    html += `<tr><th>${escapeHtml(r.label)}</th><td>${(r.date || "").slice(0, 10)}</td>`;
    for (const w of ws) {
      const s = metricSummary(r, w.name, metric);
      html += `<td class="num">${s ? fmt(s.median, metric.unit) : errorsOf(r, w.name).length ? "failed" : "–"}</td>`;
    }
    html += "</tr>";
  }
  document.getElementById("records-table").innerHTML = `${html}</tbody>`;
}

function renderMethod() {
  document.getElementById("options").textContent =
    `rewind run ${state.history.options.join(" ")} ...`;
  document.getElementById("workloads").innerHTML = workloads()
    .map(
      (w) => `<dt>${escapeHtml(w.name)}</dt><dd>${prose(w.description)}</dd>`,
    )
    .join("");
}

function renderCharts() {
  renderGroups();
  renderDerived();
}

async function main() {
  const response = await fetch(HISTORY_URL);
  state.history = await response.json();
  const recs = records();
  state.head = recs[recs.length - 1].label;
  state.base = recs[Math.max(0, recs.length - 2)].label;

  renderMachine();
  renderSelects();
  renderScorecard();
  renderMetricTabs();
  renderCharts();
  renderMilestones();
  renderRecords();
  renderMethod();

  // Charts are drawn to their container's width, so a resize redraws.
  let timer = null;
  let lastWidth = window.innerWidth;
  window.addEventListener("resize", () => {
    if (window.innerWidth === lastWidth) {
      return;
    }
    lastWidth = window.innerWidth;
    clearTimeout(timer);
    timer = setTimeout(renderCharts, RESIZE_DELAY_MS);
  });
}

main();
