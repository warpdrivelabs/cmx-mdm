/**
 * MDM 分发监控（native-page）——KPI + 三 Tab + 轮询。
 *
 * 布局：
 *  - KPI 行 cmx-kpi-card×6：今日投递 / 成功率 / 平均耗时 / 积压 / 死信（>0 红色 + 页顶告警条）/ 扇出滞后
 *  - Tab1 投递流水：过滤（订阅 id / 状态 / 时间从到）+ cmx-revo-grid + cmx-pager，行操作「详情」
 *    （详情 = cmx-floating-dialog 内 <pre> 展示 /mdm/dispatches/detail 全量 JSON）。
 *    注：投递行无 event_type 列（md_dispatch_log 无此列，详情接口 join 事件表才有），
 *    故流水表以 event_seq 替代设计稿中的 event_type 列。
 *  - Tab2 事件日志：GET /mdm/events 分页表（cmx-pager）+ 下方 pull 消费者游标表（/mdm/events/offsets）；
 *    payload 列截断展示，点击弹框查看格式化 JSON 并可复制（悬浮 title 看不全且无法复制）。
 *  - Tab3 死信处理：查 dispatches {status:"dead"} + 行 checkbox / 全选 → [批量重发][批量跳过]
 *    （cmx-pager 分页；最近错误列点击弹框查看完整错误并可复制）。
 *  - 页头「手动补发」：小对话框 subscriptionId/dictCode/fromSeq/toSeq/force → POST /mdm/publish。
 *
 * 轮询：stats 30s、当前 Tab 数据 60s（单例 interval + hosts Set + host.isConnected 清理，防多 tab 泄漏）；
 * 刷新只做局部更新（KPI value / grid setDataSet / tbody 重建 / pager），不整页重绘。
 *
 * 入参 context（订阅管理页「投递」跳转）：{ subscriptionId, subscriptionName } 预填流水过滤。
 * 多实例安全：state 按 host 隔离（WeakMap）。
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }；
 * CMX 能力经 globalThis.__cmxDataComp 取用（禁止裸 import）。
 */

const cmx = () => (typeof globalThis !== 'undefined' && globalThis.__cmxDataComp) || {}

// HTML 转义：优先用组件库挂载的权威 escHtml，缺省时本地兜底（覆盖 & < > " '）。
function esc(s) {
  const c = cmx()
  if (c && typeof c.escHtml === 'function') return c.escHtml(s)
  return String(s ?? '').replace(/[&<>"']/g, (ch) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]))
}

function unwrap(res, body) {
  if (body && typeof body === 'object' && typeof body.code === 'number') {
    if (body.code !== 0) { const e = new Error(body.msg || body.error || `业务错误 ${body.code}`); e.body = body; throw e }
    return body.data
  }
  if (!res.ok) { const e = new Error((body && (body.msg || body.error)) || `HTTP ${res.status}`); e.status = res.status; throw e }
  return body
}
async function apiGet(url, dbId) {
  const h = { Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { headers: h, credentials: 'same-origin' })
  return unwrap(r, await r.json().catch(() => null))
}
async function apiPost(url, payload, dbId) {
  const h = { 'Content-Type': 'application/json', Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { method: 'POST', headers: h, credentials: 'same-origin', body: JSON.stringify(payload || {}) })
  return unwrap(r, await r.json().catch(() => null))
}

// 轻量 toast ——对齐 activation-mapper 提示范式；校验/异常用 cmxWarn/cmxError。
let _toastTimer = null
function showToast(message, tone = 'ok', duration = 3000) {
  let el = document.getElementById('cmx-native-toast')
  if (!el) {
    el = document.createElement('div')
    el.id = 'cmx-native-toast'
    el.style.cssText = 'position:fixed;top:24px;left:50%;transform:translateX(-50%);z-index:99999;display:flex;align-items:center;gap:8px;padding:10px 18px;border-radius:8px;font:500 14px/1.4 var(--sapFontFamily,Arial,sans-serif);box-shadow:0 4px 16px rgba(0,0,0,.16);pointer-events:none;opacity:0;transition:opacity .18s ease'
    document.body.appendChild(el)
    const icon = document.createElement('span')
    icon.style.cssText = 'display:inline-flex;width:16px;height:16px;flex-shrink:0'
    const text = document.createElement('span')
    el.appendChild(icon); el.appendChild(text)
    el._icon = icon; el._text = text
  }
  if (_toastTimer) { clearTimeout(_toastTimer); _toastTimer = null }
  const isErr = tone === 'err'
  el.style.color = isErr ? 'var(--sapNegativeTextColor,#b00)' : 'var(--sapPositiveTextColor,#107e3e)'
  el.style.background = isErr ? 'color-mix(in srgb,#b00 10%,#fff)' : 'color-mix(in srgb,#107e3e 10%,#fff)'
  el.style.border = `1px solid ${isErr ? 'color-mix(in srgb,#b00 24%,transparent)' : 'color-mix(in srgb,#107e3e 24%,transparent)'}`
  el._icon.innerHTML = isErr
    ? '<svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M8 1a7 7 0 100 14A7 7 0 008 1zm0 12.5A5.5 5.5 0 118 2.5a5.5 5.5 0 010 11zM7.25 4h1.5v5h-1.5V4zm0 6h1.5v1.5h-1.5V10z"/></svg>'
    : '<svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor"><path d="M8 1a7 7 0 100 14A7 7 0 008 1zm3.4 5.1L7 10.5 4.6 8.1l1-1L7 8.5l3.4-3.4 1 1z"/></svg>'
  el._text.textContent = String(message ?? '')
  requestAnimationFrame(() => { el.style.opacity = '1' })
  _toastTimer = setTimeout(() => { el.style.opacity = '0'; _toastTimer = null }, duration)
}

// ── 状态映射（后端 status 全英文，前端统一中文展示）────────────────────────
const D_STATUS = {
  pending: { name: '待投递', tone: 'info' },
  running: { name: '投递中', tone: 'warning' },
  delivered: { name: '已送达', tone: 'success' },
  failed: { name: '失败', tone: 'danger' },
  dead: { name: '死信', tone: 'danger' },
  skipped: { name: '已跳过', tone: 'neutral' },
}
const statusName = (s) => (D_STATUS[s] || {}).name || s || ''

// 后端时间均为 UTC（RFC3339，如 2026-08-18T14:20:35.679665+00:00）——统一换算成东八区墙钟展示，
// 不直接截取字符串（截出来是 UTC，比北京少 8 小时，排序观感错乱）。无时区标记的 naive 串视为已是本地时间。
function fmtTime(t) {
  if (t == null || t === '') return ''
  const s = String(t).trim()
  if (!/[Zz]|[+-]\d{2}:?\d{2}$/.test(s)) return s.replace('T', ' ').slice(0, 19)
  const d = new Date(s.replace(/\.(\d{3})\d+/, '.$1'))   // 截掉 >3 位小数秒，兼容各引擎解析
  if (isNaN(d.getTime())) return s.replace('T', ' ').slice(0, 19)
  const b = new Date(d.getTime() + 8 * 60 * 60 * 1000)   // 东八区 = UTC+8，取 UTC 分量即北京墙钟
  const p = (n) => String(n).padStart(2, '0')
  return `${b.getUTCFullYear()}-${p(b.getUTCMonth() + 1)}-${p(b.getUTCDate())} ${p(b.getUTCHours())}:${p(b.getUTCMinutes())}:${p(b.getUTCSeconds())}`
}
function trunc(s, n) { const v = s == null ? '' : String(s); return v.length > n ? `${v.slice(0, n)}…` : v }

// ── 按 host 隔离的 state（多实例安全）──────────────────────────────────────
const _hostState = new WeakMap()
function initState() {
  return {
    dbId: '',
    stats: null,
    activeTab: 'dispatch', tick: 0,          // 轮询计数（偶数 tick 刷当前 Tab = 60s）
    subId: null, subName: '',                 // 订阅管理页跳入预填（可清除）
    // Tab1 投递流水
    dF: { subscriptionId: '', status: '', timeFrom: '', timeTo: '' },
    dRows: [], dTotal: 0, dPage: 1, dPageSize: 20, dLoaded: false, dGrid: null,
    // Tab2 事件日志
    eDict: '', eRows: [], eTotal: 0, ePage: 1, ePageSize: 20, eLoaded: false,
    offsets: [],
    // Tab3 死信处理
    deadRows: [], deadTotal: 0, deadPage: 1, deadPageSize: 20, deadLoaded: false,
    deadSel: new Set(),
  }
}
function getState(host) { if (host && !_hostState.has(host)) _hostState.set(host, initState()); return host ? _hostState.get(host) : null }

// ── 单例轮询（多 host 共享一个 interval；断连 host 自动清出集合）──────────
let _pollTimer = null
const _pollHosts = new Set()
function ensurePolling(host) {
  _pollHosts.add(host)
  if (_pollTimer) return
  _pollTimer = setInterval(() => {
    if (!_pollHosts.size) { clearInterval(_pollTimer); _pollTimer = null; return }
    for (const h of Array.from(_pollHosts)) {
      if (!(h && h.isConnected)) { _pollHosts.delete(h); continue }
      const st = getState(h)
      if (!st) { _pollHosts.delete(h); continue }
      st.tick++
      refreshStats(h).catch(() => {})                    // 30s：KPI
      if (st.tick % 2 === 0) refreshActiveTab(h).catch(() => {})  // 60s：当前 Tab
    }
  }, 30000)
}

function styleCss() {
  return `
  .pg { height:100%; overflow:hidden; display:flex; flex-direction:column; box-sizing:border-box; padding:12px 20px 16px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .pg-head { display:flex; justify-content:space-between; align-items:flex-start; gap:10px; margin-bottom:10px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor); margin-top:2px; }
  .alarm { display:none; align-items:center; gap:8px; padding:8px 14px; margin-bottom:10px; border-radius:6px;
    background:var(--sapErrorBackground,#ffebeb); border:1px solid var(--sapErrorBorderColor,#f08080);
    color:var(--sapNegativeTextColor,#b00); font-size:13px; }
  .alarm.on { display:flex; }
  .kpi-row { display:grid; grid-template-columns:repeat(auto-fit,minmax(150px,1fr)); gap:12px; margin-bottom:12px; }
  .body-card { display:flex; flex-direction:column; flex:1; min-height:0;
    background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px; padding:12px 14px; }
  cmx-view-tabs { display:flex; flex-direction:column; flex:1 1 auto; min-height:0; }
  .dm-tab-bar { display:flex; gap:4px; border-bottom:1px solid var(--sapGroup_ContentBorderColor,#d9d9d9); margin-bottom:10px; }
  .dm-tab { appearance:none; border:none; background:transparent; cursor:pointer; padding:8px 16px;
    font-size:13px; color:var(--sapContent_LabelColor,#6a6d70); border-bottom:2px solid transparent; }
  .dm-tab.active { color:var(--sapBrandColor,#0070f2); border-bottom-color:var(--sapBrandColor,#0070f2); font-weight:600; }
  .dm-panel { flex:1 1 auto; min-height:0; display:flex; flex-direction:column; }
  .fbar { display:flex; gap:8px; align-items:center; flex-wrap:wrap; margin-bottom:10px; }
  .fbar .f-ipt { min-width:120px; }
  .tbl-wrap { flex:1; min-height:120px; overflow:hidden; display:flex; flex-direction:column; }
  .tbl-wrap cmx-revo-grid { display:flex; width:100%; flex:1 1 0%; min-width:0; min-height:0; flex-direction:column; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:8px 10px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor);
    border-bottom:1px solid var(--sapList_BorderColor); background:var(--sapList_HeaderBackground,#f5f6f7); }
  .tbl td { padding:8px 10px; border-bottom:1px solid var(--sapList_BorderColor); vertical-align:middle; }
  .tbl tbody tr:hover td { background:var(--sapList_Hover_Background,#f5f5f5); }
  .muted { color:var(--sapContent_LabelColor); }
  .cell-view { cursor:pointer; color:var(--sapBrandColor,#0070f2); }
  .cell-view:hover { text-decoration:underline; }
  .scroll-y { flex:1; min-height:0; overflow:auto; }
  cmx-toolbar { display:block; }
  .dead-tools { display:flex; gap:8px; align-items:center; margin-bottom:8px; flex-wrap:wrap; }
  .sub-chip { display:inline-flex; align-items:center; gap:6px; padding:2px 10px; border-radius:10px;
    background:var(--sapInformationBackground,#eaf4ff); color:var(--sapInformativeTextColor,#0a6ed1); font-size:12px; }
  .sub-chip ui5-button { flex-shrink:0; }
  `
}

function viewHtml(st) {
  const deadN = st.stats ? Number(st.stats.dead) || 0 : 0
  return `<div class="pg">
    <div class="pg-head"><div><div class="pg-title">分发监控</div>
        <div class="pg-sub">投递流水 / 事件日志 / 死信处理 · KPI 每 30s、列表每 60s 自动刷新
          ${st.subId != null ? `<span class="sub-chip" id="dmSubChip">订阅过滤：#${esc(String(st.subId))}${st.subName ? ` ${esc(st.subName)}` : ''}<ui5-button id="dmSubClear" icon="decline" design="Transparent"></ui5-button></span>` : ''}</div></div>
      <cmx-toolbar><ui5-button design="Default" icon="restart" id="dmPublish">手动补发</ui5-button>
        <ui5-button design="Transparent" icon="refresh" slot="actions" id="dmRefresh">立即刷新</ui5-button></cmx-toolbar></div>
    <div class="alarm ${deadN > 0 ? 'on' : ''}" id="dmAlarm">⚠ 当前死信 <b id="dmAlarmN">${deadN}</b> 条：重试超限的投递进入死信，请到「死信处理」批量重发或跳过。</div>
    <div class="kpi-row">
      <cmx-kpi-card variant="card" id="dmKpiTotal" label="今日投递" value="${st.stats ? (Number(st.stats.today_total) || 0) : '…'}" tone="info"></cmx-kpi-card>
      <cmx-kpi-card variant="card" id="dmKpiRate" label="今日成功率" value="${rateText(st.stats)}" tone="success"></cmx-kpi-card>
      <cmx-kpi-card variant="card" id="dmKpiLatency" label="平均耗时" value="${st.stats ? String(Math.round(Number(st.stats.avg_latency_ms) || 0)) : '…'}" unit="ms" tone="neutral"></cmx-kpi-card>
      <cmx-kpi-card variant="card" id="dmKpiBacklog" label="积压" value="${st.stats ? (Number(st.stats.backlog) || 0) : '…'}" tone="warning"></cmx-kpi-card>
      <cmx-kpi-card variant="card" id="dmKpiDead" label="死信" value="${deadN || (st.stats ? 0 : '…')}" tone="${deadN > 0 ? 'danger' : 'neutral'}"></cmx-kpi-card>
      <cmx-kpi-card variant="card" id="dmKpiLag" label="扇出滞后" value="${st.stats ? (Number(st.stats.fanout_lag) || 0) : '…'}" tone="neutral"></cmx-kpi-card>
    </div>
    <div class="body-card">
      <cmx-view-tabs active="${esc(st.activeTab)}" id="dmTabs">
        <div slot="tabs" class="dm-tab-bar">
          <button class="dm-tab ${st.activeTab === 'dispatch' ? 'active' : ''}" data-view="dispatch">投递流水</button>
          <button class="dm-tab ${st.activeTab === 'events' ? 'active' : ''}" data-view="events">事件日志</button>
          <button class="dm-tab ${st.activeTab === 'dead' ? 'active' : ''}" data-view="dead">死信处理</button>
        </div>
        <div data-view-panel="dispatch" class="dm-panel">
          <div class="fbar">
            <ui5-input id="dmFSub" class="f-ipt" placeholder="订阅 id" value="${esc(st.dF.subscriptionId)}"></ui5-input>
            <ui5-select id="dmFStatus">
              <ui5-option value="" ${st.dF.status === '' ? 'selected' : ''}>全部状态</ui5-option>
              ${Object.entries(D_STATUS).map(([k, v]) => `<ui5-option value="${k}" ${st.dF.status === k ? 'selected' : ''}>${v.name}</ui5-option>`).join('')}
            </ui5-select>
            <ui5-date-picker id="dmFFrom" placeholder="从" format-pattern="yyyy-MM-dd" value="${esc(st.dF.timeFrom)}"></ui5-date-picker>
            <ui5-date-picker id="dmFTo" placeholder="到" format-pattern="yyyy-MM-dd" value="${esc(st.dF.timeTo)}"></ui5-date-picker>
            <ui5-button design="Default" icon="search" id="dmDSearch">查询</ui5-button>
            <ui5-button design="Transparent" icon="reset" id="dmDReset">重置</ui5-button>
            <span class="muted" id="dmDTotal" style="font-size:12px;margin-left:auto;">共 ${st.dTotal} 条</span>
          </div>
          <div class="tbl-wrap"><cmx-revo-grid id="dmGrid"></cmx-revo-grid></div>
          <cmx-pager id="dmPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
        </div>
        <div data-view-panel="events" class="dm-panel">
          <div class="fbar">
            <ui5-input id="dmEDict" class="f-ipt" placeholder="字典（如 supplier）" value="${esc(st.eDict)}"></ui5-input>
            <ui5-button design="Default" icon="search" id="dmESearch">查询</ui5-button>
            <span class="muted" id="dmETotal" style="font-size:12px;margin-left:auto;">共 ${st.eTotal} 条</span>
          </div>
          <div class="scroll-y">
            <table class="tbl"><thead><tr><th>seq</th><th>字典</th><th>事件类型</th><th>记录 id</th><th>发生时间</th><th>payload</th></tr></thead>
              <tbody id="dmEventBody"></tbody></table>
          </div>
          <cmx-pager id="dmEPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
          <div style="margin-top:10px;"><div style="font-size:13px;font-weight:600;color:var(--sapTitleColor);margin-bottom:4px;">pull 消费者游标</div>
            <div class="scroll-y" style="max-height:180px;">
              <table class="tbl"><thead><tr><th>消费者</th><th>字典</th><th>已确认 seq</th><th>滞后</th><th>确认时间</th></tr></thead>
                <tbody id="dmOffsetBody"></tbody></table>
            </div></div>
        </div>
        <div data-view-panel="dead" class="dm-panel">
          <div class="dead-tools">
            <ui5-checkbox id="dmDeadAll" text="全选本页"></ui5-checkbox>
            <ui5-button design="Emphasized" icon="restart" id="dmDeadRetry" disabled>批量重发</ui5-button>
            <ui5-button design="Default" icon="cancel" id="dmDeadSkip" disabled>批量跳过</ui5-button>
            <span class="muted" id="dmDeadInfo" style="font-size:12px;margin-left:auto;"></span>
          </div>
          <div class="scroll-y">
            <table class="tbl"><thead><tr><th></th><th>id</th><th>订阅</th><th>字典</th><th>记录 id</th><th>事件 seq</th><th>尝试</th><th>最近错误</th><th>创建时间</th></tr></thead>
              <tbody id="dmDeadBody"></tbody></table>
          </div>
          <cmx-pager id="dmDeadPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
        </div>
      </cmx-view-tabs>
    </div></div>`
}
function rateText(stats) {
  if (!stats) return '…'
  const t = Number(stats.today_total) || 0
  const ok = Number(stats.today_ok) || 0
  return t > 0 ? `${Math.round((ok / t) * 100)}%` : '-'
}

// ── 数据加载 ──────────────────────────────────────────────────────────────
async function loadStats(st) { st.stats = (await apiGet('/api/mdm/dispatches/stats', st.dbId)) || {} }
async function loadDispatch(st) {
  const f = st.dF; const body = { page: st.dPage, pageSize: st.dPageSize }
  const sub = (f.subscriptionId || '').trim()
  if (sub) { const n = Number(sub); if (Number.isFinite(n)) body.subscriptionId = n }
  if (f.status) body.status = f.status
  if (f.timeFrom) body.timeFrom = `${f.timeFrom}T00:00:00`
  if (f.timeTo) body.timeTo = `${f.timeTo}T23:59:59`
  const d = (await apiPost('/api/mdm/dispatches/query', body, st.dbId)) || {}
  st.dRows = d.list || []
  st.dTotal = Number(d.total) || 0
  st.dLoaded = true
}
async function loadEvents(st) {
  // order=desc：事件日志最新在前（后端缺省 seq ASC 是消费端 delta 契约，监控页显式倒序）
  const q = { page: String(st.ePage), pageSize: String(st.ePageSize), order: 'desc' }
  if (st.eDict.trim()) q.dictCode = st.eDict.trim()
  const d = (await apiGet(`/api/mdm/events?${new URLSearchParams(q)}`, st.dbId)) || {}
  st.eRows = d.list || []
  st.eTotal = Number(d.total) || 0
  st.eLoaded = true
}
async function loadOffsets(st) {
  try {
    const d = (await apiGet('/api/mdm/events/offsets', st.dbId)) || {}
    st.offsets = d.list || []
  } catch { st.offsets = [] }   // 游标表失败不阻塞事件日志
}
async function loadDead(st) {
  const d = (await apiPost('/api/mdm/dispatches/query', { status: 'dead', page: st.deadPage, pageSize: st.deadPageSize }, st.dbId)) || {}
  st.deadRows = d.list || []
  st.deadTotal = Number(d.total) || 0
  st.deadLoaded = true
  // 清掉不在本页的勾选
  const ids = new Set(st.deadRows.map((r) => String(r.id)))
  for (const k of Array.from(st.deadSel)) if (!ids.has(k)) st.deadSel.delete(k)
}

// ── 局部更新（不整页重绘）─────────────────────────────────────────────────
function rootOf(host) { return host && (host.renderRoot || host.shadowRoot) }

function applyStats(host, st) {
  const root = rootOf(host); if (!root || !st.stats) return
  const s = st.stats
  const set = (id, v, extra = {}) => {
    const el = root.querySelector(`#${id}`); if (!el) return
    el.setAttribute('value', String(v))
    for (const [k, val] of Object.entries(extra)) el.setAttribute(k, String(val))
  }
  set('dmKpiTotal', Number(s.today_total) || 0)
  set('dmKpiRate', rateText(s))
  set('dmKpiLatency', Math.round(Number(s.avg_latency_ms) || 0))
  set('dmKpiBacklog', Number(s.backlog) || 0)
  const dead = Number(s.dead) || 0
  set('dmKpiDead', dead, { tone: dead > 0 ? 'danger' : 'neutral' })
  set('dmKpiLag', Number(s.fanout_lag) || 0)
  const alarm = root.querySelector('#dmAlarm')
  if (alarm) {
    alarm.classList.toggle('on', dead > 0)
    const n = alarm.querySelector('#dmAlarmN'); if (n) n.textContent = String(dead)
  }
}

// 订阅显示文本：优先订阅名称（后端流水查询 LEFT JOIN md_subscription 连出 sub_name），
// 无名称/订阅已删时回退 #id；target_sys 一并带上便于区分同名订阅。
function subText(r) {
  const id = r.subscription_id ?? r.sub_id ?? ''
  const name = r.sub_name || ''
  const sys = r.sub_target_sys || r.target_sys || ''
  if (name && sys) return `${name}（${sys}）`
  if (name) return name
  return id !== '' ? `#${id}` : ''
}

function decorateDispatch(r) {
  return { ...r, status_text: statusName(r.status), sub_text: subText(r), created_at_text: fmtTime(r.created_at), last_error_text: trunc(r.last_error, 200) }
}
function applyDispatch(host, st) {
  const C = cmx(); const root = rootOf(host); if (!root) return
  const t = root.querySelector('#dmDTotal'); if (t) t.textContent = `共 ${st.dTotal} 条`
  const pager = root.querySelector('#dmPager')
  if (pager) { pager.total = st.dTotal; pager.page = st.dPage; pager.pageSize = st.dPageSize }
  const grid = st.dGrid; if (!grid) return
  const rows = st.dRows.map(decorateDispatch)
  if (C.CmxDataSet) { const ds = new C.CmxDataSet({ datasetId: 'dm-dispatch' }); ds.setRows(rows); grid.setDataSet(ds) }
  else grid.setDataSet?.(rows)
  grid.refreshLayout?.()
}

function applyEvents(host, st) {
  const root = rootOf(host); if (!root) return
  const body = root.querySelector('#dmEventBody')
  if (body) {
    body.innerHTML = st.eRows.length
      ? st.eRows.map((e, i) => {
          const pv = e.payload == null ? '' : (typeof e.payload === 'string' ? e.payload : JSON.stringify(e.payload))
          return `<tr><td>${esc(String(e.seq ?? ''))}</td><td>${esc(e.dict_code || '')}</td>
          <td>${esc(e.event_type || '')}</td><td class="muted">${esc(String(e.record_id ?? ''))}</td>
          <td class="muted">${esc(fmtTime(e.emitted_at))}</td>
          ${pv
            ? `<td class="cell-view" data-payload-idx="${i}" title="点击查看完整 payload">${esc(trunc(pv, 60))}</td>`
            : '<td class="muted">-</td>'}</tr>`
        }).join('')
      : '<tr><td colspan="6" class="muted" style="text-align:center;padding:24px;">暂无事件</td></tr>'
  }
  const info = root.querySelector('#dmETotal'); if (info) info.textContent = `共 ${st.eTotal} 条`
  const pager = root.querySelector('#dmEPager')
  if (pager) { pager.total = st.eTotal; pager.page = st.ePage; pager.pageSize = st.ePageSize }
  const ob = root.querySelector('#dmOffsetBody')
  if (ob) {
    ob.innerHTML = st.offsets.length
      ? st.offsets.map((o) => `<tr><td>${esc(o.consumer_id ?? '')}</td><td>${esc(o.dict_code ?? '')}</td>
          <td>${esc(String(o.acked_seq ?? ''))}</td>
          <td>${Number(o.lag) > 0 ? `<cmx-status-tag tone="${Number(o.lag) > 100 ? 'danger' : 'warning'}" variant="subtle" dot size="sm">${esc(String(o.lag))}</cmx-status-tag>` : '<span class="muted">0</span>'}</td>
          <td class="muted">${esc(fmtTime(o.acked_at))}</td></tr>`).join('')
      : '<tr><td colspan="5" class="muted" style="text-align:center;padding:16px;">暂无 pull 消费者</td></tr>'
  }
}

function applyDead(host, st) {
  const root = rootOf(host); if (!root) return
  const body = root.querySelector('#dmDeadBody')
  if (body) {
    body.innerHTML = st.deadRows.length
      ? st.deadRows.map((r, i) => `<tr data-dead="${esc(String(r.id))}">
          <td><ui5-checkbox class="dead-chk" data-id="${esc(String(r.id))}" ${st.deadSel.has(String(r.id)) ? 'checked' : ''}></ui5-checkbox></td>
          <td class="muted">${esc(String(r.id))}</td><td>${esc(subText(r))}</td>
          <td>${esc(r.dict_code || '')}</td><td class="muted">${esc(String(r.record_id ?? ''))}</td>
          <td>${esc(String(r.event_seq ?? ''))}</td><td>${esc(String(r.attempts ?? ''))}</td>
          ${r.last_error
            ? `<td class="cell-view" data-err-idx="${i}" title="点击查看完整错误">${esc(trunc(r.last_error, 60))}</td>`
            : '<td class="muted">-</td>'}
          <td class="muted">${esc(fmtTime(r.created_at))}</td></tr>`).join('')
      : '<tr><td colspan="9" class="muted" style="text-align:center;padding:24px;">暂无死信 🎉</td></tr>'
    body.querySelectorAll('.dead-chk').forEach((ck) => ck.addEventListener('change', () => {
      const id = ck.dataset.id
      if (ck.checked) st.deadSel.add(id); else st.deadSel.delete(id)
      updateDeadTools(root, st)
    }))
  }
  const all = root.querySelector('#dmDeadAll')
  if (all) {
    all.checked = st.deadRows.length > 0 && st.deadRows.every((r) => st.deadSel.has(String(r.id)))
    updateDeadTools(root, st)
  }
  const info = root.querySelector('#dmDeadInfo')
  if (info) info.textContent = `死信共 ${st.deadTotal} 条，已勾选 ${st.deadSel.size} 条`
  const pager = root.querySelector('#dmDeadPager')
  if (pager) { pager.total = st.deadTotal; pager.page = st.deadPage; pager.pageSize = st.deadPageSize }
}
function updateDeadTools(root, st) {
  const has = st.deadSel.size > 0
  const b1 = root.querySelector('#dmDeadRetry'); if (b1) b1.disabled = !has
  const b2 = root.querySelector('#dmDeadSkip'); if (b2) b2.disabled = !has
  const info = root.querySelector('#dmDeadInfo')
  if (info) info.textContent = `死信共 ${st.deadTotal} 条，已勾选 ${st.deadSel.size} 条`
}

// ── 各 Tab 刷新入口（轮询 & 手动共用）─────────────────────────────────────
async function refreshStats(host) { const st = getState(host); if (!st) return; await loadStats(st); applyStats(host, st) }
async function refreshActiveTab(host) {
  const st = getState(host); if (!st) return
  if (st.activeTab === 'dispatch') { await loadDispatch(st); applyDispatch(host, st) }
  else if (st.activeTab === 'events') { await Promise.all([loadEvents(st), loadOffsets(st)]); applyEvents(host, st) }
  else if (st.activeTab === 'dead') { await loadDead(st); applyDead(host, st) }
}

// ── 详情弹框（/mdm/dispatches/detail 全量 JSON）──────────────────────────
async function openDetail(st, id) {
  const M = cmx()
  if (!customElements.get('cmx-floating-dialog')) { M.cmxError?.('弹框组件未就绪'); return }
  let row = null
  try { row = await apiGet(`/api/mdm/dispatches/detail?id=${encodeURIComponent(id)}`, st.dbId) }
  catch (e) { M.cmxError?.(`加载详情失败：${e.message}`); return }
  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title: `投递详情 #${id}`, icon: 'detail-view',
    showConfirm: false, cancelText: '关闭', dialogWidth: '680px', dialogHeight: '78vh',
  })
  const wrap = document.createElement('div')
  // 同 openTextDialog：flex 链自适应高度，避免写死 max-height 被外壳裁底
  wrap.style.cssText = 'flex:1;min-height:0;display:flex;flex-direction:column;gap:8px;'   // padding 由 .dlg-content 默认提供；flex:1 填满契约（pre 内部滚动）
  const head = row && row.sub_name
    ? `<div style="font-size:13px;color:var(--sapContent_LabelColor);flex-shrink:0;">订阅 ${esc(String(row.subscription_id ?? ''))} · ${esc(row.sub_name || '')}${row.target_sys ? ` · ${esc(row.target_sys)}` : ''}${row.event_type ? ` · ${esc(row.event_type)}` : ''}</div>`
    : ''
  wrap.innerHTML = `${head}<pre style="margin:0;padding:12px;border-radius:6px;background:var(--sapList_HeaderBackground,#f5f6f7);color:var(--sapTextColor);font:12px/1.55 ui-monospace,Consolas,monospace;white-space:pre-wrap;word-break:break-all;overflow:auto;flex:1 1 auto;min-height:0;">${esc(JSON.stringify(row ?? {}, null, 2))}</pre>`
  dlg.setContent(wrap)
  document.body.appendChild(dlg)
  dlg.openModal().then(() => dlg.remove())
}

// ── 长文本探视弹框（事件 payload / 死信最近错误等）────────────────────────
// payload / last_error 列只截断展示，悬浮 title 看不全且无法复制——点击单元格
// 打开本弹框：payload 自动格式化为缩进 JSON，右上「复制」一键拷贝全文。
function prettyJson(v) {
  if (v == null) return ''
  if (typeof v !== 'string') {
    try { return JSON.stringify(v, null, 2) } catch { return String(v) }
  }
  try { return JSON.stringify(JSON.parse(v), null, 2) } catch { return v }
}

function openTextDialog(title, text, headHtml) {
  const M = cmx()
  if (!customElements.get('cmx-floating-dialog')) { M.cmxError?.('弹框组件未就绪'); return }
  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title, icon: 'detail-view',
    showConfirm: false, cancelText: '关闭', dialogWidth: '640px', dialogHeight: '72vh',
  })
  const wrap = document.createElement('div')
  // pre 不写死 max-height：写死会在大屏下超出 dlg-body 可视高被 overflow:hidden 裁底，
  // 改用 flex 链（wrap 列布局 + pre flex:1/min-height:0）自适应填满，滚动可见完整。
  wrap.style.cssText = 'flex:1;min-height:0;display:flex;flex-direction:column;gap:8px;'   // padding 由 .dlg-content 默认提供；flex:1 填满契约（pre 内部滚动）
  wrap.innerHTML = `
    <div style="display:flex;align-items:center;gap:8px;flex-shrink:0;">
      <div style="flex:1;min-width:0;font-size:13px;color:var(--sapContent_LabelColor);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${headHtml || ''}</div>
      <ui5-button icon="copy" design="Transparent" id="otCopy">复制</ui5-button>
    </div>
    <pre style="margin:0;padding:12px;border-radius:6px;background:var(--sapList_HeaderBackground,#f5f6f7);color:var(--sapTextColor);font:12px/1.55 ui-monospace,Consolas,monospace;white-space:pre-wrap;word-break:break-all;overflow:auto;flex:1 1 auto;min-height:0;">${esc(text)}</pre>`
  wrap.querySelector('#otCopy')?.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(text)
      showToast('已复制到剪贴板')
    } catch {
      // clipboard API 不可用时的兜底（隐藏 textarea + execCommand）
      const ta = document.createElement('textarea')
      ta.value = text
      ta.style.cssText = 'position:fixed;left:-9999px;top:0;'
      document.body.appendChild(ta); ta.select()
      try { document.execCommand('copy'); showToast('已复制到剪贴板') } catch { M.cmxError?.('复制失败，请手动选中文本复制') }
      ta.remove()
    }
  })
  dlg.setContent(wrap)
  document.body.appendChild(dlg)
  dlg.openModal().then(() => dlg.remove())
}
// ── 手动补发对话框（POST /mdm/publish）───────────────────────────────────
function openPublishDialog(st) {
  const M = cmx()
  if (!customElements.get('cmx-floating-dialog')) { M.cmxError?.('弹框组件未就绪'); return }
  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title: '手动补发', icon: 'restart', confirmText: '补发', cancelText: '取消', dialogWidth: '440px',
    beforeClose: async (ctx) => {
      if (ctx.action !== 'confirm') return true
      const body = {}
      const subId = (wrap.querySelector('#pbSubId')?.value || '').trim()
      if (subId) body.subscriptionId = Number(subId)
      const dict = (wrap.querySelector('#pbDict')?.value || '').trim()
      if (dict) body.dictCode = dict
      const from = (wrap.querySelector('#pbFrom')?.value || '').trim()
      if (from !== '') body.fromSeq = Number(from)
      const to = (wrap.querySelector('#pbTo')?.value || '').trim()
      if (to !== '') body.toSeq = Number(to)
      body.force = !!wrap.querySelector('#pbForce')?.checked
      if (body.subscriptionId != null && !Number.isFinite(body.subscriptionId)) { M.cmxWarn?.('订阅 id 须为数字'); return false }
      if ((body.fromSeq != null && !Number.isFinite(body.fromSeq)) || (body.toSeq != null && !Number.isFinite(body.toSeq))) { M.cmxWarn?.('seq 范围须为数字'); return false }
      if (body.subscriptionId == null && !body.dictCode) { M.cmxWarn?.('请填写订阅 id 或字典（二选一）'); return false }
      try {
        const d = (await apiPost('/api/mdm/publish', body, st.dbId)) || {}
        const n = Number(d.created) || 0
        showToast(n > 0 ? `补发完成：已创建 ${n} 条待投递实例` : '没有匹配的事件需要补发（已投递且未勾选 force 的会跳过）')
        refreshActiveTab(currentHostOf(st)).catch(() => {})
        return true
      } catch (e) { M.cmxError?.(`补发失败：${e.message}`); return false }
    },
  })
  const wrap = document.createElement('div')
  wrap.style.cssText = 'display:flex;flex-direction:column;gap:10px;font-size:13px;'   // padding 由 .dlg-content 默认提供
  wrap.innerHTML = `
    <div class="hint">按订阅/字典 + 事件 seq 范围重建待投递实例（上限 5000 行）。不勾 force 时已送达的不重发。</div>
    <div style="display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">订阅 id</label>
      <ui5-input id="pbSubId" placeholder="数字 id" value="${esc(st.subId != null ? String(st.subId) : '')}"></ui5-input></div>
    <div style="display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">字典</label>
      <ui5-input id="pbDict" placeholder="如 supplier（与订阅 id 至少填一项）"></ui5-input></div>
    <div style="display:flex;gap:10px;">
      <div style="flex:1;display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">起始 seq</label>
        <ui5-input id="pbFrom" placeholder="可空"></ui5-input></div>
      <div style="flex:1;display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">截止 seq</label>
        <ui5-input id="pbTo" placeholder="可空"></ui5-input></div>
    </div>
    <ui5-checkbox id="pbForce" text="强制重发已送达（force）"></ui5-checkbox>`
  dlg.setContent(wrap)
  document.body.appendChild(dlg)
  dlg.openModal().then(() => dlg.remove())
}

// host 反查（补发成功后局部刷新当前 tab）
const _stHost = new WeakMap()
function currentHostOf(st) { return _stHost.get(st) || null }

// ── Tab1 grid（列模型 + 详情操作，bind 一次）─────────────────────────────
function buildDispatchGrid(host, st) {
  const C = cmx()
  const root = rootOf(host)
  const wrap = root && root.querySelector('[data-view-panel="dispatch"] .tbl-wrap'); if (!wrap) return
  const grid = wrap.querySelector('cmx-revo-grid')
  if (!grid) return
  grid.setAttribute('data-cmx-fill-height', '')
  grid.setAttribute('data-cmx-options', '{"editable":false,"showTotals":false,"showRequiredMark":false}')
  grid.classList.add('cmx-grid-neo')
  st.dGrid = grid
  if (!(C.CmxColumnModel && C.CmxColumn)) return
  const cm = new C.CmxColumnModel({ datasetId: 'dm-dispatch' })
  cm.setMembers([
    new C.CmxColumn({ id: 'created_at_text', caption: '时间', dataType: 'VARCHAR', width: '150px' }),
    new C.CmxColumn({ id: 'sub_text', caption: '订阅', dataType: 'VARCHAR', width: '150px' }),
    new C.CmxColumn({ id: 'event_seq', caption: '事件seq', dataType: 'VARCHAR', width: '90px' }),
    new C.CmxColumn({ id: 'dict_code', caption: '字典', dataType: 'VARCHAR', width: '110px' }),
    new C.CmxColumn({ id: 'record_id', caption: '记录id', dataType: 'VARCHAR', width: '90px' }),
    new C.CmxColumn({ id: 'status_text', caption: '状态', dataType: 'VARCHAR', width: '90px' }),
    new C.CmxColumn({ id: 'attempts', caption: '尝试', dataType: 'VARCHAR', width: '70px' }),
    new C.CmxColumn({ id: 'http_status', caption: 'HTTP', dataType: 'VARCHAR', width: '80px' }),
    new C.CmxColumn({ id: 'last_error_text', caption: '最近错误', dataType: 'VARCHAR', width: '240px' }),
    new C.CmxColumn({ id: '_action', caption: '操作', dataType: 'VARCHAR', width: '100px', frozen: 'right', edit: { mode: 'readonly' },
      display: { mode: 'actions', actions: [{ text: '详情', actionRef: 'detail', icon: 'detail-view' }] } }),
  ])
  grid.setColumnModel(cm)
  grid.setOptions?.({ selectionMode: 'none', fillHeight: true, showRowIndex: true, showTotals: false, allowTextSelect: true, resize: true })
  grid.addEventListener('cmx-cell-link-click', (e) => {
    const d = e.detail || {}; const ds = grid._ds
    const row = (ds && ds.rows && !isNaN(parseInt(d.rowId, 10))) ? ds.rows[parseInt(d.rowId, 10)] : null
    const rec = row ? (row.toPlainObject ? row.toPlainObject() : row) : null
    if (!rec || rec.id == null || d.actionRef !== 'detail') return
    openDetail(getState(host), rec.id)
  })
}

// ── 死信批量操作 ─────────────────────────────────────────────────────────
async function deadBatch(host, st, kind) {
  const M = cmx()
  const ids = Array.from(st.deadSel).map(Number).filter(Number.isFinite)
  if (!ids.length) { M.cmxWarn?.('请先勾选死信'); return }
  const ok = await M.cmxConfirm?.({
    title: kind === 'retry' ? '批量重发' : '批量跳过', danger: kind !== 'retry',
    message: kind === 'retry'
      ? `确认重发勾选的 ${ids.length} 条死信？将重置为待投递并按订阅策略重试。`
      : `确认跳过勾选的 ${ids.length} 条死信？跳过后不再投递（决策留痕）。`,
    confirmText: kind === 'retry' ? '重发' : '跳过',
  })
  if (ok === false) return
  try {
    if (kind === 'retry') {
      const d = (await apiPost('/api/mdm/dispatches/retry', { ids }, st.dbId)) || {}
      showToast(`已重发 ${Number(d.retried) || 0} 条`)
    } else {
      const d = (await apiPost('/api/mdm/dispatches/skip', { ids }, st.dbId)) || {}
      showToast(`已跳过 ${Number(d.skipped) || 0} 条`)
    }
    st.deadSel.clear()
    await Promise.all([loadDead(st), loadStats(st)])
    applyDead(host, st); applyStats(host, st)
  } catch (e) { M.cmxError?.(`操作失败：${e.message}`) }
}

// ── 事件绑定 ──────────────────────────────────────────────────────────────
function bind(host, st, root) {
  root.querySelector('#dmRefresh')?.addEventListener('click', () => {
    refreshStats(host).catch(() => {})
    refreshActiveTab(host).catch(() => {})
  })
  root.querySelector('#dmPublish')?.addEventListener('click', () => openPublishDialog(st))
  root.querySelector('#dmSubClear')?.addEventListener('click', () => {
    st.subId = null; st.subName = ''
    st.dF.subscriptionId = ''; st.dPage = 1
    const chip = root.querySelector('#dmSubChip'); if (chip) chip.remove()
    const inp = root.querySelector('#dmFSub'); if (inp) inp.value = ''
    loadDispatch(st).then(() => applyDispatch(host, st)).catch(() => {})
  })

  // Tab 切换：记录当前 tab；未加载过的 tab 懒加载
  root.querySelector('#dmTabs')?.addEventListener('cmx-view-change', (e) => {
    const v = (e.detail && e.detail.view) || 'dispatch'
    st.activeTab = v
    root.querySelectorAll('.dm-tab').forEach((b) => b.classList.toggle('active', b.dataset.view === v))
    refreshActiveTab(host).catch(() => {})
  })

  // Tab1 过滤
  const doSearch = () => {
    st.dF.subscriptionId = (root.querySelector('#dmFSub') || {}).value || ''
    st.dF.status = (root.querySelector('#dmFStatus') || {}).value || ''
    st.dF.timeFrom = (root.querySelector('#dmFFrom') || {}).value || ''
    st.dF.timeTo = (root.querySelector('#dmFTo') || {}).value || ''
    st.dPage = 1
    loadDispatch(st).then(() => applyDispatch(host, st)).catch(() => {})
  }
  root.querySelector('#dmDSearch')?.addEventListener('click', doSearch)
  root.querySelector('#dmDReset')?.addEventListener('click', () => {
    st.dF = { subscriptionId: st.subId != null ? String(st.subId) : '', status: '', timeFrom: '', timeTo: '' }
    st.dPage = 1
    const i1 = root.querySelector('#dmFSub'); if (i1) i1.value = st.dF.subscriptionId
    const s1 = root.querySelector('#dmFStatus'); if (s1) s1.value = ''
    const d1 = root.querySelector('#dmFFrom'); if (d1) d1.value = ''
    const d2 = root.querySelector('#dmFTo'); if (d2) d2.value = ''
    loadDispatch(st).then(() => applyDispatch(host, st)).catch(() => {})
  })
  root.querySelector('#dmFSub')?.addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch() })
  const pager = root.querySelector('#dmPager')
  if (pager) {
    pager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== st.dPageSize) { st.dPageSize = d.pageSize; st.dPage = 1 }
      else st.dPage = d.page || 1
      loadDispatch(st).then(() => applyDispatch(host, st)).catch(() => {})
    })
  }

  // Tab2 事件日志
  const eSearch = () => {
    st.eDict = (root.querySelector('#dmEDict') || {}).value || ''
    st.ePage = 1
    Promise.all([loadEvents(st), loadOffsets(st)]).then(() => applyEvents(host, st)).catch(() => {})
  }
  root.querySelector('#dmESearch')?.addEventListener('click', eSearch)
  root.querySelector('#dmEDict')?.addEventListener('keydown', (e) => { if (e.key === 'Enter') eSearch() })
  const ePager = root.querySelector('#dmEPager')
  if (ePager) {
    ePager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== st.ePageSize) { st.ePageSize = d.pageSize; st.ePage = 1 }
      else st.ePage = d.page || 1
      loadEvents(st).then(() => applyEvents(host, st)).catch(() => {})
    })
  }
  // payload 单元格点击 → 弹框查看完整 JSON（委托绑 tbody，innerHTML 重建不影响）
  root.querySelector('#dmEventBody')?.addEventListener('click', (e) => {
    const td = e.target && e.target.closest ? e.target.closest('td[data-payload-idx]') : null
    if (!td) return
    const row = st.eRows[Number(td.dataset.payloadIdx)]
    if (!row) return
    openTextDialog(
      `事件 payload · seq ${esc(String(row.seq ?? ''))}`,
      prettyJson(row.payload),
      `字典 ${esc(row.dict_code || '')} · ${esc(row.event_type || '')} · 记录 ${esc(String(row.record_id ?? ''))} · ${esc(fmtTime(row.emitted_at))}`,
    )
  })

  // Tab3 死信处理
  root.querySelector('#dmDeadAll')?.addEventListener('change', (e) => {
    const on = !!e.target.checked
    st.deadRows.forEach((r) => { if (on) st.deadSel.add(String(r.id)); else st.deadSel.delete(String(r.id)) })
    root.querySelectorAll('.dead-chk').forEach((ck) => { ck.checked = on })
    updateDeadTools(root, st)
  })
  root.querySelector('#dmDeadRetry')?.addEventListener('click', () => deadBatch(host, st, 'retry'))
  root.querySelector('#dmDeadSkip')?.addEventListener('click', () => deadBatch(host, st, 'skip'))
  const deadPager = root.querySelector('#dmDeadPager')
  if (deadPager) {
    deadPager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== st.deadPageSize) { st.deadPageSize = d.pageSize; st.deadPage = 1 }
      else st.deadPage = d.page || 1
      loadDead(st).then(() => applyDead(host, st)).catch(() => {})
    })
  }
  // 最近错误单元格点击 → 弹框查看完整错误
  root.querySelector('#dmDeadBody')?.addEventListener('click', (e) => {
    const td = e.target && e.target.closest ? e.target.closest('td[data-err-idx]') : null
    if (!td) return
    const row = st.deadRows[Number(td.dataset.errIdx)]
    if (!row) return
    openTextDialog(
      `死信错误 · 投递 #${esc(String(row.id ?? ''))}`,
      String(row.last_error ?? ''),
      `订阅 ${esc(subText(row))} · ${esc(row.dict_code || '')} · 记录 ${esc(String(row.record_id ?? ''))} · 事件 seq ${esc(String(row.event_seq ?? ''))} · 已尝试 ${esc(String(row.attempts ?? ''))} 次`,
    )
  })

  buildDispatchGrid(host, st)
  // 首帧双 rAF 等 grid 布局就绪再填（其后轮询/翻页直接填）
  requestAnimationFrame(() => requestAnimationFrame(() => applyDispatch(host, st)))
  applyEvents(host, st)
  applyDead(host, st)
}

function whenRendered(host, sel, cb, t) {
  const n = t == null ? 60 : t
  const root = host && (host.renderRoot || host.shadowRoot)
  if (root && root.querySelector(sel)) { cb(root); return }
  if (n <= 0) return
  requestAnimationFrame(() => whenRendered(host, sel, cb, n - 1))
}

export default {
  defaultView: 'content',
  views: {
    async content(ctx) {
      const host = ctx && ctx.host
      const st = getState(host)
      _stHost.set(st, host)
      const p = (ctx && ctx.props) || {}
      const wctx = host && host.workspace && host.workspace.context
      const get = (k) => { try { return wctx && typeof wctx.get === 'function' ? wctx.get(k) : undefined } catch { return undefined } }
      st.dbId = p.dbId || p.db_id || get('dbId') || get('db_id') || ''
      // 订阅管理页跳入预填（subscriptionId 过滤流水）
      const subId = get('subscriptionId') ?? p.subscriptionId ?? null
      if (subId != null && subId !== '') {
        st.subId = Number(subId)
        st.dF.subscriptionId = String(subId)
        st.subName = get('subscriptionName') || p.subscriptionName || ''
      }
      try {
        await Promise.all([loadStats(st), loadDispatch(st)])
      } catch (e) { console.error('[dispatch-monitor] init fail', e); cmx().cmxError?.(`初始化失败：${e.message}`) }
      if (host) {
        ensurePolling(host)
        whenRendered(host, '.pg', (r) => bind(host, st, r))
      }
      return `<style>${styleCss()}</style>${viewHtml(st)}`
    },
  },
}
