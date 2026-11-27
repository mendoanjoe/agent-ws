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

function saveRoot(root: string) {
  if (root) localStorage.setItem("root", root);
  else localStorage.removeItem("root");
}

function reconcileChildren(parent: Element, next: Element[]) {
  const keep = new Set(next);
  for (const child of Array.from(parent.children)) {
    if (!keep.has(child)) child.remove();
  }
  for (let i = 0; i < next.length; i++) {
    const node = next[i];
    if (!node) continue;
    const currentChild = parent.children[i] ?? null;
    if (currentChild !== node) {
      parent.insertBefore(node, currentChild);
    }
  }
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
    saveRoot(state.root);
  }
  renderRepos();
}

function renderRepos() {
  const nav = $("#repos");
  const existing = new Map<string, HTMLButtonElement>();
  for (const child of Array.from(nav.children)) {
    if (child instanceof HTMLButtonElement && child.dataset.root) {
      existing.set(child.dataset.root, child);
    }
  }
  const buttons = state.repos.map((r) => {
    let b = existing.get(r.root);
    if (!b) {
      b = el("button", "repo") as HTMLButtonElement;
      b.type = "button";
      b.dataset.root = r.root;
      b.append(el("span", "name"), el("span", "count"));
      b.onclick = () => selectRepo(r.root);
    }
    const active = r.root === state.root;
    b.className = active ? "repo active" : "repo";
    b.setAttribute("aria-current", active ? "true" : "false");
    b.title = r.root;
    const nameSpan = b.children[0];
    const countSpan = b.children[1];
    if (nameSpan) nameSpan.textContent = repoName(r.root);
    if (countSpan) countSpan.textContent = String(r.workspaces);
    return b;
  });
  reconcileChildren(nav, buttons);
  $("#sidebar-note").textContent = state.repos.length
    ? ""
    : "No repositories yet. Run `agent-ws spawn NAME` in one.";
}

function selectRepo(root: string) {
  state.root = root;
  saveRoot(root);
  refreshSeq++;
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
let pendingRefresh = false;
let refreshSeq = 0;

async function refresh() {
  if (!state.root) return;
  if (refreshing) {
    pendingRefresh = true;
    return;
  }
  refreshing = true;
  const seq = ++refreshSeq;
  const root = state.root;
  try {
    const [rows, status] = await Promise.all([api.workspaces(root, false), api.status(root)]);
    if (root !== state.root || seq !== refreshSeq) return;
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
    if (root === state.root && seq === refreshSeq) failed(e);
  } finally {
    refreshing = false;
    if (pendingRefresh) {
      pendingRefresh = false;
      void refresh();
    }
  }
}

async function loadDisk() {
  if (!state.root) return;
  const root = state.root;
  try {
    const entries = await api.diskUsage(root);
    if (root !== state.root) return;
    state.disk = new Map(entries.filter((e) => e.kind === "workspace").map((e) => [e.name, e.private]));
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

function selectWorkspace(name: string) {
  state.selected = name;
  state.diffShown = "";
  renderTable();
  renderDetail();
  void (state.tab === "diff" ? loadDiff() : loadLog());
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
  const existing = new Map<string, HTMLTableRowElement>();
  for (const child of Array.from(tbody.children)) {
    if (child instanceof HTMLTableRowElement && child.dataset.ws) {
      existing.set(child.dataset.ws, child);
    }
  }
  const rows = state.rows.map((w) => {
    let tr = existing.get(w.name);
    if (!tr) {
      tr = el("tr") as HTMLTableRowElement;
      tr.dataset.ws = w.name;
      tr.tabIndex = 0;
      tr.onclick = () => selectWorkspace(w.name);
      tr.onkeydown = (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          selectWorkspace(w.name);
        } else if (e.key === "ArrowDown") {
          e.preventDefault();
          (tr?.nextElementSibling as HTMLElement | null)?.focus();
        } else if (e.key === "ArrowUp") {
          e.preventDefault();
          (tr?.previousElementSibling as HTMLElement | null)?.focus();
        }
      };
    }
    const isSelected = w.name === state.selected;
    tr.className = isSelected ? "selected" : "";
    tr.setAttribute("aria-selected", String(isSelected));
    const name = el("td", "name");
    name.append(el("span", "", w.name));
    if (w.origin === "claude") name.append(el("span", "badge", "claude"));
    tr.replaceChildren(
      name,
      el("td", "num", age(w.age_secs)),
      el("td", "num", w.port ? String(w.port) : "–"),
      runCell(w),
      el("td", w.dirty ? "dirty" : "", changesText(w)),
      el("td", "num", String(w.processes.length)),
      el("td", "num", state.disk.has(w.name) ? bytes(state.disk.get(w.name) ?? 0) : "…"),
    );
    return tr;
  });
  reconcileChildren(tbody, rows);
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

let diffSeq = 0;
let logSeq = 0;

async function loadDiff() {
  const w = current();
  if (!w) return;
  const root = state.root;
  const name = w.name;
  const seq = ++diffSeq;
  const shown = key();
  const box = $("#tab-diff");
  if (state.diffShown === "") box.replaceChildren(el("p", "muted", "Reading the workspace…"));
  try {
    const diff = await api.diff(root, name);
    if (root !== state.root || current()?.name !== name || seq !== diffSeq) return;
    state.diffShown = shown;
    renderDiff(diff);
  } catch (e) {
    if (root !== state.root || current()?.name !== name || seq !== diffSeq) return;
    state.diffShown = `error:${shown}`;
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
  const rawLines = diff.patch.endsWith("\n") ? diff.patch.slice(0, -1).split("\n") : diff.patch.split("\n");
  const frag = document.createDocumentFragment();
  for (const line of rawLines.slice(0, MAX_LINES)) {
    const cls = /^(diff --git|index |--- |\+\+\+ |new file|deleted file|similarity|rename )/.test(line)
      ? "meta"
      : line.startsWith("@@")
        ? "hunk"
        : line.startsWith("+")
          ? "add"
          : line.startsWith("-")
            ? "del"
            : "";
    frag.append(el("span", cls, line + "\n"));
  }
  if (diff.truncated || rawLines.length > MAX_LINES) {
    frag.append(el("span", "meta", "… diff truncated; open the workspace to see all lines\n"));
  }
  patch.append(frag);
  box.replaceChildren(files, patch);
}

async function loadLog() {
  const w = current();
  if (!w) return;
  const root = state.root;
  const name = w.name;
  const seq = ++logSeq;
  const tabLog = $("#tab-log");
  const pre = $("#log");
  const nearBottom = tabLog.scrollHeight - tabLog.clientHeight - tabLog.scrollTop <= 24;
  try {
    const text = await api.runLog(root, name);
    if (root !== state.root || current()?.name !== name || seq !== logSeq) return;
    const next = text ?? "No log. Commands started with `agent-ws exec NAME --log -- …` (or without a terminal) keep one.";
    if (pre.textContent !== next) {
      pre.textContent = next;
      if (nearBottom) tabLog.scrollTop = tabLog.scrollHeight;
    }
  } catch (e) {
    if (root !== state.root || current()?.name !== name || seq !== logSeq) return;
    pre.textContent = String(e);
  }
}

function showTab(tab: "diff" | "log") {
  state.tab = tab;
  document.querySelectorAll<HTMLButtonElement>("[data-tab]").forEach((b) => {
    const active = b.dataset.tab === tab;
    b.setAttribute("aria-selected", String(active));
    b.tabIndex = active ? 0 : -1;
  });
  $("#tab-diff").hidden = tab !== "diff";
  $("#tab-log").hidden = tab !== "log";
  void (tab === "diff" ? loadDiff() : loadLog());
}

// ---- dialogs and actions ----------------------------------------------------

/// Show the dialog with `body`; resolves to whether OK was pressed.
function ask(
  title: string,
  body: HTMLElement[],
  ok: string,
  danger = false,
  validate?: () => string | null,
): Promise<boolean> {
  const dialog = $<HTMLDialogElement>("#dialog");
  if (dialog.open) return Promise.resolve(false);
  const form = $<HTMLFormElement>("#dialog-form");
  $("#dialog-title").textContent = title;
  $("#dialog-body").replaceChildren(...body);
  const okButton = $<HTMLButtonElement>("#dialog-ok");
  okButton.textContent = ok;
  okButton.className = danger ? "danger" : "primary";
  dialog.returnValue = "";
  const onSubmit = (e: SubmitEvent) => {
    const submitter = e.submitter as HTMLButtonElement | null;
    if (submitter?.value === "ok" && validate) {
      const err = validate();
      if (err) {
        e.preventDefault();
        toast(err, "error");
      }
    }
  };
  form.addEventListener("submit", onSubmit);
  dialog.showModal();
  return new Promise((resolve) => {
    dialog.addEventListener(
      "close",
      () => {
        form.removeEventListener("submit", onSubmit);
        resolve(dialog.returnValue === "ok");
      },
      { once: true },
    );
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

let actionBusy = false;
async function withActionLock(work: () => Promise<void>) {
  if (actionBusy) return;
  actionBusy = true;
  const buttons = ["#prune", "#land", "#drop"].map((s) => $<HTMLButtonElement>(s));
  for (const b of buttons) b.disabled = true;
  try {
    await work();
  } finally {
    for (const b of buttons) b.disabled = false;
    actionBusy = false;
  }
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
  const ok = await ask(
    `Land ${w.name}`,
    [message, squashRow, prRow, note],
    "Land",
    false,
    () => {
      if (message.value.trim()) return null;
      message.focus();
      return "A commit message is needed to land.";
    },
  );
  if (!ok) return;
  try {
    const l = await api.land(state.root, w.name, message.value.trim(), squash.checked, pr.checked);
    toast(l.pr_url ? `Landed on ${l.branch}: ${l.pr_url}` : `Landed ${l.files} files on ${l.branch}`);
    if (l.pr_url) api.open(l.pr_url, "url").catch(failed);
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
  $("#prune").onclick = () => void withActionLock(pruneRepo);
  $("#land").onclick = () => void withActionLock(landSelected);
  $("#drop").onclick = () => void withActionLock(dropSelected);
  $("#open-port").onclick = () => {
    const w = current();
    if (w?.port) api.open(`http://localhost:${w.port}`, "url").catch(failed);
  };
  document.querySelectorAll<HTMLButtonElement>("[data-open]").forEach((b) => {
    b.onclick = () => openSelected(b.dataset.open as "files" | "editor" | "terminal");
  });
  const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>("[data-tab]"));
  tabs.forEach((b, idx) => {
    b.tabIndex = b.dataset.tab === state.tab ? 0 : -1;
    b.onclick = () => showTab(b.dataset.tab as "diff" | "log");
    b.onkeydown = (e) => {
      if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
      e.preventDefault();
      const nextIdx = (idx + (e.key === "ArrowRight" ? 1 : -1) + tabs.length) % tabs.length;
      const target = tabs[nextIdx];
      if (!target) return;
      showTab(target.dataset.tab as "diff" | "log");
      target.focus();
    };
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
  let changeTimer = 0;
  await listen("workspaces-changed", () => {
    clearTimeout(changeTimer);
    changeTimer = window.setTimeout(() => {
      void refresh();
      void loadRepos();
    }, 150);
  });
}

start().catch(failed);
