// Minimal read-only session inspector. No framework, no build step: fetches
// JSON from the API routes served alongside this file and renders it with
// plain DOM APIs (textContent everywhere payload text is involved, so a
// message's content can never be interpreted as HTML).

const app = document.getElementById("app");
const tokenStorageKey = "mcptracer-inspector-token";
let accessToken = loadAccessToken();

function loadAccessToken() {
  const token = new URLSearchParams(window.location.hash.slice(1)).get("token");
  // Fragments are never sent to the server; also remove this one from the
  // current history entry before rendering or following any links.
  if (token !== null) {
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  try {
    if (token !== null) window.sessionStorage.setItem(tokenStorageKey, token);
    return token ?? window.sessionStorage.getItem(tokenStorageKey);
  } catch {
    // An opened launch link still works when browser storage is disabled.
    return token;
  }
}

function el(tag, attrs, children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (key === "class") node.className = value;
    else if (key === "href") node.setAttribute("href", value);
    else node.setAttribute(key, value);
  }
  for (const child of children || []) {
    node.appendChild(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return node;
}

function fmtTime(ns) {
  if (ns === null || ns === undefined) return "-";
  return new Date(ns / 1e6).toISOString().replace("T", " ").replace("Z", " UTC");
}

function fmtMs(ns) {
  if (ns === null || ns === undefined) return "-";
  return (ns / 1e6).toFixed(1) + "ms";
}

async function fetchJson(url) {
  if (!accessToken) {
    throw new Error("Open the inspector link printed by mcptracer to view recordings.");
  }
  const res = await fetch(url, {
    headers: { Authorization: `Bearer ${accessToken}` },
    credentials: "omit",
    mode: "same-origin",
    cache: "no-store",
  });
  if (res.status === 401) {
    try { window.sessionStorage.removeItem(tokenStorageKey); } catch { /* storage disabled */ }
    throw new Error("Inspector access expired. Open the current link printed by mcptracer.");
  }
  if (!res.ok) {
    throw new Error(`${url}: HTTP ${res.status}`);
  }
  return res.json();
}

function renderError(message) {
  app.replaceChildren(el("div", { class: "error-panel" }, [message]));
}

async function renderSessionList() {
  app.replaceChildren(el("div", {}, ["Loading sessions…"]));
  let sessions;
  try {
    sessions = await fetchJson("/api/sessions");
  } catch (err) {
    renderError(String(err));
    return;
  }

  if (sessions.length === 0) {
    app.replaceChildren(
      el("div", { class: "empty" }, [
        "No sessions recorded yet. Run ",
        el("code", { class: "mono" }, ["mcptracer record -- <mcp-server-command>"]),
        " first.",
      ])
    );
    return;
  }

  const rows = sessions.map((s) => {
    const row = el("tr", { class: "session-row" }, [
      el("td", { class: "mono" }, [s.id.slice(0, 8)]),
      el("td", {}, [s.client]),
      el("td", { class: "mono" }, [s.server_command]),
      el("td", {}, [s.transport]),
      el("td", {}, [String(s.total_messages)]),
      el("td", {}, [String(s.dropped_messages)]),
      el("td", {}, [s.redaction_policy]),
      el("td", {}, [fmtTime(s.started_at_ns)]),
    ]);
    row.addEventListener("click", () => {
      window.location.href = `/session/${s.id}`;
    });
    return row;
  });

  const table = el("table", {}, [
    el("thead", {}, [
      el("tr", {}, [
        "id",
        "client",
        "server",
        "transport",
        "messages",
        "dropped",
        "redaction",
        "started",
      ].map((h) => el("th", {}, [h]))),
    ]),
    el("tbody", {}, rows),
  ]);
  app.replaceChildren(table);
}

function exchangeFor(model, seq, isRequest) {
  return model.exchanges.find((e) =>
    isRequest ? e.request_seq === seq : e.response_seq === seq
  );
}

function statusBadge(status) {
  return el("span", { class: `badge ${status.toLowerCase()}` }, [status]);
}

function renderMessageRow(msg, model) {
  const isRequest = msg.message_kind === "request";
  const isNotification = msg.message_kind === "notification";
  const exchange = isRequest || msg.message_kind === "response"
    ? exchangeFor(model, msg.seq, isRequest)
    : undefined;

  const dirClass = msg.direction === "c2s" ? "dir-c2s" : "dir-s2c";
  const badge = isNotification
    ? statusBadge("notification")
    : exchange
      ? statusBadge(exchange.status)
      : null;

  const row = el("tr", { class: "msg-row" }, [
    el("td", { class: "mono" }, [String(msg.seq)]),
    el("td", { class: dirClass }, [msg.direction]),
    el("td", {}, [msg.message_kind]),
    el("td", { class: "mono" }, [msg.method || ""]),
    el("td", { class: "mono" }, [msg.tool_name || ""]),
    el("td", {}, badge ? [badge] : []),
    el("td", {}, [exchange && exchange.latency_ns != null ? fmtMs(exchange.latency_ns) : ""]),
    el("td", {}, [fmtTime(msg.ts_ns)]),
  ]);

  const payloadPanel = el("pre", { class: "payload mono" }, [
    JSON.stringify(msg.payload, null, 2),
  ]);

  row.addEventListener("click", () => {
    payloadPanel.classList.toggle("open");
  });

  return [row, el("tr", {}, [el("td", { colspan: "8" }, [payloadPanel])])];
}

async function renderSessionDetail(sessionId) {
  app.replaceChildren(el("div", {}, ["Loading session…"]));
  let data;
  try {
    data = await fetchJson(`/api/sessions/${sessionId}`);
  } catch (err) {
    renderError(String(err));
    return;
  }

  const { session, messages, model } = data;
  const stats = model.stats;

  const header = el("div", { class: "session-header" }, [
    el("a", { class: "back-link", href: "/" }, ["← all sessions"]),
    el("h1", { class: "mono" }, [session.id]),
    el("div", { class: "meta" }, [
      `${session.client} → ${session.server_command} (${session.transport}) · redaction: ${session.redaction_policy} · started ${fmtTime(session.started_at_ns)}`,
    ]),
  ]);

  const statsBar = el("div", { class: "stats-bar" }, [
    el("span", {}, [el("b", {}, [String(stats.total_exchanges)]), " exchanges"]),
    el("span", {}, [el("b", {}, [String(stats.ok)]), " ok"]),
    el("span", {}, [el("b", {}, [String(stats.errors)]), " error"]),
    el("span", {}, [el("b", {}, [String(stats.unanswered)]), " unanswered"]),
    el("span", {}, [el("b", {}, [String(stats.orphan_responses)]), " orphan"]),
    el("span", {}, [el("b", {}, [String(stats.notifications)]), " notifications"]),
    stats.latency_p50_ns != null
      ? el("span", {}, [
          "latency p50=",
          el("b", {}, [fmtMs(stats.latency_p50_ns)]),
          " p95=",
          el("b", {}, [fmtMs(stats.latency_p95_ns)]),
          " max=",
          el("b", {}, [fmtMs(stats.latency_max_ns)]),
        ])
      : null,
  ].filter(Boolean));

  if (messages.length === 0) {
    app.replaceChildren(header, statsBar, el("div", { class: "empty" }, ["No recorded messages."]));
    return;
  }

  const bodyRows = messages.flatMap((msg) => renderMessageRow(msg, model));
  const table = el("table", {}, [
    el("thead", {}, [
      el("tr", {}, [
        "seq",
        "dir",
        "kind",
        "method",
        "tool",
        "status",
        "latency",
        "time",
      ].map((h) => el("th", {}, [h]))),
    ]),
    el("tbody", {}, bodyRows),
  ]);

  app.replaceChildren(header, statsBar, table);
}

function route() {
  const match = window.location.pathname.match(/^\/session\/([^/]+)\/?$/);
  if (match) {
    renderSessionDetail(decodeURIComponent(match[1]));
  } else {
    renderSessionList();
  }
}

window.addEventListener("hashchange", () => {
  accessToken = loadAccessToken();
  route();
});

route();
