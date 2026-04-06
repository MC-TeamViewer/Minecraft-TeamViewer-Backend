from __future__ import annotations


def render_admin_page() -> str:
    return """<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>TeamViewRelay Admin</title>
  <style>
    :root {
      --bg: #f4efe6;
      --panel: rgba(255, 251, 245, 0.92);
      --panel-strong: #fff9f2;
      --ink: #1f2937;
      --muted: #6b7280;
      --line: rgba(94, 82, 64, 0.18);
      --accent: #b45309;
      --accent-soft: #f59e0b;
      --accent-deep: #7c2d12;
      --success: #166534;
      --danger: #b91c1c;
      --shadow: 0 22px 54px rgba(68, 44, 14, 0.12);
    }

    * { box-sizing: border-box; }

    body {
      margin: 0;
      color: var(--ink);
      font-family: "Avenir Next", "Segoe UI", "PingFang SC", "Hiragino Sans GB", sans-serif;
      background:
        radial-gradient(circle at top left, rgba(245, 158, 11, 0.24), transparent 28%),
        radial-gradient(circle at top right, rgba(180, 83, 9, 0.18), transparent 24%),
        linear-gradient(180deg, #fbf7f0 0%, var(--bg) 100%);
      min-height: 100vh;
    }

    .shell {
      width: min(1240px, calc(100vw - 32px));
      margin: 0 auto;
      padding: 32px 0 48px;
    }

    .hero {
      padding: 28px;
      border: 1px solid var(--line);
      border-radius: 28px;
      background:
        linear-gradient(135deg, rgba(255, 248, 238, 0.96), rgba(255, 251, 245, 0.84)),
        linear-gradient(120deg, rgba(180, 83, 9, 0.08), transparent 60%);
      box-shadow: var(--shadow);
      position: relative;
      overflow: hidden;
    }

    .hero::after {
      content: "";
      position: absolute;
      inset: auto -8% -42% auto;
      width: 340px;
      height: 340px;
      background: radial-gradient(circle, rgba(245, 158, 11, 0.18), transparent 68%);
      pointer-events: none;
    }

    h1, h2 {
      margin: 0;
      font-family: Georgia, "Times New Roman", serif;
      letter-spacing: 0.02em;
    }

    h1 {
      font-size: clamp(2rem, 4vw, 3.1rem);
      line-height: 1;
    }

    .hero p {
      margin: 12px 0 0;
      max-width: 760px;
      color: var(--muted);
      font-size: 0.98rem;
    }

    .meta {
      margin-top: 20px;
      display: flex;
      flex-wrap: wrap;
      gap: 10px;
    }

    .tag {
      padding: 8px 12px;
      border-radius: 999px;
      background: rgba(124, 45, 18, 0.06);
      border: 1px solid rgba(124, 45, 18, 0.1);
      color: var(--accent-deep);
      font-size: 0.9rem;
    }

    .grid {
      display: grid;
      gap: 18px;
      margin-top: 20px;
    }

    .cards {
      grid-template-columns: repeat(4, minmax(0, 1fr));
    }

    .split {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }

    .panel {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 24px;
      padding: 20px;
      box-shadow: var(--shadow);
      backdrop-filter: blur(12px);
    }

    .card-value {
      margin-top: 10px;
      font-size: clamp(1.9rem, 3vw, 2.5rem);
      font-weight: 700;
      color: var(--accent-deep);
    }

    .card-label {
      color: var(--muted);
      font-size: 0.95rem;
    }

    .chart {
      display: grid;
      grid-template-rows: auto 1fr;
      gap: 16px;
      min-height: 320px;
      min-width: 0;
    }

    .chart-shell {
      display: grid;
      gap: 12px;
      min-width: 0;
    }

    .chart-stage {
      height: 240px;
      display: flex;
      align-items: stretch;
      gap: 4px;
      padding: 12px 12px 14px;
      border-radius: 18px;
      background:
        linear-gradient(180deg, rgba(255, 255, 255, 0.78), rgba(251, 241, 224, 0.78));
      border: 1px solid rgba(180, 83, 9, 0.12);
      overflow: hidden;
      position: relative;
      min-width: 0;
    }

    .chart-stage::before {
      content: "";
      position: absolute;
      inset: 12px 12px 14px 12px;
      background-image:
        linear-gradient(to top, rgba(124, 45, 18, 0.08) 1px, transparent 1px);
      background-size: 100% 25%;
      pointer-events: none;
    }

    .chart-bars {
      display: flex;
      align-items: flex-end;
      gap: 4px;
      width: 100%;
      min-width: 0;
      position: relative;
      z-index: 1;
    }

    .bar {
      flex: 1 1 0;
      min-width: 0;
      border-radius: 12px 12px 8px 8px;
      background: linear-gradient(180deg, var(--accent-soft), var(--accent));
      position: relative;
      transition: height 180ms ease;
      box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.28);
    }

    .bar:hover::after {
      content: attr(data-value);
      position: absolute;
      inset: -28px 50% auto auto;
      transform: translateX(50%);
      text-align: center;
      padding: 2px 6px;
      border-radius: 999px;
      background: rgba(124, 45, 18, 0.92);
      color: #fffaf2;
      white-space: nowrap;
      box-shadow: 0 8px 16px rgba(31, 41, 55, 0.18);
      pointer-events: none;
      z-index: 2;
    }

    .bar.is-empty {
      background: linear-gradient(180deg, rgba(180, 83, 9, 0.18), rgba(180, 83, 9, 0.08));
    }

    .chart-axis {
      display: flex;
      justify-content: space-between;
      gap: 12px;
      font-size: 0.76rem;
      color: var(--muted);
      min-width: 0;
    }

    .chart-axis span {
      flex: 0 1 auto;
      min-width: 0;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .chart-meta {
      display: flex;
      justify-content: space-between;
      align-items: center;
      gap: 12px;
      color: var(--accent-deep);
      font-size: 0.78rem;
      flex-wrap: wrap;
    }

    .chart-meta strong {
      color: var(--accent-deep);
    }

    .table-wrap {
      overflow: auto;
      border-radius: 18px;
      border: 1px solid rgba(124, 45, 18, 0.08);
      background: var(--panel-strong);
    }

    .panel-wide {
      min-width: 0;
    }

    .room-status-grid {
      grid-template-columns: minmax(0, 1.9fr) minmax(280px, 0.9fr);
    }

    .status-stack {
      display: grid;
      gap: 14px;
    }

    .status-block {
      padding: 16px 18px;
      border-radius: 18px;
      background:
        linear-gradient(180deg, rgba(255, 255, 255, 0.84), rgba(251, 241, 224, 0.88));
      border: 1px solid rgba(180, 83, 9, 0.12);
    }

    .status-kicker {
      color: var(--muted);
      font-size: 0.84rem;
      letter-spacing: 0.03em;
      text-transform: uppercase;
    }

    .status-value {
      margin-top: 8px;
      font-size: clamp(1.8rem, 2.6vw, 2.3rem);
      font-weight: 700;
      color: var(--accent-deep);
      line-height: 1;
    }

    .status-caption {
      margin-top: 8px;
      color: var(--muted);
      font-size: 0.9rem;
    }

    .status-inline-values {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 12px;
    }

    .status-inline-values .status-block {
      height: 100%;
    }

    .audit-section {
      margin-top: 18px;
    }

    table {
      width: 100%;
      border-collapse: collapse;
      font-size: 0.92rem;
    }

    th, td {
      padding: 12px 14px;
      text-align: left;
      border-bottom: 1px solid rgba(94, 82, 64, 0.1);
      vertical-align: top;
    }

    th {
      background: rgba(180, 83, 9, 0.05);
      color: var(--accent-deep);
      font-weight: 600;
      position: relative;
    }

    .resizable-table {
      table-layout: fixed;
    }

    .resizable-table th,
    .resizable-table td {
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .th-inner {
      position: relative;
      padding-right: 14px;
    }

    .col-resizer {
      position: absolute;
      top: -12px;
      right: -14px;
      width: 18px;
      height: calc(100% + 24px);
      cursor: col-resize;
      user-select: none;
    }

    .col-resizer::after {
      content: "";
      position: absolute;
      top: 10px;
      bottom: 10px;
      left: 8px;
      width: 2px;
      border-radius: 999px;
      background: rgba(124, 45, 18, 0.18);
    }

    .col-resizer:hover::after,
    .is-resizing .col-resizer::after {
      background: rgba(180, 83, 9, 0.72);
    }

    .filters {
      display: flex;
      flex-wrap: wrap;
      gap: 10px;
      margin-bottom: 14px;
    }

    .filters input,
    .filters select,
    .filters button {
      border-radius: 14px;
      border: 1px solid rgba(124, 45, 18, 0.16);
      background: #fffdf9;
      color: var(--ink);
      padding: 10px 12px;
      font: inherit;
    }

    .filters button {
      cursor: pointer;
      background: linear-gradient(180deg, #f59e0b, #d97706);
      color: white;
      border: none;
      min-width: 108px;
    }

    .checkbox-group {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      align-items: center;
    }

    .checkbox-chip {
      display: inline-flex;
      align-items: center;
      gap: 8px;
      border-radius: 999px;
      border: 1px solid rgba(124, 45, 18, 0.16);
      background: #fffdf9;
      padding: 9px 12px;
      color: var(--ink);
      font: inherit;
      cursor: pointer;
      user-select: none;
    }

    .checkbox-chip input {
      margin: 0;
      accent-color: var(--accent);
    }

    .status-ok {
      color: var(--success);
      font-weight: 600;
    }

    .status-fail {
      color: var(--danger);
      font-weight: 600;
    }

    .muted {
      color: var(--muted);
    }

    .inline-code {
      font-family: "IBM Plex Mono", "SFMono-Regular", Consolas, monospace;
      font-size: 0.84rem;
      white-space: pre-wrap;
      word-break: break-word;
    }

    .notice {
      margin-top: 12px;
      color: var(--muted);
      font-size: 0.88rem;
    }

    .audit-detail {
      max-height: 180px;
      overflow: auto;
      margin: 0;
    }

    .connection-detail-wrap {
      margin-top: 14px;
    }

    @media (max-width: 980px) {
      .cards, .split, .room-status-grid, .status-inline-values {
        grid-template-columns: 1fr;
      }

      .shell {
        width: min(100vw - 20px, 1240px);
        padding-top: 20px;
      }

      .hero, .panel {
        border-radius: 20px;
      }
    }
  </style>
</head>
<body>
  <div class="shell">
    <section class="hero">
      <h1>TeamViewRelay Admin</h1>
      <p>只读管理后台，汇总在线概况、最近 30 天 DAU、最近 48 小时活跃趋势和审计日志。页面依赖同源 Basic Auth，无需额外前端构建链。</p>
      <div class="meta" id="hero-meta">
        <span class="tag">Loading overview...</span>
      </div>
    </section>

    <section class="grid cards" id="overview-cards">
      <article class="panel"><div class="card-label">在线玩家</div><div class="card-value">-</div></article>
      <article class="panel"><div class="card-label">在线 Web Map</div><div class="card-value">-</div></article>
      <article class="panel"><div class="card-label">活跃房间</div><div class="card-value">-</div></article>
      <article class="panel"><div class="card-label">24h 峰值</div><div class="card-value">-</div></article>
    </section>

    <section class="grid split">
      <article class="panel chart">
        <div>
          <h2>最近 30 天 DAU</h2>
          <div class="notice">按玩家 UUID 去重，默认统计全局玩家。</div>
        </div>
        <div class="chart-shell">
          <div class="chart-stage">
            <div class="chart-bars" id="daily-chart"></div>
          </div>
          <div class="chart-axis" id="daily-labels"></div>
          <div class="chart-meta" id="daily-meta"></div>
        </div>
      </article>

      <article class="panel chart">
        <div>
          <h2>最近 48 小时活跃</h2>
          <div class="notice">按本地时区整点统计，空桶自动补零。</div>
        </div>
        <div class="chart-shell">
          <div class="chart-stage">
            <div class="chart-bars" id="hourly-chart"></div>
          </div>
          <div class="chart-axis" id="hourly-labels"></div>
          <div class="chart-meta" id="hourly-meta"></div>
        </div>
      </article>
    </section>

    <section class="grid room-status-grid">
      <article class="panel panel-wide">
        <h2>房间概览</h2>
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Room</th>
                <th>玩家连接</th>
                <th>Web Map</th>
                <th>玩家 ID</th>
              </tr>
            </thead>
            <tbody id="rooms-table"></tbody>
          </table>
        </div>
      </article>

      <article class="panel">
        <h2>当前连接状态</h2>
        <div class="notice">统计的是已经完成 WebSocket 握手并登记到服务端内存态的连接。</div>
        <div class="status-stack">
          <div class="status-block">
            <div class="status-kicker">当前总连接</div>
            <div class="status-value" id="current-total-connections">-</div>
            <div class="status-caption">游戏端 + 网页端（已完成握手登记）</div>
          </div>
          <div class="status-inline-values">
            <div class="status-block">
              <div class="status-kicker">游戏端</div>
              <div class="status-value" id="current-player-connections">-</div>
              <div class="status-caption">/mc-client 握手登记数量</div>
            </div>
            <div class="status-block">
              <div class="status-kicker">网页端</div>
              <div class="status-value" id="current-webmap-connections">-</div>
              <div class="status-caption">/web-map/ws 握手登记数量</div>
            </div>
          </div>
          <div class="status-block">
            <div class="status-kicker">活跃房间</div>
            <div class="status-value" id="current-active-rooms">-</div>
            <div class="status-caption">当前至少存在一个已登记连接的房间数</div>
          </div>
          <div class="status-block connection-detail-wrap">
            <div class="status-kicker">连接详情</div>
            <div class="status-caption">按当前已完成握手登记的连接展开，包含名字、房间、协议版本与程序版本。</div>
            <div class="table-wrap" style="margin-top: 12px;">
              <table>
                <thead>
                  <tr>
                    <th>类型</th>
                    <th>名字</th>
                    <th>房间</th>
                    <th>协议</th>
                    <th>程序版本</th>
                    <th>地址</th>
                  </tr>
                </thead>
                <tbody id="connection-details-table"></tbody>
              </table>
            </div>
          </div>
        </div>
      </article>
    </section>

    <section class="audit-section">
      <article class="panel panel-wide">
        <h2>审计日志</h2>
        <div class="filters">
          <select id="audit-event-type">
            <option value="">全部事件</option>
          </select>
          <div class="checkbox-group" id="audit-actor-types">
            <label class="checkbox-chip"><input type="checkbox" value="player" checked>游戏端</label>
            <label class="checkbox-chip"><input type="checkbox" value="web_map" checked>网页端</label>
            <label class="checkbox-chip"><input type="checkbox" value="system" checked>系统</label>
            <label class="checkbox-chip"><input type="checkbox" value="admin">管理端</label>
          </div>
          <select id="audit-success">
            <option value="">全部结果</option>
            <option value="true">成功</option>
            <option value="false">失败</option>
          </select>
          <button id="audit-refresh" type="button">刷新日志</button>
        </div>
        <div class="table-wrap">
          <table class="resizable-table" id="audit-resizable-table">
            <colgroup>
              <col style="width: 84px">
              <col style="width: 180px">
              <col style="width: 200px">
              <col style="width: 240px">
              <col style="width: 120px">
              <col style="width: 520px">
            </colgroup>
            <thead>
              <tr>
                <th><div class="th-inner">ID<span class="col-resizer"></span></div></th>
                <th><div class="th-inner">时间<span class="col-resizer"></span></div></th>
                <th><div class="th-inner">事件<span class="col-resizer"></span></div></th>
                <th><div class="th-inner">角色<span class="col-resizer"></span></div></th>
                <th><div class="th-inner">结果<span class="col-resizer"></span></div></th>
                <th><div class="th-inner">详情</div></th>
              </tr>
            </thead>
            <tbody id="audit-table"></tbody>
          </table>
        </div>
      </article>
    </section>
  </div>

  <script>
    const state = {
      overview: null,
      daily: [],
      hourly: [],
      audit: [],
      liveStatus: "Initializing",
      eventSource: null,
      reconnectTimer: null,
    };

    async function fetchJson(url) {
      const response = await fetch(url, { headers: { "Accept": "application/json" } });
      if (!response.ok) {
        throw new Error(`${response.status} ${response.statusText}`);
      }
      return response.json();
    }

    function compactLabel(value, type) {
      if (type === "daily") {
        return value.slice(5);
      }
      return value.slice(11, 16);
    }

    function buildAxisLabels(items, type) {
      if (!items.length) {
        return [];
      }

      const targetCount = type === "daily" ? 6 : 8;
      const lastIndex = items.length - 1;
      const indexes = new Set([0, lastIndex]);
      for (let step = 1; step < targetCount - 1; step += 1) {
        indexes.add(Math.round((lastIndex * step) / (targetCount - 1)));
      }

      return Array.from(indexes)
        .sort((left, right) => left - right)
        .map((index) => compactLabel(items[index].label, type));
    }

    function renderBars(targetId, labelsId, metaId, items, type) {
      const target = document.getElementById(targetId);
      const labels = document.getElementById(labelsId);
      const meta = document.getElementById(metaId);
      target.innerHTML = "";
      labels.innerHTML = "";
      meta.innerHTML = "";
      const max = Math.max(1, ...items.map((item) => item.activePlayers || 0));
      const sum = items.reduce((total, item) => total + (item.activePlayers || 0), 0);
      const nonZero = items.filter((item) => (item.activePlayers || 0) > 0).length;

      items.forEach((item) => {
        const bar = document.createElement("div");
        bar.className = "bar";
        if ((item.activePlayers || 0) === 0) {
          bar.classList.add("is-empty");
        }
        bar.style.height = `${Math.max(6, ((item.activePlayers || 0) / max) * 100)}%`;
        bar.dataset.value = `${item.label}: ${item.activePlayers || 0}`;
        bar.title = `${item.label}: ${item.activePlayers || 0}`;
        target.appendChild(bar);
      });

      buildAxisLabels(items, type).forEach((text) => {
        const label = document.createElement("span");
        label.textContent = text;
        labels.appendChild(label);
      });

      meta.innerHTML = `
        <span>最高值 <strong>${max}</strong></span>
        <span>总活跃计数 <strong>${sum}</strong></span>
        <span>非空时间桶 <strong>${nonZero}</strong></span>
      `;
    }

    function renderOverview(overview) {
      state.overview = overview;
      const cards = [
        { label: "在线玩家", value: overview.playerConnections },
        { label: "在线 Web Map", value: overview.webMapConnections },
        { label: "活跃房间", value: overview.activeRooms },
        { label: "24h 峰值", value: overview.hourlyPeak24h },
      ];

      document.getElementById("overview-cards").innerHTML = cards.map((card) => `
        <article class="panel">
          <div class="card-label">${card.label}</div>
          <div class="card-value">${card.value}</div>
        </article>
      `).join("");

      document.getElementById("hero-meta").innerHTML = `
        <span class="tag">Timezone: ${overview.timezone}</span>
        <span class="tag">DB: ${overview.dbPathMasked}</span>
        <span class="tag">Broadcast Hz: ${overview.broadcastHz}</span>
        <span class="tag" id="live-status-tag">SSE: ${state.liveStatus}</span>
      `;

      document.getElementById("current-total-connections").textContent =
        String((overview.playerConnections || 0) + (overview.webMapConnections || 0));
      document.getElementById("current-player-connections").textContent =
        String(overview.playerConnections || 0);
      document.getElementById("current-webmap-connections").textContent =
        String(overview.webMapConnections || 0);
      document.getElementById("current-active-rooms").textContent =
        String(overview.activeRooms || 0);
      const connectionDetails = overview.connectionDetails || [];
      document.getElementById("connection-details-table").innerHTML = connectionDetails.length ? connectionDetails.map((item) => `
        <tr>
          <td>${item.channel === "player" ? "游戏端" : "网页端"}</td>
          <td>${item.displayName || item.actorId || "-"}</td>
          <td>${item.roomCode || "-"}</td>
          <td class="inline-code">${item.protocolVersion || "-"}</td>
          <td class="inline-code">${item.programVersion || "-"}</td>
          <td class="inline-code">${item.remoteAddr || "-"}</td>
        </tr>
      `).join("") : '<tr><td colspan="6" class="muted">当前没有已登记连接</td></tr>';

      const rooms = overview.rooms || [];
      document.getElementById("rooms-table").innerHTML = rooms.length ? rooms.map((room) => `
        <tr>
          <td>${room.roomCode}</td>
          <td>${room.playerConnections}</td>
          <td>${room.webMapConnections}</td>
          <td class="inline-code">${(room.playerIds || []).join("\\n") || "-"}</td>
        </tr>
      `).join("") : '<tr><td colspan="4" class="muted">当前没有在线房间</td></tr>';
    }

    function renderAudit(items) {
      state.audit = items;
      const tbody = document.getElementById("audit-table");
      tbody.innerHTML = items.length ? items.map((item) => `
        <tr>
          <td>${item.id}</td>
          <td>${item.localHour}</td>
          <td>${item.eventType}</td>
          <td>${item.actorType}${item.actorId ? `<div class="muted inline-code">${item.actorId}</div>` : ""}</td>
          <td class="${item.success ? "status-ok" : "status-fail"}">${item.success ? "成功" : "失败"}</td>
          <td><pre class="inline-code audit-detail">${escapeHtml(JSON.stringify(item.detail || {}, null, 2))}</pre></td>
        </tr>
      `).join("") : '<tr><td colspan="6" class="muted">暂无日志</td></tr>';

      populateAuditFilters(items);
    }

    function populateAuditFilters(items) {
      const eventTypes = Array.from(new Set(items.map((item) => item.eventType).filter(Boolean))).sort();

      const eventSelect = document.getElementById("audit-event-type");
      const currentEvent = eventSelect.value;

      eventSelect.innerHTML = '<option value="">全部事件</option>' + eventTypes.map((value) => `<option value="${value}">${value}</option>`).join("");
      eventSelect.value = currentEvent;
    }

    function escapeHtml(value) {
      return value
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;");
    }

    function setLiveStatus(status) {
      state.liveStatus = status;
      const tag = document.getElementById("live-status-tag");
      if (tag) {
        tag.textContent = `SSE: ${status}`;
      }
    }

    function readAuditFilters() {
      const eventType = document.getElementById("audit-event-type").value;
      const actorTypes = Array.from(document.querySelectorAll("#audit-actor-types input:checked"))
        .map((input) => input.value)
        .filter(Boolean);
      const success = document.getElementById("audit-success").value;
      return {
        eventType,
        actorTypes,
        success,
      };
    }

    function buildSseUrl() {
      const params = new URLSearchParams({ auditLimit: "100" });
      const filters = readAuditFilters();
      if (filters.eventType) params.set("auditEventType", filters.eventType);
      filters.actorTypes.forEach((value) => params.append("auditActorTypes", value));
      if (filters.success) params.set("auditSuccess", filters.success);
      return `/admin/api/events?${params.toString()}`;
    }

    function clearReconnectTimer() {
      if (state.reconnectTimer !== null) {
        window.clearTimeout(state.reconnectTimer);
        state.reconnectTimer = null;
      }
    }

    function closeEventSource() {
      clearReconnectTimer();
      if (state.eventSource) {
        state.eventSource.close();
        state.eventSource = null;
      }
    }

    function applyBootstrap(payload) {
      if (payload.overview) {
        renderOverview(payload.overview);
      }
      if (payload.dailyMetrics) {
        state.daily = payload.dailyMetrics.items || [];
        renderBars("daily-chart", "daily-labels", "daily-meta", state.daily, "daily");
      }
      if (payload.hourlyMetrics) {
        state.hourly = payload.hourlyMetrics.items || [];
        renderBars("hourly-chart", "hourly-labels", "hourly-meta", state.hourly, "hourly");
      }
      if (payload.audit) {
        renderAudit(payload.audit.items || []);
      }
    }

    function scheduleSseReconnect() {
      if (state.reconnectTimer !== null) {
        return;
      }
      setLiveStatus("Reconnecting");
      state.reconnectTimer = window.setTimeout(() => {
        state.reconnectTimer = null;
        connectSse();
      }, 2000);
    }

    function connectSse() {
      if (!window.EventSource) {
        setLiveStatus("Unsupported");
        return;
      }

      closeEventSource();
      setLiveStatus("Connecting");
      const source = new EventSource(buildSseUrl());
      state.eventSource = source;

      source.addEventListener("open", () => {
        setLiveStatus("Live");
      });

      source.addEventListener("bootstrap", (event) => {
        applyBootstrap(JSON.parse(event.data));
        setLiveStatus("Live");
      });

      source.addEventListener("overview", (event) => {
        renderOverview(JSON.parse(event.data));
      });

      source.addEventListener("daily_metrics", (event) => {
        const payload = JSON.parse(event.data);
        state.daily = payload.items || [];
        renderBars("daily-chart", "daily-labels", "daily-meta", state.daily, "daily");
      });

      source.addEventListener("hourly_metrics", (event) => {
        const payload = JSON.parse(event.data);
        state.hourly = payload.items || [];
        renderBars("hourly-chart", "hourly-labels", "hourly-meta", state.hourly, "hourly");
      });

      source.addEventListener("audit", (event) => {
        const payload = JSON.parse(event.data);
        renderAudit(payload.items || []);
      });

      source.addEventListener("heartbeat", () => {
        setLiveStatus("Live");
      });

      source.onerror = () => {
        if (state.eventSource === source) {
          source.close();
          state.eventSource = null;
        }
        scheduleSseReconnect();
      };
    }

    function reconnectSse() {
      closeEventSource();
      connectSse();
    }

    async function loadOverview() {
      const overview = await fetchJson("/admin/api/overview");
      renderOverview(overview);
    }

    async function loadDaily() {
      const daily = await fetchJson("/admin/api/metrics/daily?days=30");
      state.daily = daily.items || [];
      renderBars("daily-chart", "daily-labels", "daily-meta", state.daily, "daily");
    }

    async function loadHourly() {
      const hourly = await fetchJson("/admin/api/metrics/hourly?hours=48");
      state.hourly = hourly.items || [];
      renderBars("hourly-chart", "hourly-labels", "hourly-meta", state.hourly, "hourly");
    }

    async function loadAudit() {
      const params = new URLSearchParams({ limit: "100" });
      const filters = readAuditFilters();
      if (filters.eventType) params.set("eventType", filters.eventType);
      filters.actorTypes.forEach((value) => params.append("actorTypes", value));
      if (filters.success) params.set("success", filters.success);
      const audit = await fetchJson(`/admin/api/audit?${params.toString()}`);
      renderAudit(audit.items || []);
    }

    async function bootstrap() {
      enableResizableTable("audit-resizable-table");
      try {
        await Promise.all([loadOverview(), loadDaily(), loadHourly(), loadAudit()]);
      } catch (error) {
        document.getElementById("hero-meta").innerHTML = `<span class="tag">Load failed: ${error.message}</span>`;
      }
      connectSse();
    }

    function enableResizableTable(tableId) {
      const table = document.getElementById(tableId);
      if (!table || table.dataset.resizableReady === "true") {
        return;
      }

      const columns = Array.from(table.querySelectorAll("colgroup col"));
      const handles = Array.from(table.querySelectorAll(".col-resizer"));

      handles.forEach((handle, index) => {
        handle.addEventListener("mousedown", (event) => {
          event.preventDefault();
          const startX = event.clientX;
          const startWidth = columns[index].getBoundingClientRect().width;

          function onMouseMove(moveEvent) {
            const nextWidth = Math.max(72, startWidth + moveEvent.clientX - startX);
            columns[index].style.width = `${nextWidth}px`;
            table.classList.add("is-resizing");
          }

          function onMouseUp() {
            table.classList.remove("is-resizing");
            window.removeEventListener("mousemove", onMouseMove);
            window.removeEventListener("mouseup", onMouseUp);
          }

          window.addEventListener("mousemove", onMouseMove);
          window.addEventListener("mouseup", onMouseUp);
        });
      });

      table.dataset.resizableReady = "true";
    }

    async function handleAuditSubscriptionChange() {
      try {
        await loadAudit();
      } catch (error) {
        console.error(error);
      }
      reconnectSse();
    }

    document.getElementById("audit-refresh").addEventListener("click", handleAuditSubscriptionChange);
    document.getElementById("audit-event-type").addEventListener("change", handleAuditSubscriptionChange);
    document.querySelectorAll("#audit-actor-types input").forEach((input) => {
      input.addEventListener("change", handleAuditSubscriptionChange);
    });
    document.getElementById("audit-success").addEventListener("change", handleAuditSubscriptionChange);
    window.addEventListener("beforeunload", closeEventSource);

    bootstrap();
  </script>
</body>
</html>
"""
