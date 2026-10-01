// MCPTracer inspector client.
// Plain DOM APIs with textContent for payload safety (no innerHTML on payloads).
// Supports session inspection, real-time filtering, visual diffing, and .mtrace exports.

const app = document.getElementById("app");
const tokenStorageKey = "mcptracer-inspector-token";
let accessToken = loadAccessToken();

function loadAccessToken() {
  const token = new URLSearchParams(window.location.hash.slice(1)).get("token");
  if (token !== null) {
    window.history.replaceState(null, "", window.location.pathname + window.location.search);
  }
  try {
    if (token !== null) window.sessionStorage.setItem(tokenStorageKey, token);
    return token ?? window.sessionStorage.getItem(tokenStorageKey);
  } catch {
    return token;
  }
}

function el(tag, attrs, children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs || {})) {
    if (key === "class") node.className = value;
    else if (key === "href") node.setAttribute("href", value);
    else if (key === "id") node.id = value;
    else if (key === "type") node.type = value;
    else if (key === "placeholder") node.placeholder = value;
    else if (key === "checked") node.checked = Boolean(value);
    else if (key === "disabled") node.disabled = Boolean(value);
    else node.setAttribute(key, value);
  }
  for (const child of children || []) {
    if (child == null) continue;
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

async function downloadExport(sessionId) {
  if (!accessToken) return;
  const allow = { unredacted: false, sensitive: false };
  try {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      const query = new URLSearchParams();
      if (allow.unredacted) query.set("allow_unredacted", "true");
      if (allow.sensitive) query.set("allow_sensitive_content", "true");
      const encodedQuery = query.toString();
      const suffix = encodedQuery ? `?${encodedQuery}` : "";
      const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/export${suffix}`, {
        headers: { Authorization: `Bearer ${accessToken}` },
        credentials: "omit",
        mode: "same-origin",
        cache: "no-store",
      });
      if (res.ok) {
        const blob = await res.blob();
        const url = window.URL.createObjectURL(blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = `${sessionId}.mtrace`;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        window.URL.revokeObjectURL(url);
        return;
      }
      const reason = await res.text();
      if (!allow.unredacted && reason.includes("--allow-unredacted")) {
        if (window.confirm("This session was recorded without redaction. Its export may contain secrets. Export it anyway?")) {
          allow.unredacted = true;
          continue;
        }
      } else if (!allow.sensitive && reason.includes("--allow-sensitive-content")) {
        if (window.confirm(`Sensitive content was detected in this export. The response lists finding locations and categories, not secret values. Export it anyway?\n\n${reason}`)) {
          allow.sensitive = true;
          continue;
        }
      }
      alert(`Export failed: ${reason}`);
      return;
    }
    alert("Export failed: consent checks did not complete.");
  } catch (err) {
    alert(`Export failed: ${err.message}`);
  }
}

function renderError(message) {
  app.replaceChildren(el("div", { class: "error-panel" }, [message]));
}

// Navigates without page reload when possible
function navigate(url) {
  window.history.pushState(null, "", url);
  route();
}

let selectedSessionIds = new Set();

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

  let searchQuery = "";
  const container = el("div", {});

  function updateTable() {
    const query = searchQuery.toLowerCase().trim();
    const filtered = sessions.filter((s) => {
      if (!query) return true;
      return (
        s.id.toLowerCase().includes(query) ||
        s.client.toLowerCase().includes(query) ||
        s.server_command.toLowerCase().includes(query) ||
        s.transport.toLowerCase().includes(query)
      );
    });

    const tbody = el("tbody", {});

    for (const s of filtered) {
      const isSelected = selectedSessionIds.has(s.id);
      const checkbox = el("input", {
        type: "checkbox",
        checked: isSelected,
      });

      checkbox.addEventListener("click", (e) => {
        e.stopPropagation();
        if (checkbox.checked) {
          selectedSessionIds.add(s.id);
        } else {
          selectedSessionIds.delete(s.id);
        }
        updateCompareBar();
        row.classList.toggle("selected", checkbox.checked);
      });

      const row = el(
        "tr",
        { class: `session-row ${isSelected ? "selected" : ""}` },
        [
          el("td", {}, [checkbox]),
          el("td", { class: "mono" }, [s.id.slice(0, 8)]),
          el("td", {}, [s.client]),
          el("td", { class: "mono" }, [s.server_command]),
          el("td", {}, [s.transport]),
          el("td", {}, [String(s.total_messages)]),
          el(
            "td",
            {},
            s.dropped_messages > 0
              ? [el("span", { class: "badge error" }, [String(s.dropped_messages)])]
              : [String(s.dropped_messages)]
          ),
          el("td", {}, [el("span", { class: "badge subtle" }, [s.redaction_policy])]),
          el("td", {}, [fmtTime(s.started_at_ns)]),
        ]
      );

      row.addEventListener("click", () => {
        navigate(`/session/${s.id}`);
      });

      tbody.appendChild(row);
    }

    const tableWrap = el("div", { class: "table-wrap" }, [
      el("table", {}, [
        el("thead", {}, [
          el("tr", {}, [
            el("th", { style: "width: 32px;" }, [""]),
            el("th", {}, ["id"]),
            el("th", {}, ["client"]),
            el("th", {}, ["server"]),
            el("th", {}, ["transport"]),
            el("th", {}, ["messages"]),
            el("th", {}, ["dropped"]),
            el("th", {}, ["redaction"]),
            el("th", {}, ["started"]),
          ]),
        ]),
        tbody,
      ]),
    ]);

    tableContainer.replaceChildren(tableWrap);
  }

  const searchInput = el("input", {
    id: "session-search",
    class: "search-input",
    placeholder: "Filter sessions by ID, client, server, or transport…",
    type: "text",
  });

  searchInput.addEventListener("input", (e) => {
    searchQuery = e.target.value;
    updateTable();
  });

  const toolbar = el("div", { class: "toolbar" }, [
    el("div", { class: "search-bar" }, [searchInput]),
    el("div", { class: "meta" }, [`${sessions.length} session(s) recorded`]),
  ]);

  const tableContainer = el("div", {});
  const compareBarContainer = el("div", {});

  function updateCompareBar() {
    if (selectedSessionIds.size === 2) {
      const ids = Array.from(selectedSessionIds);
      const compareBtn = el(
        "button",
        { id: "btn-compare", class: "btn btn-primary" },
        ["Compare 2 Selected Sessions →"]
      );
      compareBtn.addEventListener("click", () => {
        navigate(`/diff/${ids[0]}/${ids[1]}`);
      });

      const clearBtn = el("button", { class: "btn" }, ["Clear"]);
      clearBtn.addEventListener("click", () => {
        selectedSessionIds.clear();
        updateCompareBar();
        updateTable();
      });

      compareBarContainer.replaceChildren(
        el("div", { class: "compare-bar" }, [
          el("span", {}, ["2 sessions selected"]),
          compareBtn,
          clearBtn,
        ])
      );
    } else if (selectedSessionIds.size > 2) {
      compareBarContainer.replaceChildren(
        el("div", { class: "compare-bar" }, [
          el("span", {}, [`${selectedSessionIds.size} selected (select exactly 2 to compare)`]),
          el("button", { class: "btn" }, ["Clear"]).addEventListener("click", () => {
            selectedSessionIds.clear();
            updateCompareBar();
            updateTable();
          }),
        ])
      );
    } else {
      compareBarContainer.replaceChildren();
    }
  }

  container.replaceChildren(toolbar, tableContainer, compareBarContainer);
  app.replaceChildren(container);
  updateTable();
  updateCompareBar();
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

  return [row, el("tr", {}, [el("td", { colspan: "8", style: "padding: 0 12px;" }, [payloadPanel])])];
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

  const backLink = el("a", { class: "back-link", href: "/" }, ["← All Sessions"]);
  backLink.addEventListener("click", (e) => {
    e.preventDefault();
    navigate("/");
  });

  const exportBtn = el("button", { id: "btn-export", class: "btn btn-accent" }, [
    "↓ Export .mtrace",
  ]);
  exportBtn.addEventListener("click", () => {
    downloadExport(session.id);
  });

  const header = el("div", { class: "session-header" }, [
    backLink,
    el("div", { class: "session-header-top" }, [
      el("h1", { class: "mono" }, [session.id]),
      el("div", { class: "session-actions" }, [exportBtn]),
    ]),
    el("div", { class: "meta" }, [
      el("span", { class: "badge subtle" }, [session.client]),
      el("span", {}, ["→"]),
      el("span", { class: "mono" }, [session.server_command]),
      el("span", {}, ["·"]),
      el("span", {}, [session.transport]),
      el("span", {}, ["·"]),
      el("span", {}, [`redaction: ${session.redaction_policy}`]),
      el("span", {}, ["·"]),
      el("span", {}, [`started ${fmtTime(session.started_at_ns)}`]),
    ]),
  ]);

  const statsBar = el("div", { class: "stats-bar" }, [
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Exchanges"]),
      el("span", { class: "stat-value" }, [String(stats.total_exchanges)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["OK"]),
      el("span", { class: "stat-value", style: "color: var(--ok);" }, [String(stats.ok)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Error"]),
      el("span", { class: "stat-value", style: "color: var(--error);" }, [String(stats.errors)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Unanswered"]),
      el("span", { class: "stat-value" }, [String(stats.unanswered)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Cancelled"]),
      el("span", { class: "stat-value" }, [String(stats.cancelled)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Orphan"]),
      el("span", { class: "stat-value" }, [String(stats.orphan_responses)]),
    ]),
    el("div", { class: "stat-item" }, [
      el("span", { class: "stat-label" }, ["Notifications"]),
      el("span", { class: "stat-value" }, [String(stats.notifications)]),
    ]),
    stats.latency_p50_ns != null
      ? el("div", { class: "stat-item" }, [
          el("span", { class: "stat-label" }, ["Latency (p50 / p95)"]),
          el("span", { class: "stat-value" }, [
            `${fmtMs(stats.latency_p50_ns)} / ${fmtMs(stats.latency_p95_ns)}`,
          ]),
        ])
      : null,
  ]);

  if (messages.length === 0) {
    app.replaceChildren(header, statsBar, el("div", { class: "empty" }, ["No recorded messages."]));
    return;
  }

  let filterKind = "all";
  const messageContainer = el("div", {});

  function updateMessages() {
    const filtered = messages.filter((m) => {
      if (filterKind === "requests") return m.message_kind === "request";
      if (filterKind === "responses") return m.message_kind === "response";
      if (filterKind === "notifications") return m.message_kind === "notification";
      if (filterKind === "errors") return m.is_error;
      return true;
    });

    const bodyRows = filtered.flatMap((msg) => renderMessageRow(msg, model));
    const tableWrap = el("div", { class: "table-wrap" }, [
      el("table", {}, [
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
      ]),
    ]);
    messageContainer.replaceChildren(tableWrap);
  }

  const pills = [
    { id: "all", label: `All (${messages.length})` },
    { id: "requests", label: `Requests (${messages.filter(m => m.message_kind === 'request').length})` },
    { id: "responses", label: `Responses (${messages.filter(m => m.message_kind === 'response').length})` },
    { id: "notifications", label: `Notifications (${messages.filter(m => m.message_kind === 'notification').length})` },
    { id: "errors", label: `Errors (${messages.filter(m => m.is_error).length})` },
  ].map((item) => {
    const pill = el("span", { class: `pill ${item.id === filterKind ? "active" : ""}` }, [item.label]);
    pill.addEventListener("click", () => {
      filterKind = item.id;
      pills.forEach((p) => p.classList.remove("active"));
      pill.classList.add("active");
      updateMessages();
    });
    return pill;
  });

  const filterBar = el("div", { class: "filter-pills" }, pills);

  app.replaceChildren(header, statsBar, filterBar, messageContainer);
  updateMessages();
}

async function renderDiffView(id1, id2) {
  const explainSchema = new URLSearchParams(window.location.search).get("explain_schema") === "true";
  app.replaceChildren(el("div", {}, ["Analyzing session differences…"]));
  let data;
  try {
    const query = explainSchema ? "?explain_schema=true" : "";
    data = await fetchJson(`/api/diff/${encodeURIComponent(id1)}/${encodeURIComponent(id2)}${query}`);
  } catch (err) {
    renderError(String(err));
    return;
  }

  const { session_a, session_b, report } = data;

  const backLink = el("a", { class: "back-link", href: "/" }, ["← All Sessions"]);
  backLink.addEventListener("click", (e) => {
    e.preventDefault();
    navigate("/");
  });

  const header = el("div", { class: "diff-header" }, [
    backLink,
    el("div", { class: "diff-title" }, [
      el("h1", {}, ["Session Comparison & Diff"]),
      el("button", { class: "btn", type: "button" }, [
        explainSchema ? "Hide schema explanations" : "Explain schema changes",
      ]),
    ]),
    el("div", { class: "diff-meta-grid" }, [
      el("div", { class: "diff-session-box" }, [
        el("h3", {}, [
          el("span", { class: "badge subtle" }, ["Session A (Baseline)"]),
          el("a", { class: "mono", href: `/session/${session_a.id}` }, [session_a.id.slice(0, 8)]),
        ]),
        el("div", { class: "meta" }, [
          `${session_a.client} → ${session_a.server_command} (${session_a.transport})`,
        ]),
        el("div", { class: "meta" }, [`${session_a.total_messages} messages · started ${fmtTime(session_a.started_at_ns)}`]),
      ]),
      el("div", { class: "diff-session-box" }, [
        el("h3", {}, [
          el("span", { class: "badge subtle" }, ["Session B (Candidate)"]),
          el("a", { class: "mono", href: `/session/${session_b.id}` }, [session_b.id.slice(0, 8)]),
        ]),
        el("div", { class: "meta" }, [
          `${session_b.client} → ${session_b.server_command} (${session_b.transport})`,
        ]),
        el("div", { class: "meta" }, [`${session_b.total_messages} messages · started ${fmtTime(session_b.started_at_ns)}`]),
      ]),
    ]),
  ]);

  const explainButton = header.querySelector(".diff-title button");
  explainButton.addEventListener("click", () => {
    const next = new URL(window.location.href);
    if (explainSchema) next.searchParams.delete("explain_schema");
    else next.searchParams.set("explain_schema", "true");
    history.pushState({}, "", next);
    renderDiffView(id1, id2);
  });

  const cards = [];
  if (explainSchema) {
    const explanations = data.schema_explanations || [];
    const rows = explanations.map((item) =>
      el("div", { class: "pointer-diff-row" }, [
        el("div", {}, [
          el("b", {}, [`${item.tool} · ${item.schema} · ${item.pointer || "/"}`]),
          ` · ${item.rule_id} · ${item.classification}`,
        ]),
        el("div", { class: "meta" }, [item.before_summary + " → " + item.after_summary]),
        el("div", { class: "meta" }, [item.reason]),
        item.unsupported_keywords?.length
          ? el("div", { class: "meta" }, ["Unclassified keywords: " + item.unsupported_keywords.join(", ")])
          : null,
      ].filter(Boolean))
    );
    cards.push(
      el("div", { class: "diff-card" }, [
        el("div", { class: "diff-card-header" }, [
          `Tool Schema Explanations (advisory · ${data.analysis_status || "unavailable"})`,
        ]),
        el("div", { class: "diff-card-body" }, rows.length ? rows : [
          el("div", { class: "meta" }, [
            data.analysis_status === "incomplete_catalog"
              ? "Both tool catalogs must be complete before schema constraints can be compared."
              : "No supported schema changes were found; inspect the existing security findings for unsupported changes.",
          ]),
        ]),
      ])
    );
  }

  // 1. Point of Divergence Callout
  if (report.point_of_divergence) {
    const pod = report.point_of_divergence;
    cards.push(
      el("div", { class: "callout-divergence" }, [
        el("h4", {}, ["⚡ Earliest Point of Divergence"]),
        el("div", {}, [
          `Trajectories branched at step #${pod.step_index} (${pod.key}): `,
          el("b", {}, [pod.kind]),
          ` — ${pod.detail}`,
        ]),
      ])
    );
  }

  // 2. Security Findings (Rug-pull detection)
  if (report.security && report.security.length > 0) {
    const findingItems = report.security.map((f) =>
      el("li", {}, [
        el("b", {}, [f.kind]),
        ` on tool `,
        el("code", { class: "mono" }, [f.tool]),
        `: ${f.detail}`,
      ])
    );

    cards.push(
      el("div", { class: "callout-security" }, [
        el("h4", {}, ["🛡 Security / Rug-Pull Drift Detected"]),
        el("ul", { style: "margin: 6px 0 0; padding-left: 20px;" }, findingItems),
      ])
    );
  }

  // 3. Changed Exchanges
  if (report.changed && report.changed.length > 0) {
    const deltaItems = report.changed.map((change) => {
      const deltaDetails = change.deltas.map((d) => {
        if (d.kind === "status_changed") {
          return el("div", { class: "pointer-diff-row" }, [
            el("span", {}, ["Status changed: "]),
            el("span", { class: "diff-from" }, [d.from]),
            el("span", {}, ["→"]),
            el("span", { class: "diff-to" }, [d.to]),
          ]);
        }
        if (d.kind === "error_code_changed") {
          return el("div", { class: "pointer-diff-row" }, [
            el("span", {}, ["Error code: "]),
            el("span", { class: "diff-from" }, [String(d.from)]),
            el("span", {}, ["→"]),
            el("span", { class: "diff-to" }, [String(d.to)]),
          ]);
        }
        if (d.kind === "response_changed" || d.kind === "request_changed") {
          const pointerRows = (d.pointer_diffs || []).map((p) =>
            el("div", { class: "pointer-diff-row" }, [
              el("span", { class: "pointer-name" }, [p.pointer]),
              el("span", { class: "diff-from" }, [JSON.stringify(p.from)]),
              el("span", {}, ["→"]),
              el("span", { class: "diff-to" }, [JSON.stringify(p.to)]),
            ])
          );
          return el("div", {}, [
            el("div", { style: "font-weight: 500; margin: 4px 0;" }, [
              d.kind === "request_changed" ? "Request params changed:" : "Response body changed:",
            ]),
            ...pointerRows,
          ]);
        }
        if (d.kind === "latency_changed") {
          return el("div", { class: "pointer-diff-row" }, [
            el("span", {}, ["Latency delta: "]),
            el("span", {}, [`${fmtMs(d.from_ns)} → ${fmtMs(d.to_ns)} (${d.pct > 0 ? "+" : ""}${d.pct.toFixed(1)}%)`]),
          ]);
        }
        return el("div", {}, [JSON.stringify(d)]);
      });

      return el("div", { class: "delta-item" }, [
        el("div", { class: "delta-key mono" }, [change.key]),
        ...deltaDetails,
      ]);
    });

    cards.push(
      el("div", { class: "diff-card" }, [
        el("div", { class: "diff-card-header" }, [
          `Changed Exchanges (${report.changed.length})`,
        ]),
        el("div", { class: "diff-card-body" }, deltaItems),
      ])
    );
  }

  // 4. Added / Removed Exchanges
  if ((report.added && report.added.length > 0) || (report.removed && report.removed.length > 0)) {
    const addedList = (report.added || []).map((k) =>
      el("div", { class: "pointer-diff-row" }, [
        el("span", { class: "diff-to" }, [`+ ${k}`]),
      ])
    );
    const removedList = (report.removed || []).map((k) =>
      el("div", { class: "pointer-diff-row" }, [
        el("span", { class: "diff-from" }, [`- ${k}`]),
      ])
    );

    cards.push(
      el("div", { class: "diff-card" }, [
        el("div", { class: "diff-card-header" }, ["Structural Exchange Differences"]),
        el("div", { class: "diff-card-body" }, [...addedList, ...removedList]),
      ])
    );
  }

  // 5. Identical Result
  if (cards.length === 0) {
    cards.push(
      el("div", { class: "diff-card" }, [
        el("div", { class: "diff-card-body", style: "text-align: center; color: var(--ok); padding: 32px;" }, [
          el("div", { style: "font-size: 24px; margin-bottom: 8px;" }, ["✓"]),
          el("h3", { style: "margin: 0 0 6px;" }, ["Sessions Are Functionally Identical"]),
          el("p", { style: "margin: 0; color: var(--text-muted);" }, [
            "No meaningful differences found across exchanges, tool definitions, response payloads, or error rates under standard diff rules.",
          ]),
        ]),
      ])
    );
  }

  app.replaceChildren(header, ...cards);
}

function route() {
  const diffMatch = window.location.pathname.match(/^\/diff\/([^/]+)\/([^/]+)\/?$/);
  if (diffMatch) {
    renderDiffView(decodeURIComponent(diffMatch[1]), decodeURIComponent(diffMatch[2]));
    return;
  }
  const sessionMatch = window.location.pathname.match(/^\/session\/([^/]+)\/?$/);
  if (sessionMatch) {
    renderSessionDetail(decodeURIComponent(sessionMatch[1]));
    return;
  }
  renderSessionList();
}

window.addEventListener("hashchange", () => {
  accessToken = loadAccessToken();
  route();
});

window.addEventListener("popstate", () => {
  route();
});

route();
