/**
 * MDM 主数据·通用详情页（native-page，元数据驱动公共页）。
 *
 * 由通用列表页 master-list「查看详情」经 openNode 打开，经 workspace.context 读
 * { dictCode, recordId, title, icon, columns }（ctx.props 兜底）。自行调接口加载（刷新后自洽）：
 *   1) 头字段：GET /api/dct/meta?dict=…&with_props=true → 列（caption/enumValues），默认过滤平台列
 *      （或 context 透传的 columns）→ cmx-desc-list 渲染（enum 值→文本映射）。
 *   2) 头记录：POST /api/dct/data/search?dict=… + filters:{id}。
 *   3) 子表：GET /api/mdm/activations?targetDict=… → line_mappings（targetDict + parentIdField）自动发现；
 *      每个子表列取自其 dct/meta（默认过滤并**无条件剔除 parentIdField**），数据 filters:{[parentIdField]:recordId}，
 *      页面级表格渲染 + enum 映射。无激活/无子表时优雅降级为仅头字段。
 *
 * 多实例安全：state 按 host 隔离（WeakMap）。
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }。
 */

// 平台/审计/治理/scope/系统列默认隐藏集合（与 master-list 一致；context.columns 可覆盖）。
const PLATFORM_COLS = new Set([
  'id', 'sort_no',
  'create_by', 'create_time', 'update_by', 'update_time',
  'lifecycle_status', 'published_version', 'effective_date', 'effective_from', 'effective_to',
  'disabled_reason', 'disabled_time',
  'scope_type', 'entity_id', 'is_system',
  'level_no', 'full_path', 'is_leaf', 'parent_id', 'parent_code',
])

function unwrap(res, body) {
  if (body && typeof body === 'object' && typeof body.code === 'number') {
    if (body.code !== 0) { const e = new Error(body.msg || `业务错误 ${body.code}`); e.body = body; throw e }
    return body.data
  }
  if (!res.ok) { const e = new Error((body && body.error) || `HTTP ${res.status}`); e.status = res.status; throw e }
  return body
}
async function apiPost(url, payload, dbId) {
  const h = { 'Content-Type': 'application/json', Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { method: 'POST', headers: h, credentials: 'same-origin', body: JSON.stringify(payload || {}) })
  return unwrap(r, await r.json().catch(() => null))
}
async function apiGet(url, dbId) {
  const h = { Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { method: 'GET', headers: h, credentials: 'same-origin' })
  return unwrap(r, await r.json().catch(() => null))
}

const _hostState = new WeakMap()
function initState() {
  return { coord: null, dbId: '', dictCode: '', recordId: null, title: '', icon: '', columns: null,
    dictMeta: null, record: null, subs: [], loading: true, loadErr: '' }
}
function getState(host) { if (host && !_hostState.has(host)) _hostState.set(host, initState()); return host ? _hostState.get(host) : null }

function readCoord(ctx) {
  const p = (ctx && ctx.props) || {}
  const wctx = ctx && ctx.host && ctx.host.workspace && ctx.host.workspace.context
  const get = (k) => (wctx && typeof wctx.get === 'function' ? wctx.get(k) : undefined)
  return {
    domain: get('domain') || p.domain || p.domainCode || '',
    application: get('application') || p.application || p.applicationCode || '',
    module: get('module') || p.module || 'mdm',
    dbId: p.dbId || p.db_id || get('dbId') || get('db_id') || '',
  }
}
function coordQs(st, extra = {}) {
  const c = st.coord || {}
  return new URLSearchParams({ domain: c.domain || '', application: c.application || '', module: c.module || 'mdm', ...extra }).toString()
}

function styleCss() {
  return `
  .pg { height:100%; overflow:auto; box-sizing:border-box; padding:12px 16px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .card { background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px;
    padding:12px 14px; margin-bottom:12px; }
  .card-title { font-size:14px; font-weight:600; color:var(--sapTitleColor); margin-bottom:10px;
    display:flex; align-items:center; gap:6px; }
  .card-title ui5-icon { color:var(--neo-cyan,var(--sapInformativeTextColor,#00b4d8)); font-size:15px; }
  cmx-desc-list { display:block; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:9px 12px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor);
    border-bottom:1px solid var(--sapList_BorderColor); background:var(--sapList_Background); }
  .tbl td { padding:9px 12px; border-bottom:1px solid var(--sapList_BorderColor); }
  .tbl tbody tr:hover td { background:var(--sapList_Hover_Background); }
  .muted { color:var(--sapContent_LabelColor); }
  .loading { padding:40px; text-align:center; color:var(--sapContent_LabelColor); font-size:13px; }
  .load-err { padding:24px; color:var(--sapNegativeTextColor,#b00); font-size:13px; }
  `
}
function esc(s) { return String(s ?? '').replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])) }
function escAttr(s) { return String(s ?? '').replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c])) }

// 列 caption（{zh_CN} 或字符串）→ 文本。
function captionOf(col) {
  const cap = col && col.caption
  return (cap && (cap.zh_CN || cap)) || (col && col.name) || (col && col.id) || ''
}
// 列标识归一：dct/meta 原始列只有 name（无 id），转换列模型才有 id——统一 id||name，
// 否则 pickCols 匹配/过滤、取值 rec[col] 全部失配（列被清空 → 页面无数据）。
function cid(col) { return (col && (col.id || col.name)) || '' }
// enum 值→label 映射；非 enum 列原样返回。
function displayVal(col, val) {
  if (val == null || val === '') return null
  const evs = col && col.enumValues
  if (Array.isArray(evs)) {
    const hit = evs.find((e) => String(e.value) === String(val))
    if (hit) return hit.label || hit.caption || hit.text || String(hit.value)
  }
  return val
}
// 头/子表显示列：context.columns 为最终清单；否则默认过滤平台列 + visible!==false。
function pickCols(st, meta, extraHide) {
  const all = (meta && meta.columns) || []
  let cols
  if (Array.isArray(st.columns) && st.columns.length) {
    cols = st.columns.map((id) => all.find((c) => cid(c) === id)).filter(Boolean)
  } else {
    cols = all.filter((c) => !PLATFORM_COLS.has(cid(c)) && c.visible !== false)
  }
  if (extraHide) cols = cols.filter((c) => cid(c) !== extraHide)
  return cols
}

function viewHtml(st) {
  if (st.loading) return `<div class="pg"><div class="loading">正在加载详情…</div></div>`
  if (st.loadErr) return `<div class="pg"><div class="load-err">⚠ ${esc(st.loadErr)}</div></div>`
  const rec = st.record || {}
  const labelField = (st.dictMeta && st.dictMeta.labelField) || 'name'
  const headCols = pickCols(st, st.dictMeta)
  const kv = (l, v) => `<cmx-desc-item label="${escAttr(l)}">${v == null || v === '' ? '—' : esc(v)}</cmx-desc-item>`
  const headHtml = `<div class="card"><div class="card-title">${st.icon ? `<ui5-icon name="${escAttr(st.icon)}" mode="Decorative"></ui5-icon>` : ''}${esc(st.title || '')}·${esc(rec[labelField] || '')}</div>
    <cmx-desc-list columns="3" border>${headCols.map((c) => kv(captionOf(c), displayVal(c, rec[cid(c)]))).join('')}</cmx-desc-list></div>`
  const subsHtml = (st.subs || []).map((sub) => {
    const cols = sub.cols
    const rows = sub.rows || []
    const body = rows.length
      ? rows.map((r) => `<tr>${cols.map((c) => { const v = displayVal(c, r[cid(c)]); return `<td${v == null ? ' class="muted"' : ''}>${v == null ? '—' : esc(v)}</td>` }).join('')}</tr>`).join('')
      : `<tr><td colspan="${cols.length || 1}" class="muted">暂无数据</td></tr>`
    return `<div class="card"><div class="card-title">${esc(sub.title)}（${rows.length}）</div>
      <table class="tbl"><thead><tr>${cols.map((c) => `<th>${esc(captionOf(c))}</th>`).join('')}</tr></thead><tbody>${body}</tbody></table></div>`
  }).join('')
  return `<div class="pg">${headHtml}${subsHtml}</div>`
}

// ── 数据加载 ──────────────────────────────────────────────────────────────
async function loadDetail(host) {
  const st = getState(host); if (!st) return
  if (!st.dictCode) { st.loadErr = '缺少 dictCode'; return }
  if (st.recordId == null || st.recordId === '') { st.loadErr = '缺少记录 ID'; return }
  // 头元数据 + 头记录
  st.dictMeta = await apiGet(`/api/dct/meta?${coordQs(st, { dict: st.dictCode })}&with_props=true`, st.dbId)
  const main = (await apiPost(`/api/dct/data/search?${coordQs(st, { dict: st.dictCode })}`, {
    filters: { id: st.recordId }, pageSize: 1,
  }, st.dbId)) || {}
  st.record = (main.rows && main.rows[0]) || null
  if (!st.record) { st.loadErr = `${st.dictCode} ${st.recordId} 不存在`; return }
  // 子表自动发现：activations?targetDict → line_mappings（targetDict + parentIdField）
  st.subs = []
  const acts = (await apiGet(`/api/mdm/activations?targetDict=${encodeURIComponent(st.dictCode)}`, st.dbId).catch(() => null)) || []
  const withLines = acts.filter((a) => Array.isArray(a.line_mappings) && a.line_mappings.length)
  const act = withLines.find((a) => a.cr_type === 'create') || withLines[0]
  for (const lm of ((act && act.line_mappings) || [])) {
    const tdict = lm.targetDict || lm.target_dict
    const pf = lm.parentIdField || lm.parent_id_field
    if (!tdict || !pf) continue
    const meta = await apiGet(`/api/dct/meta?${coordQs(st, { dict: tdict })}&with_props=true`, st.dbId).catch(() => null)
    const data = (await apiPost(`/api/dct/data/search?${coordQs(st, { dict: tdict })}`, {
      filters: { [pf]: st.recordId }, pageSize: 100,
    }, st.dbId).catch(() => null)) || {}
    const sub = { dictCode: tdict, parentField: pf, title: (meta && meta.dictName) || lm.lineType || tdict, meta, rows: data.rows || [] }
    sub.cols = pickCols(st, meta, pf) // 默认过滤 + 无条件剔除外键列
    st.subs.push(sub)
  }
}

function refresh(host) {
  if (!host) return
  const st = getState(host); if (!st) return
  const root = host.renderRoot || host.shadowRoot; if (!root) return
  root.innerHTML = `<style>${styleCss()}</style>${viewHtml(st)}`
}
async function init(host) {
  const st = getState(host); if (!st) return
  try { await loadDetail(host) } catch (e) { st.loadErr = `加载失败：${e.message}`; console.error('[master-detail] load fail', e) }
  st.loading = false
  refresh(host)
}

export default {
  defaultView: 'content',
  views: {
    async content(ctx) {
      const host = ctx && ctx.host
      const st = getState(host)
      st.coord = readCoord(ctx)
      st.dbId = st.coord.dbId || ''
      const wctx = host && host.workspace && host.workspace.context
      const get = (k) => { try { return wctx && wctx.get ? wctx.get(k) : undefined } catch { return undefined } }
      const p = (ctx && ctx.props) || {}
      st.dictCode = get('dictCode') || p.dictCode || ''
      st.recordId = get('recordId') ?? p.recordId ?? null
      st.title = get('title') || p.title || ''
      st.icon = get('icon') || p.icon || ''
      const cols = get('columns') ?? p.columns
      st.columns = Array.isArray(cols) ? cols : null
      st.record = null; st.subs = []; st.loading = true; st.loadErr = ''
      init(host)
      return `<style>${styleCss()}</style>${viewHtml(st)}`
    },
  },
}
