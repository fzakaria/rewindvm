// The hero's scrubber: the desktop app's timeline, redrawn in HTML and run
// on one example. A build of mylib-0.3.0 fails in test_pool_shutdown on
// run #3 and passes on run #2; the two runs first differ at a futex wakeup.
// Every step number, pid and path below is example data, not a real run.
(() => {
  "use strict";

  // The run: how many steps it has, and the two steps the app marks.
  const TOTAL_STEPS = 11760;
  // Where the two runs' schedules part: only the failing run is
  // rescheduled there, and everything after follows from it.
  const SPLIT_STEP = 11030;
  const DIVERGENCE_STEP = 11204;
  const FAILURE_STEP = 11742;
  const START_STEP = FAILURE_STEP;

  // Example virtual time per step, only to give the readout a clock.
  const MS_PER_STEP = 16.41;

  // How many lines each list shows. The log's panel is a fixed height and
  // clips its oldest lines, so this is enough to fill it.
  const LOG_LINES = 18;
  const FILE_LINES = 5;

  // A file written this many steps ago or fewer is drawn as fresh.
  const RECENT_STEPS = 400;

  // The phase a segment needs to be at least this share of the run to
  // carry its label.
  const LABEL_MIN_SHARE = 0.05;

  // The opening sweep from the start of checkPhase to the failure.
  const SWEEP_FROM = 8620;
  const SWEEP_MS = 2600;

  // The run that a fork creates.
  const FORK_RUN = 4;

  const Kind = Object.freeze({
    NORMAL: "normal",
    PHASE: "phase",
    ERROR: "error",
    DIVERGENCE: "divergence",
  });

  const PHASES = [
    { label: "unpack", start: 0, end: 420, color: "#2c323d" },
    { label: "patch", start: 420, end: 610, color: "#323946" },
    { label: "configure", start: 610, end: 2140, color: "#39414f" },
    { label: "build", start: 2140, end: 8620, color: "#414a5a" },
    { label: "check", start: 8620, end: TOTAL_STEPS, color: "#4a5466" },
  ];

  const LOG = [
    [0, "building '/nix/store/9x2k…-mylib-0.3.0.drv'", Kind.NORMAL],
    [5, "Running phase: unpackPhase", Kind.PHASE],
    [
      40,
      "unpacking source archive /nix/store/…-mylib-0.3.0.tar.gz",
      Kind.NORMAL,
    ],
    [400, "source root is mylib-0.3.0", Kind.NORMAL],
    [420, "Running phase: patchPhase", Kind.PHASE],
    [600, "patching script interpreter paths in ./scripts", Kind.NORMAL],
    [610, "Running phase: configurePhase", Kind.PHASE],
    [700, "-- The C compiler identification is GNU 13.3.0", Kind.NORMAL],
    [1300, "-- Looking for pthread_create in pthreads - found", Kind.NORMAL],
    [
      2100,
      "-- Build files have been written to: /build/mylib-0.3.0/build",
      Kind.NORMAL,
    ],
    [2140, "Running phase: buildPhase", Kind.PHASE],
    [
      2300,
      "[  4%] Building C object src/CMakeFiles/mylib.dir/pool.c.o",
      Kind.NORMAL,
    ],
    [
      3900,
      "[ 31%] Building C object src/CMakeFiles/mylib.dir/queue.c.o",
      Kind.NORMAL,
    ],
    [6100, "[ 72%] Linking C shared library libmylib.so", Kind.NORMAL],
    [8500, "[100%] Built target test_pool", Kind.NORMAL],
    [8620, "Running phase: checkPhase", Kind.PHASE],
    [8700, "Test project /build/mylib-0.3.0/build", Kind.NORMAL],
    [9400, "1/4 Test #1: test_queue .......... Passed 0.21 sec", Kind.NORMAL],
    [10300, "2/4 Test #2: test_alloc .......... Passed 0.40 sec", Kind.NORMAL],
    [11100, "3/4 Test #3: test_pool_basic ..... Passed 0.33 sec", Kind.NORMAL],
    [
      11742,
      "4/4 Test #4: test_pool_shutdown .. ***Exception: SegFault",
      Kind.ERROR,
    ],
    [
      11750,
      "error: builder for '/nix/store/9x2k…-mylib-0.3.0.drv' failed with exit code 8",
      Kind.ERROR,
    ],
  ].map(([step, text, kind]) => ({ step, text, kind }));

  // Processes from their first step to the step they exit. The first two
  // live for the whole run.
  const PROCS = [
    {
      pid: 1,
      cmd: "init (rewind)",
      depth: 0,
      start: 0,
      end: TOTAL_STEPS + 1,
    },
    {
      pid: 2,
      cmd: "bash -e default-builder.sh",
      depth: 1,
      start: 0,
      end: TOTAL_STEPS + 1,
    },
    {
      pid: 14,
      cmd: "tar xf mylib-0.3.0.tar.gz",
      depth: 2,
      start: 40,
      end: 400,
    },
    { pid: 31, cmd: "cmake ..", depth: 2, start: 620, end: 2130 },
    { pid: 88, cmd: "make -j4", depth: 2, start: 2150, end: 8610 },
    { pid: 91, cmd: "cc -c pool.c", depth: 3, start: 2200, end: 3100 },
    { pid: 97, cmd: "cc -c queue.c", depth: 3, start: 3800, end: 4700 },
    {
      pid: 120,
      cmd: "ld -shared -o libmylib.so",
      depth: 3,
      start: 6050,
      end: 6400,
    },
    {
      pid: 140,
      cmd: "ctest --output-on-failure",
      depth: 2,
      start: 8650,
      end: 11755,
    },
    { pid: 161, cmd: "test_pool_shutdown", depth: 3, start: 11120, end: 11745 },
    {
      pid: 162,
      cmd: "thread worker-0",
      depth: 4,
      start: 11130,
      end: 11745,
      thread: true,
    },
    {
      pid: 163,
      cmd: "thread worker-1",
      depth: 4,
      start: 11131,
      end: 11745,
      thread: true,
    },
  ];

  const FILES = [
    { path: "/build/mylib-0.3.0/CMakeLists.txt", step: 380 },
    { path: "/build/mylib-0.3.0/build/Makefile", step: 2090 },
    { path: "/build/…/build/src/pool.c.o", step: 3080 },
    { path: "/build/…/build/src/queue.c.o", step: 4680 },
    { path: "/build/…/build/libmylib.so", step: 6390 },
    { path: "/build/…/build/test_pool", step: 8480 },
    { path: "/build/…/Testing/Temporary/LastTest.log", step: 8720 },
    { path: "/build/…/build/core.161", step: 11744, bad: true },
  ];

  // Indentation of one level in the process tree, in pixels.
  const INDENT_PX = 14;
  const ROW_PAD_PX = 16;

  // The process that wrote a log line or a file at a step.
  function pidAt(step) {
    if (step >= 8650) {
      return 140;
    }
    if (step >= 2150 && step < 8610) {
      return 88;
    }
    if (step >= 620 && step < 2130) {
      return 31;
    }
    return 2;
  }

  // Every event on the timeline, as the app would list the VM's syscalls:
  // writes for log lines, execve, clone and exit for processes, openat for
  // files, and the two steps the story hangs on.
  function buildEvents() {
    const events = [];

    for (const line of LOG) {
      const kind = line.kind === Kind.ERROR ? Kind.ERROR : Kind.NORMAL;
      events.push({
        step: line.step,
        pid: pidAt(line.step),
        text: `write(1, "${line.text}")`,
        kind,
      });
    }

    for (const p of PROCS) {
      if (p.start > 0) {
        const text = p.thread
          ? `clone(CLONE_THREAD): ${p.cmd}`
          : `execve("${p.cmd}")`;
        events.push({ step: p.start, pid: p.pid, text, kind: Kind.NORMAL });
      }
      if (p.end <= TOTAL_STEPS) {
        events.push({
          step: p.end,
          pid: p.pid,
          text: "exit_group()",
          kind: Kind.NORMAL,
        });
      }
    }

    for (const f of FILES) {
      const kind = f.bad ? Kind.ERROR : Kind.NORMAL;
      events.push({
        step: f.step,
        pid: pidAt(f.step),
        text: `openat(O_CREAT) ${f.path}`,
        kind,
      });
    }

    events.push({
      step: DIVERGENCE_STEP,
      pid: 163,
      text: "futex(FUTEX_WAKE): worker-1 woken before worker-0",
      kind: Kind.DIVERGENCE,
    });
    events.push({
      step: FAILURE_STEP,
      pid: 162,
      text: "SIGSEGV in pool_shutdown() at queue.c:88 in worker-0",
      kind: Kind.ERROR,
    });

    // Stable sort: two events on one step keep the order they were added in.
    events.sort((a, b) => a.step - b.step);
    return events;
  }

  const EVENTS = buildEvents();

  const fmt = (n) => n.toLocaleString("en-US");
  const pct = (n) => `${((n / TOTAL_STEPS) * 100).toFixed(3)}%`;
  const clamp = (n) => Math.max(0, Math.min(TOTAL_STEPS, Math.round(n)));

  // What the demo shows: the playhead and, once forked, where from.
  const state = { step: START_STEP, forkStep: null };

  const root = document.getElementById("demo");
  if (!root) {
    return;
  }
  const $ = (id) => document.getElementById(id);
  const el = {
    track: $("track"),
    segments: $("segments"),
    playhead: $("playhead"),
    split: $("mark-split"),
    divergence: $("mark-divergence"),
    failure: $("mark-failure"),
    fork: $("mark-fork"),
    scrub: $("scrub"),
    stepLabel: $("step-label"),
    clock: $("clock"),
    phase: $("phase"),
    log: $("log"),
    procs: $("procs"),
    files: $("files"),
    evMeta: $("ev-meta"),
    evText: $("ev-text"),
    diff: $("diff"),
    note: $("note"),
    forkBtn: $("fork"),
  };

  // One list row: a number column and a text column. Text only, never HTML.
  function row(num, text, textClass, indentPx) {
    const li = document.createElement("li");
    if (indentPx !== undefined) {
      li.style.paddingLeft = `${indentPx}px`;
    }
    const n = document.createElement("span");
    n.className = "n";
    n.textContent = num;
    const t = document.createElement("span");
    t.className = textClass ? `t ${textClass}` : "t";
    t.textContent = text;
    li.append(n, t);
    return li;
  }

  // The phase segments are built once; render() only moves the highlight.
  const segmentEls = PHASES.map((p) => {
    const div = document.createElement("div");
    const share = (p.end - p.start) / TOTAL_STEPS;
    div.style.width = `calc(${(share * 100).toFixed(3)}% - 2px)`;
    div.style.background = p.color;
    div.dataset.label = share > LABEL_MIN_SHARE ? p.label : "";
    el.segments.append(div);
    return div;
  });

  // A phase label that does not fit its segment is left off rather than cut
  // short, which on a phone drops "configure" and keeps "build" and "check".
  function fitLabels() {
    for (const div of segmentEls) {
      div.textContent = div.dataset.label;
      if (div.scrollWidth > div.clientWidth) {
        div.textContent = "";
      }
    }
  }
  fitLabels();
  window.addEventListener("resize", fitLabels);
  document.fonts.ready.then(fitLabels);
  el.split.style.left = pct(SPLIT_STEP);
  el.divergence.style.left = pct(DIVERGENCE_STEP);
  el.failure.style.left = pct(FAILURE_STEP);

  function phaseAt(step) {
    const last = PHASES.length - 1;
    return PHASES.findIndex(
      (p, i) =>
        (step >= p.start && step < p.end) || (i === last && step >= p.start),
    );
  }

  function clockAt(step) {
    const ms = Math.round(step * MS_PER_STEP);
    const min = String(Math.floor(ms / 60000)).padStart(2, "0");
    const sec = String(Math.floor(ms / 1000) % 60).padStart(2, "0");
    const frac = String(ms % 1000).padStart(3, "0");
    return `${min}:${sec}.${frac}`;
  }

  // The last event at or before a step; the first event when there is none.
  function eventAt(step) {
    let found = EVENTS[0];
    for (const e of EVENTS) {
      if (e.step > step) {
        break;
      }
      found = e;
    }
    return found;
  }

  function render() {
    const s = state.step;

    // Timeline and readout.
    const phase = phaseAt(s);
    segmentEls.forEach((div, i) => div.classList.toggle("active", i === phase));
    el.playhead.style.left = pct(s);
    el.scrub.value = String(s);
    el.scrub.setAttribute(
      "aria-valuetext",
      `step ${fmt(s)} of ${fmt(TOTAL_STEPS)}, ${PHASES[phase].label} phase`,
    );
    el.stepLabel.textContent = `${fmt(s)} / ${fmt(TOTAL_STEPS)}`;
    el.clock.textContent = clockAt(s);
    el.phase.textContent = PHASES[phase].label;

    // Build log up to this step, the newest line highlighted.
    const shown = LOG.filter((l) => l.step <= s).slice(-LOG_LINES);
    el.log.replaceChildren(
      ...shown.map((l, i) => {
        const cls =
          l.kind === Kind.ERROR ? "err" : l.kind === Kind.PHASE ? "phase" : "";
        const li = row(fmt(l.step), l.text, cls);
        li.classList.toggle("now", i === shown.length - 1);
        return li;
      }),
    );

    // Processes alive at this step, indented by depth.
    const alive = PROCS.filter((p) => p.start <= s && s < p.end);
    el.procs.replaceChildren(
      ...alive.map((p) =>
        row(
          String(p.pid),
          p.cmd,
          p.thread ? "thread" : "",
          ROW_PAD_PX + p.depth * INDENT_PX,
        ),
      ),
    );

    // Files written so far, newest first.
    const written = FILES.filter((f) => f.step <= s)
      .reverse()
      .slice(0, FILE_LINES);
    el.files.replaceChildren(
      ...written.map((f) => {
        const cls = f.bad ? "err" : s - f.step < RECENT_STEPS ? "recent" : "";
        return row(fmt(f.step), f.path, cls);
      }),
    );

    // The event at this step.
    const ev = eventAt(s);
    el.evMeta.textContent = `last event · step ${fmt(ev.step)} · pid ${ev.pid}`;
    el.evText.textContent = ev.text;
    el.evText.className =
      ev.kind === Kind.ERROR
        ? "ev-text err"
        : ev.kind === Kind.DIVERGENCE
          ? "ev-text div"
          : "ev-text";

    // Run #3 against the passing run #2: identical up to the divergence.
    const title = document.createElement("strong");
    const body = document.createElement("span");
    if (s < DIVERGENCE_STEP) {
      title.textContent = "Same as run #2 so far";
      body.textContent = `The two runs are the same until step ${fmt(SPLIT_STEP)}, where only this run has a reschedule. They do the same things until step ${fmt(DIVERGENCE_STEP)}.`;
    } else {
      title.textContent = `Diverged from run #2 at step ${fmt(DIVERGENCE_STEP)}`;
      body.textContent =
        `The two runs are the same until step ${fmt(SPLIT_STEP)}, where only this run has a reschedule. ` +
        "In the passing run the shutdown futex woke worker-0 first. Here worker-1 wins and frees the queue while worker-0 still holds a pointer into it.";
    }
    el.diff.replaceChildren(title, body);

    // The fork marker, once there is a fork.
    el.fork.hidden = state.forkStep === null;
    if (state.forkStep !== null) {
      el.fork.style.left = pct(state.forkStep);
    }
  }

  // What the inspect buttons would do in the app, said in the note card,
  // with a link to the app's screenshot of it when there is one.
  function say(strong, rest, shot) {
    const b = document.createElement("strong");
    b.textContent = strong;
    el.note.replaceChildren(b, document.createTextNode(` ${rest}`));
    const tab = shot && document.getElementById(shot);
    if (!tab) {
      return;
    }
    const a = document.createElement("a");
    a.href = "#features";
    a.textContent = "See it in the app";
    a.addEventListener("click", (e) => {
      e.preventDefault();
      showTab(tab);
      tab.closest(".features").scrollIntoView({ behavior: "smooth" });
    });
    el.note.append(" ", a);
  }

  function resetNote() {
    say(
      "Inspect.",
      "Fork from the step under the playhead, or compare it with run #2.",
    );
  }

  // What each inspect button does in the app, said for the step under
  // the playhead.
  const INSPECT = {
    gdb: (s, ev) => [
      "Attach gdb.",
      `gdb on a fork stopped at step ${fmt(s)}, with the symbols of pid ${ev.pid}, the process running there.`,
      "tab-gdb",
    ],
    shell: (s) => [
      "Open shell.",
      `A shell inside the VM at step ${fmt(s)}, with the build's environment.`,
      "tab-shell",
    ],
    export: () => [
      "Export.",
      "Run #3 as one .rwd file that replays on another machine.",
    ],
  };

  // Any user input stops the opening sweep.
  let sweep = null;
  function stopSweep() {
    if (sweep !== null) {
      cancelAnimationFrame(sweep);
      sweep = null;
    }
  }

  function go(step) {
    stopSweep();
    state.step = clamp(step);
    render();
  }

  function prevEventStep(s) {
    let t = 0;
    for (const e of EVENTS) {
      if (e.step >= s) {
        break;
      }
      t = e.step;
    }
    return t;
  }

  function nextEventStep(s) {
    const next = EVENTS.find((e) => e.step > s);
    return next ? next.step : TOTAL_STEPS;
  }

  const GO = {
    start: () => 0,
    prev: prevEventStep,
    next: nextEventStep,
    divergence: () => DIVERGENCE_STEP,
    failure: () => FAILURE_STEP,
  };

  // The slider: arrow keys move one step natively; Page Up and Page Down
  // move to the previous or next event instead of the browser's own jump.
  el.scrub.addEventListener("input", () => go(Number(el.scrub.value)));
  el.scrub.addEventListener("keydown", (e) => {
    if (e.key === "PageUp") {
      e.preventDefault();
      go(nextEventStep(state.step));
      return;
    }
    if (e.key === "PageDown") {
      e.preventDefault();
      go(prevEventStep(state.step));
    }
  });

  // The track: click or drag on the phase bar to seek.
  function seekFromPointer(e) {
    const box = el.track.getBoundingClientRect();
    go(((e.clientX - box.left) / box.width) * TOTAL_STEPS);
  }
  el.track.addEventListener("pointerdown", (e) => {
    el.track.setPointerCapture(e.pointerId);
    seekFromPointer(e);
  });
  el.track.addEventListener("pointermove", (e) => {
    if (el.track.hasPointerCapture(e.pointerId)) {
      seekFromPointer(e);
    }
  });

  // Transport buttons.
  for (const button of root.querySelectorAll("[data-go]")) {
    button.addEventListener("click", () =>
      go(GO[button.dataset.go](state.step)),
    );
  }

  // Inspect buttons.
  for (const button of root.querySelectorAll("[data-inspect]")) {
    button.addEventListener("click", () => {
      stopSweep();
      const [strong, rest, shot] = INSPECT[button.dataset.inspect](
        state.step,
        eventAt(state.step),
      );
      say(strong, rest, shot);
    });
  }

  // Fork: mark the step and say what a fork is for.
  el.forkBtn.addEventListener("click", () => {
    stopSweep();
    state.forkStep = state.step;
    say(
      `Forked at step ${fmt(state.step)} as run #${FORK_RUN}.`,
      "It is run #3 up to here, then runs under a new schedule, so the threads interleave differently.",
    );
    render();
  });

  // The panels' tabs. A phone opens on the step's detail; a wider screen
  // shows the detail beside the tabs, so it opens on the log, and a tab
  // that has no panel of its own there falls back to the log.
  const panels = $("panels");
  const panelTabs = [...panels.querySelectorAll("[role=tab]")];
  const phone = window.matchMedia("(max-width: 600px)");
  function showPanel(name) {
    panels.dataset.show = name;
    for (const tab of panelTabs) {
      tab.setAttribute("aria-selected", String(tab.dataset.panel === name));
    }
  }
  for (const tab of panelTabs) {
    tab.addEventListener("click", () => showPanel(tab.dataset.panel));
  }
  showPanel(phone.matches ? "step" : "log");
  phone.addEventListener("change", () => {
    if (!phone.matches && panels.dataset.show === "step") {
      showPanel("log");
    }
  });

  // The "Try me" note goes away on the first pointer or key press anywhere in
  // the demo. The opening sweep is not the visitor's doing, so it does not count.
  const tryMe = document.getElementById("try-me");
  function dismissTryMe() {
    if (tryMe) {
      tryMe.classList.add("gone");
    }
    root.removeEventListener("pointerdown", dismissTryMe);
    root.removeEventListener("keydown", dismissTryMe);
  }
  root.addEventListener("pointerdown", dismissTryMe);
  root.addEventListener("keydown", dismissTryMe);

  resetNote();
  render();

  // The phone menu closes when one of its links is followed, or on a tap
  // anywhere else.
  const menu = document.querySelector(".nav-menu");
  if (menu) {
    menu.addEventListener("click", (e) => {
      if (e.target.closest("a")) {
        menu.open = false;
      }
    });
    document.addEventListener("click", (e) => {
      if (menu.open && !menu.contains(e.target)) {
        menu.open = false;
      }
    });
  }

  // The app's features: a tab shows its panel and hides the others. The
  // arrow keys move between tabs, as in any tab list.
  const tabs = [...document.querySelectorAll(".shots-tabs [role=tab]")];
  // The page's side gutter on a phone, which a row to swipe keeps when it
  // scrolls a tab or card into view.
  const GUTTER_PX = 20;
  function showTab(tab) {
    for (const other of tabs) {
      const selected = other === tab;
      other.setAttribute("aria-selected", String(selected));
      other.tabIndex = selected ? 0 : -1;
      document.getElementById(other.getAttribute("aria-controls")).hidden =
        !selected;
    }

    // On a phone the tabs are a row to swipe: bring the chosen one into it.
    const row = tab.parentElement;
    const r = tab.getBoundingClientRect();
    const box = row.getBoundingClientRect();
    if (r.left < box.left || r.right > box.right) {
      row.scrollBy({ left: r.left - box.left - GUTTER_PX });
    }
  }
  const TAB_KEYS = Object.freeze({
    ArrowRight: 1,
    ArrowDown: 1,
    ArrowLeft: -1,
    ArrowUp: -1,
  });
  for (const [i, tab] of tabs.entries()) {
    tab.tabIndex = i === 0 ? 0 : -1;
    tab.addEventListener("click", () => showTab(tab));
    tab.addEventListener("keydown", (e) => {
      const delta = TAB_KEYS[e.key];
      if (delta === undefined) {
        return;
      }
      e.preventDefault();
      const next = tabs[(i + delta + tabs.length) % tabs.length];
      showTab(next);
      next.focus();
    });
  }

  // A row of cards to swipe gets a dot per card under it, the card most in
  // view lit; a dot scrolls to its card. style.css shows the dots, and lays
  // the cards out as a row, only on a phone.
  for (const row of document.querySelectorAll(".swipe")) {
    const cards = [...row.children];
    const dots = document.createElement("div");
    dots.className = "swipe-dots";
    const buttons = cards.map((card, i) => {
      const dot = document.createElement("button");
      dot.type = "button";
      dot.setAttribute("aria-label", `Card ${i + 1} of ${cards.length}`);
      dot.addEventListener("click", () =>
        row.scrollTo({
          left: card.offsetLeft - row.offsetLeft - GUTTER_PX,
          behavior: "smooth",
        }),
      );
      return dot;
    });
    dots.append(...buttons);
    row.after(dots);

    const light = () => {
      const box = row.getBoundingClientRect();
      let best = 0;
      let bestOverlap = -1;
      for (const [i, card] of cards.entries()) {
        const r = card.getBoundingClientRect();
        const overlap =
          Math.min(r.right, box.right) - Math.max(r.left, box.left);
        if (overlap > bestOverlap) {
          best = i;
          bestOverlap = overlap;
        }
      }
      buttons.forEach((dot, i) => dot.classList.toggle("on", i === best));
    };
    row.addEventListener("scroll", light, { passive: true });
    light();
  }

  // The workloads' definitions fold under their terms on a phone, and a
  // term opens its own. style.css folds them only on a phone.
  for (const list of document.querySelectorAll(".rows-def")) {
    list.classList.add("folds");
    for (const term of list.querySelectorAll("dt")) {
      const toggle = () => term.parentElement.classList.toggle("open");
      term.addEventListener("click", toggle);
    }
  }

  // The feature tabs' row loses its fade at the right edge once it is
  // scrolled to the end, where there is nothing more to swipe to.
  const tabRow = document.querySelector(".shots-tabs");
  if (tabRow) {
    const markEnd = () =>
      tabRow.classList.toggle(
        "at-end",
        tabRow.scrollLeft + tabRow.clientWidth >= tabRow.scrollWidth - 1,
      );
    tabRow.addEventListener("scroll", markEnd, { passive: true });
    window.addEventListener("resize", markEnd);
    markEnd();
  }

  // The opening sweep: the playhead runs through checkPhase to the failure,
  // once. Skipped for anyone who asked for reduced motion.
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (reduced) {
    return;
  }
  state.step = SWEEP_FROM;
  render();
  const t0 = performance.now();
  const tick = (now) => {
    const k = Math.min(1, (now - t0) / SWEEP_MS);
    const eased = 1 - Math.pow(1 - k, 3);
    state.step = clamp(SWEEP_FROM + (START_STEP - SWEEP_FROM) * eased);
    render();
    sweep = k < 1 ? requestAnimationFrame(tick) : null;
  };
  sweep = requestAnimationFrame(tick);
})();
