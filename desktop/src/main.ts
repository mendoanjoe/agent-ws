// The agent-ws window: repositories on the left, their workspaces in a table,
// the selected one's diff and log below. It polls the library through the
// Rust side and refreshes at once when the menu-bar monitor sees a change.
import { listen } from "@tauri-apps/api/event";
import { api, type Diff, type Repo, type Status, type Workspace } from "./api";

const $ = <T extends HTMLElement>(sel: string) => document.querySelector(sel) as T;

const state = {
  repos: [] as Repo[],
  root: localStorage.getItem("root") ?? "",
  rows: [] as Workspace[],
  disk: new Map<string, number>(),
  status: null as Status | null,
  selected: "",
  tab: "diff" as "diff" | "log",
  diffShown: "",
};

// ---- formatting -------------------------------------------------------------

function age(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

function bytes(b: number): string {
  if (b >= 1e9) return `${(b / 1e9).toFixed(1)} GB`;
  if (b >= 1e6) return `${(b / 1e6).toFixed(1)} MB`;
  return `${Math.round(b / 1e3)} KB`;
}

function el(tag: string, cls = "", text = ""): HTMLElement {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

function repoName(root: string): string {
  return root.split("/").filter(Boolean).pop() ?? root;
}

let toastTimer = 0;
function toast(message: string, kind: "ok" | "error" = "ok") {
  const t = $("#toast");
  t.textContent = message;
  t.className = kind;
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (t.hidden = true), kind === "error" ? 8000 : 4000);
}

function failed(e: unknown) {
  toast(String(e), "error");
}

// ---- repositories -----------------------------------------------------------

async function loadRepos() {
  try {
    state.repos = await api.repos();
  } catch (e) {
    return failed(e);
  }
  if (!state.repos.some((r) => r.root === state.root)) {
    state.root = state.repos[0]?.root ?? "";
  }
  renderRepos();
}

function renderRepos() {
  const nav = $("#repos");
  nav.replaceChildren(
    ...state.repos.map((r) => {
      const b = el("button", r.root === state.root ? "repo active" : "repo");
      b.append(el("span", "name", repoName(r.root)), el("span", "count", String(r.workspaces)));
      b.title = r.root;
      b.onclick = () => selectRepo(r.root);
      return b;
    }),
  );
  $("#sidebar-note").textContent = state.repos.length
    ? ""
    : "No repositories yet. Run `agent-ws spawn NAME` in one.";
}

function selectRepo(root: string) {
  state.root = root;
  localStorage.setItem("root", root);
  state.rows = [];
  state.disk.clear();
  state.status = null;
  state.selected = "";
  state.diffShown = "";
  renderRepos();
  renderAll();
  void refresh();
  void loadDisk();
}

// ---- workspaces -------------------------------------------------------------

let refreshing = false;
async function refresh() {
  if (!state.root || refreshing) return;
  refreshing = true;
  const root = state.root;
  try {
    const [rows, status] = await Promise.all([api.workspaces(root, false), api.status(root)]);
    if (root !== state.root) return;
    state.rows = rows;
    state.status = status;
    if (!rows.some((w) => w.name === state.selected)) {
      state.selected = rows[0]?.name ?? "";
      state.diffShown = "";
    }
    renderAll();
    if (state.selected && state.tab === "diff" && state.diffShown !== key()) void loadDiff();
    if (state.selected && state.tab === "log") void loadLog();
  } catch (e) {
    failed(e);
  } finally {
    refreshing = false;
  }
}

async function loadDisk() {
  if (!state.root) return;
  const root = state.root;
  try {
    const rows = await api.workspaces(root, true);
    if (root !== state.root) return;
    state.disk = new Map(rows.map((w) => [w.name, w.private_bytes ?? 0]));
    renderTable();
  } catch {
    // The table still works without disk use.
  }
}

/// What the shown diff depends on: the workspace, its commits, its changed
/// paths and its last run. Edits inside an already changed file need Refresh.
function key(): string {
  const w = current();
  if (!w) return "";
  const changed = state.status?.workspaces.find((s) => s.name === w.name)?.changed ?? [];
  return `${w.name}|${w.dirty}|${w.ahead}|${w.run?.ended ?? ""}|${changed.join(",")}`;
}

function current(): Workspace | undefined {
  return state.rows.find((w) => w.name === state.selected);
}

function renderAll() {
  $("#repo-name").textContent = state.root ? repoName(state.root) : "agent-ws";
  $("#repo-path").textContent = state.root;
  renderBanner();
  renderTable();
  renderDetail();
}

function renderBanner() {
  const banner = $("#banner");
  const s = state.status;
  const lines: HTMLElement[] = [];
  for (const c of s?.conflicts ?? []) {
    lines.push(el("p", "conflict", `CONFLICT ${c.workspaces.join(" + ")}: ${c.paths.join(", ")}`));
  }
  const conflicted = new Set((s?.conflicts ?? []).flatMap((c) => c.paths));
  for (const [path, names] of Object.entries(s?.overlaps ?? {})) {
    if (!conflicted.has(path)) lines.push(el("p", "overlap", `OVERLAP ${path}: ${names.join(", ")} (merges cleanly)`));
  }
  if (s && s.conflicts === null && Object.keys(s.overlaps).length) {
    lines.push(el("p", "muted", "Conflict check needs Git 2.38 or later."));
  }
  banner.replaceChildren(...lines);
  banner.hidden = lines.length === 0;
}

function runCell(w: Workspace): HTMLElement {
  const r = w.run;
  if (!r) return el("td", "muted", "–");
  const cell = el("td");
  const label =
    r.state === "running" ? "● running" : r.state === "interrupted" ? "⚠ interrupted" : r.exit_code === 0 ? "✓ exit 0" : `✗ exit ${r.exit_code}`;
  cell.append(el("span", `run ${r.state === "exited" ? (r.exit_code === 0 ? "ok" : "bad") : r.state}`, label));
  cell.title = r.command.join(" ");
  return cell;
}

function changesText(w: Workspace): string {
  if (w.dirty === null) return "?";
  const parts = [];
  if (w.dirty) parts.push("dirty");
  if (w.ahead) parts.push(`+${w.ahead}`);
  return parts.length ? parts.join(" ") : "clean";
}

function renderTable() {
  const tbody = $("#workspaces tbody");
  tbody.replaceChildren(
    ...state.rows.map((w) => {
      const tr = el("tr", w.name === state.selected ? "selected" : "");
      const name = el("td", "name");
      name.append(el("span", "", w.name));
      if (w.origin === "claude") name.append(el("span", "badge", "claude"));
      tr.append(
        name,
        el("td", "num", age(w.age_secs)),
        el("td", "num", w.port ? String(w.port) : "–"),
        runCell(w),
        el("td", w.dirty ? "dirty" : "", changesText(w)),
        el("td", "num", String(w.processes.length)),
        el("td", "num", state.disk.has(w.name) ? bytes(state.disk.get(w.name) ?? 0) : "…"),
      );
      tr.onclick = () => {
        state.selected = w.name;
        state.diffShown = "";
        renderTable();
        renderDetail();
        void (state.tab === "diff" ? loadDiff() : loadLog());
      };
      return tr;
    }),
  );
  const empty = $("#empty");
  empty.hidden = state.rows.length > 0 || !state.root;
  empty.textContent = "No workspaces. Create one with `agent-ws spawn NAME`, or let Claude Code make them (`agent-ws claude install`).";
}

function renderDetail() {
  const w = current();
  const detail = $("#detail");
  detail.hidden = !w;
  if (!w) return;
  $("#ws-name").textContent = w.name;
  const meta = [w.branch ? `branch ${w.branch}` : "detached", `base ${w.base.slice(0, 12)}`, w.path];
  $("#ws-meta").textContent = meta.join(" · ");
  const port = $<HTMLButtonElement>("#open-port");
  port.hidden = !w.port;
  port.textContent = w.port ? `localhost:${w.port}` : "";
}

// ---- diff and log -----------------------------------------------------------

async function loadDiff() {
  const w = current();
  if (!w) return;
  const shown = key();
  const box = $("#tab-diff");
  if (state.diffShown === "") box.replaceChildren(el("p", "muted", "Reading the workspace…"));
  try {
    const diff = await api.diff(state.root, w.name);
    if (current()?.name !== w.name) return;
    state.diffShown = shown;
    renderDiff(diff);
  } catch (e) {
    box.replaceChildren(el("p", "error", String(e)));
  }
}

const MAX_LINES = 4000;

function renderDiff(diff: Diff) {
  const box = $("#tab-diff");
  if (!diff.files.length) {
    box.replaceChildren(el("p", "muted", "No changes since the base."));
    return;
  }
  const files = el("ul", "files");
  for (const f of diff.files) {
    const li = el("li");
    li.append(
      el("span", `st st-${f.status}`, f.status),
      el("span", "path", f.path),
      el("span", "add", f.added === null ? "bin" : `+${f.added}`),
      el("span", "del", f.deleted === null ? "" : `−${f.deleted}`),
    );
    files.append(li);
  }
  const patch = el("pre", "patch");
  const lines = diff.patch.split("\n");
  for (const line of lines.slice(0, MAX_LINES)) {
    const cls = /^(diff --git|index |--- |\+\+\+ |new file|deleted file|similarity|rename )/.test(line)
      ? "meta"
      : line.startsWith("@@")
        ? "hunk"
        : line.startsWith("+")
          ? "add"
          : line.startsWith("-")
            ? "del"
            : "";
    patch.append(el("span", cls, line + "\n"));
  }
  if (lines.length > MAX_LINES) patch.append(el("span", "meta", `… ${lines.length - MAX_LINES} more lines; open the workspace to see them all\n`));
  box.replaceChildren(files, patch);
}

async function loadLog() {
  const w = current();
  if (!w) return;
  const pre = $("#log");
  try {
    const text = await api.runLog(state.root, w.name);
    if (current()?.name !== w.name) return;
    pre.textContent = text ?? "No log. Commands started with `agent-ws exec NAME --log -- …` (or without a terminal) keep one.";
    pre.scrollTop = pre.scrollHeight;
  } catch (e) {
    pre.textContent = String(e);
  }
}

function showTab(tab: "diff" | "log") {
  state.tab = tab;
  document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.tab === tab)));
  $("#tab-diff").hidden = tab !== "diff";
  $("#tab-log").hidden = tab !== "log";
  void (tab === "diff" ? loadDiff() : loadLog());
}

// ---- dialogs and actions ----------------------------------------------------

/// Show the dialog with `body`; resolves to whether OK was pressed.
function ask(title: string, body: HTMLElement[], ok: string, danger = false): Promise<boolean> {
  const dialog = $<HTMLDialogElement>("#dialog");
  $("#dialog-title").textContent = title;
  $("#dialog-body").replaceChildren(...body);
  const okButton = $<HTMLButtonElement>("#dialog-ok");
  okButton.textContent = ok;
  okButton.className = danger ? "danger" : "primary";
  dialog.returnValue = "";
  dialog.showModal();
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue === "ok"), { once: true });
  });
}

function checkbox(label: string, checked = false): [HTMLElement, HTMLInputElement] {
  const wrap = el("label", "check");
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;
  wrap.append(input, document.createTextNode(` ${label}`));
  return [wrap, input];
}

async function landSelected() {
  const w = current();
  if (!w) return;
  const message = document.createElement("textarea");
  message.required = true;
  message.rows = 4;
  message.placeholder = "feat: what the agent did";
  const [squashRow, squash] = checkbox("Squash into one commit");
  const [prRow, pr] = checkbox("Push and open a pull request (GitHub CLI)");
  const branch = w.branch || `agent-ws/${w.name}`;
  const note = el("p", "muted", `Commits what is left, puts it on branch ${branch} and removes the workspace.`);
  setTimeout(() => message.focus(), 0);
  if (!(await ask(`Land ${w.name}`, [message, squashRow, prRow, note], "Land"))) return;
  if (!message.value.trim()) return toast("A commit message is needed to land.", "error");
  try {
    const l = await api.land(state.root, w.name, message.value.trim(), squash.checked, pr.checked);
    toast(l.pr_url ? `Landed on ${l.branch}: ${l.pr_url}` : `Landed ${l.files} files on ${l.branch}`);
    if (l.pr_url) void api.open(l.pr_url, "url");
  } catch (e) {
    failed(e);
  }
  void refresh();
  void loadRepos();
}

async function dropSelected() {
  const w = current();
  if (!w) return;
  const warnings = [];
  if (w.dirty) warnings.push("it has uncommitted changes");
  if (w.ahead) warnings.push(`it has ${w.ahead} commit(s) on no branch yet`);
  if (w.run?.state === "running") warnings.push("its last exec is still running");
  const body = [el("p", "", `Remove ${w.name} without landing it?`)];
  if (warnings.length) body.push(el("p", "error", `Careful: ${warnings.join(", ")}. That work is lost.`));
  if (!(await ask(`Drop ${w.name}`, body, "Drop", true))) return;
  try {
    await api.drop(state.root, w.name);
    toast(`Dropped ${w.name}`);
  } catch (e) {
    failed(e);
  }
  void refresh();
  void loadRepos();
}

async function pruneRepo() {
  if (!state.root) return;
  try {
    const dry = await api.prune(state.root, true);
    if (!dry.pruned.length) return toast(`Nothing merged into ${dry.base} to drop.`);
    const list = el("ul", "plain");
    for (const p of dry.pruned) {
      list.append(el("li", "", `${p.name} — ${p.reason}, frees ${bytes(p.private_bytes)}${p.branch ? `, and branch ${p.branch}` : ""}`));
    }
    const body = [el("p", "", `Their work is already in ${dry.base}:`), list];
    if (!(await ask("Prune merged workspaces", body, `Drop ${dry.pruned.length}`, true))) return;
    const done = await api.prune(state.root, false);
    toast(`Dropped ${done.pruned.length}, freed at least ${bytes(done.freed_bytes)}`);
  } catch (e) {
    failed(e);
  }
  void refresh();
  void loadRepos();
}

function openSelected(how: "files" | "editor" | "terminal") {
  const w = current();
  if (w) api.open(w.path, how).catch(failed);
}

// ---- start ------------------------------------------------------------------

function wire() {
  $("#refresh").onclick = () => {
    state.diffShown = "";
    void refresh();
    void loadDisk();
  };
  $("#prune").onclick = () => void pruneRepo();
  $("#land").onclick = () => void landSelected();
  $("#drop").onclick = () => void dropSelected();
  $("#open-port").onclick = () => {
    const w = current();
    if (w?.port) api.open(`http://localhost:${w.port}`, "url").catch(failed);
  };
  document.querySelectorAll<HTMLButtonElement>("[data-open]").forEach((b) => {
    b.onclick = () => openSelected(b.dataset.open as "files" | "editor" | "terminal");
  });
  document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((b) => {
    b.onclick = () => showTab(b.dataset.tab as "diff" | "log");
  });
}

async function start() {
  wire();
  await loadRepos();
  renderAll();
  await refresh();
  void loadDisk();
  setInterval(() => !document.hidden && void refresh(), 4000);
  setInterval(() => !document.hidden && void loadDisk(), 30000);
  setInterval(() => !document.hidden && void loadRepos(), 15000);
  await listen("workspaces-changed", () => {
    void refresh();
    void loadRepos();
  });
}

void start();
