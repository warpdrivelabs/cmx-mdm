/**
 * MDM 分发订阅管理（native-page）。
 *
 * 列表 + 编辑弹框双形态：
 *  - 列表：cmx-filter-bar（目标系统/字典/通道/状态）+ cmx-revo-grid + cmx-pager，
 *    行操作 编辑 / 测试 / 启停（visible 按 active）/ 查看投递 / 补发 / 删除（仅停用态）。
 *  - 编辑：cmx-floating-dialog 大尺寸，手写分区表单（不用 cmx-ui5-form）：
 *    ① 基本信息（名称/目标系统/字典 cmx-combo-box/描述/启用）
 *    ② 事件与过滤（created/updated/merged 复选 + .rule-row 行编辑过滤条件，字段名带字典字段候选）
 *    ③ 通道配置（webhook：URL/秘钥[随机生成]/超时；rest_pull：consumerId；其余未启用）
 *    ④ 投递与字段映射（最大重试/批量大小 + field_map 简化三输入）
 *    底部自绘 [取消][保存并测试][保存]。
 *  - 补发：小对话框 fromSeq/toSeq/force → POST /api/mdm/publish。
 *
 * 端点（全部 /api 前缀，行字段 snake_case，前端展示层适配）：
 *   GET  /mdm/subscriptions            过滤 + 分页（近 24h 统计列 + secret 掩码 ***）
 *   POST /mdm/subscriptions            upsert（secret 回传 *** = 未变更）
 *   POST /mdm/subscriptions/{delete,set-active,test}
 *   GET  /mdm/subscriptions/channels   通道枚举
 *   POST /mdm/publish                  手动补发（重建 pending 投递实例）
 *   GET  /mdm/activations              字典下拉数据源（target_dict 去重）
 *
 * 多实例安全：state 按 host 隔离（WeakMap）；局部更新（applyData 不整页重绘）。
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
  // 后端错误响应有两种字段名：ApiResp 用 msg，cmx_api_types::Error 用 error；两者都兼容。
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

// 轻量 toast（成功/失败轻反馈，3s 自动消失）——对齐 activation-mapper 提示范式。
// 校验警告用 cmxWarn、异常用 cmxError（需用户停下查看）。
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

// ── 按 host 隔离的 state（多实例安全）──────────────────────────────────────
const _hostState = new WeakMap()
function initState() {
  return {
    coord: null, dbId: '',
    rows: [], total: 0, page: 1, pageSize: 20,
    fTarget: '', fDict: '', fChannel: '', fActive: '',
    channels: [],           // [{type,label}]
    dicts: [],              // 去重后的 target_dict 字典码
    grid: null,
  }
}
function getState(host) { if (host && !_hostState.has(host)) _hostState.set(host, initState()); return host ? _hostState.get(host) : null }

// 坐标四元组（module 回退 mdm，dbId 兼读 workspace.context）——照 master-list 版本。
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
function coordCtx(st) {
  const c = st.coord || {}
  if (!c.domain && !c.application) return {}
  return { domain: c.domain, application: c.application, module: c.module || 'mdm', dbId: c.dbId }
}

// 打开并列门户标签页（照 master-list 模式；找不到 openNode 时仅告警不报错）。
function openTab(host, st, caption, nativePage, context, opts = {}) {
  let app = null
  try { app = document.querySelector('cmx-portal-app') } catch { app = null }
  if (!app || typeof app.openNode !== 'function') {
    let n = host
    for (let i = 0; i < 6 && n; i++) {
      if (typeof n.openNode === 'function') { app = n; break }
      const r = n.getRootNode && n.getRootNode(); n = r && r.host
    }
  }
  if (!app || typeof app.openNode !== 'function') { console.warn('[subscription-manager] 未找到 portal-app.openNode'); return }
  const ctxKey = (context && context.subscriptionId) || ''
  const key = opts.single ? 'single' : (ctxKey || Date.now())
  const c = st.coord || {}
  app.openNode({
    id: `${nativePage}-${key}`, name: nativePage, caption, type: 'workspace-node',
    domainCode: c.domain || '', applicationCode: c.application || '',
    workspace: { content: { caption, views: [{ type: 'native_pages', native_page: nativePage, view: 'content' }] } },
  }, { initialContext: context })
}

function styleCss() {
  return `
  .pg { height:100%; overflow:hidden; display:flex; flex-direction:column; box-sizing:border-box; padding:12px 20px 16px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .pg-head { margin-bottom:10px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor); margin-top:2px; }
  .card { display:flex; flex-direction:column; flex:1; min-height:0;
    background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px; padding:12px 14px; }
  .card-hd { display:flex; justify-content:space-between; align-items:center; gap:8px; margin-bottom:10px; }
  .card-title { font-size:15px; font-weight:600; color:var(--sapTitleColor); }
  .tbl-wrap { flex:1; min-height:0; overflow:hidden; display:flex; flex-direction:column; margin-top:10px; }
  .tbl-wrap cmx-revo-grid { display:flex; width:100%; flex:1 1 0%; min-width:0; min-height:0; flex-direction:column; }
  cmx-toolbar, cmx-filter-bar { display:block; }
  .f-ipt { min-width:130px; }
  `
}

function fmtTime(t) { if (!t) return ''; const s = String(t); return s.length > 19 ? s.slice(0, 19).replace('T', ' ') : s }

function viewHtml(st) {
  const chOpts = ['<ui5-option value="">全部通道</ui5-option>']
    .concat(st.channels.map((c) => `<ui5-option value="${esc(c.type)}" ${st.fChannel === c.type ? 'selected' : ''}>${esc(c.label || c.type)}</ui5-option>`))
    .join('')
  return `<div class="pg">
    <div class="pg-head"><div class="pg-title">分发订阅管理</div>
      <div class="pg-sub">主数据变更事件按订阅分发到目标系统：webhook 推送 / rest_pull 拉取；支持过滤、字段映射与补投</div></div>
    <div class="card">
      <div class="card-hd"><div class="card-title" id="smTotal">订阅列表（共 ${st.total} 条）</div>
        <cmx-toolbar><ui5-button design="Emphasized" icon="add" id="smAdd">新建订阅</ui5-button>
          <ui5-button design="Transparent" icon="refresh" slot="actions" id="smReload">刷新</ui5-button></cmx-toolbar></div>
      <cmx-filter-bar id="smFilter" show-search="false">
        <ui5-input id="smFTarget" class="f-ipt" placeholder="目标系统（如 wms）" value="${esc(st.fTarget)}"></ui5-input>
        <ui5-input id="smFDict" class="f-ipt" placeholder="字典（如 supplier）" value="${esc(st.fDict)}"></ui5-input>
        <ui5-select id="smFChannel">${chOpts}</ui5-select>
        <ui5-select id="smFActive">
          <ui5-option value="" ${st.fActive === '' ? 'selected' : ''}>全部状态</ui5-option>
          <ui5-option value="true" ${st.fActive === 'true' ? 'selected' : ''}>启用</ui5-option>
          <ui5-option value="false" ${st.fActive === 'false' ? 'selected' : ''}>停用</ui5-option>
        </ui5-select>
        <ui5-button slot="actions" design="Default" icon="search" id="smSearch">查询</ui5-button>
        <ui5-button slot="actions" design="Transparent" icon="reset" id="smReset">重置</ui5-button>
      </cmx-filter-bar>
      <div class="tbl-wrap"><cmx-revo-grid id="smGrid"></cmx-revo-grid></div>
      <cmx-pager id="smPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
    </div></div>`
}

// ── 列表加载（GET /mdm/subscriptions，行字段 snake_case）──────────────────
async function loadRows(st) {
  const q = { page: String(st.page), pageSize: String(st.pageSize) }
  if (st.fTarget) q.targetSys = st.fTarget.trim()
  if (st.fDict) q.dictCode = st.fDict.trim()
  if (st.fChannel) q.channel = st.fChannel
  if (st.fActive !== '') q.active = st.fActive
  const d = (await apiGet(`/api/mdm/subscriptions?${new URLSearchParams(q)}`, st.dbId)) || {}
  st.rows = d.list || []
  st.total = Number(d.total) || 0
}

// 派生展示列（成功率 / 状态徽章文本 / 事件类型文本）——不改动后端行结构。
// event_types 容错：jsonb 正常回数组；若经中转变成字符串则再解析一次。
function parseEvts(v) {
  if (Array.isArray(v)) return v
  if (typeof v === 'string' && v.trim()) { try { const p = JSON.parse(v); return Array.isArray(p) ? p : [] } catch { return [] } }
  return []
}
// jsonb 对象容错（filter/field_map/channel_config）：字符串则再解析，异常回退空对象。
function parseObj(v) {
  if (v && typeof v === 'object') return v
  if (typeof v === 'string' && v.trim()) { try { const p = JSON.parse(v); return (p && typeof p === 'object') ? p : {} } catch { return {} } }
  return {}
}
function decorate(r) {
  const total = Number(r.stat_total_24h) || 0
  const ok = Number(r.stat_ok_24h) || 0
  const evts = parseEvts(r.event_types)
  return {
    ...r,
    success_text: total > 0 ? `${Math.round((ok / total) * 100)}%（${ok}/${total}）` : '-',
    active_text: r.active ? '● 启用' : '○ 停用',
    event_types_text: evts.length ? evts.join(' / ') : '全部',
    backlog_text: String(r.stat_backlog ?? 0),
  }
}

// 列表 grid：仅建列模型与事件（bind 时一次）；数据填充由 applyData 局部更新。
function buildListGrid(host, st) {
  const C = cmx()
  const root = host && (host.renderRoot || host.shadowRoot)
  const wrap = root && root.querySelector('.tbl-wrap'); if (!wrap) return
  const grid = wrap.querySelector('cmx-revo-grid')
  if (!grid) return
  grid.setAttribute('data-cmx-fill-height', '')
  grid.setAttribute('data-cmx-options', '{"editable":false,"showTotals":false,"showRequiredMark":false}')
  grid.classList.add('cmx-grid-neo')
  st.grid = grid
  if (!(C.CmxColumnModel && C.CmxColumn)) return
  const cm = new C.CmxColumnModel({ datasetId: 'sm-list' })
  cm.setMembers([
    new C.CmxColumn({ id: 'name', caption: '名称', dataType: 'VARCHAR', width: '170px' }),
    new C.CmxColumn({ id: 'target_sys', caption: '目标系统', dataType: 'VARCHAR', width: '110px' }),
    new C.CmxColumn({ id: 'dict_code', caption: '字典', dataType: 'VARCHAR', width: '110px' }),
    new C.CmxColumn({ id: 'channel', caption: '通道', dataType: 'VARCHAR', width: '100px' }),
    new C.CmxColumn({ id: 'event_types_text', caption: '事件类型', dataType: 'VARCHAR', width: '140px' }),
    new C.CmxColumn({ id: 'active_text', caption: '状态', dataType: 'VARCHAR', width: '90px' }),
    new C.CmxColumn({ id: 'success_text', caption: '近24h成功率', dataType: 'VARCHAR', width: '130px' }),
    new C.CmxColumn({ id: 'backlog_text', caption: '积压', dataType: 'VARCHAR', width: '70px' }),
    new C.CmxColumn({ id: '_action', caption: '操作', dataType: 'VARCHAR', width: '360px', frozen: 'right', edit: { mode: 'readonly' },
      display: { mode: 'actions', actions: [
        { text: '编辑', actionRef: 'edit', icon: 'edit' },
        { text: '测试', actionRef: 'test', icon: 'paper-plane' },
        { text: '启用', actionRef: 'enable', icon: 'play', visible: (m) => !m.active },
        { text: '停用', actionRef: 'disable', icon: 'pause', visible: (m) => !!m.active },
        { text: '投递', actionRef: 'dispatch', icon: 'detail-view' },
        { text: '补发', actionRef: 'republish', icon: 'restart' },
        { text: '删除', actionRef: 'delete', icon: 'delete', variant: 'negative', visible: (m) => !m.active },
      ] } }),
  ])
  grid.setColumnModel(cm)
  grid.setOptions?.({ selectionMode: 'none', fillHeight: true, showRowIndex: true, showTotals: false, allowTextSelect: true, resize: true })
  grid.addEventListener('cmx-cell-link-click', (e) => {
    const d = e.detail || {}; const ds = grid._ds
    const row = (ds && ds.rows && !isNaN(parseInt(d.rowId, 10))) ? ds.rows[parseInt(d.rowId, 10)] : null
    const rec = row ? (row.toPlainObject ? row.toPlainObject() : row) : null
    if (!rec || rec.id == null) return
    doAction(host, st, d.actionRef, rec)
  })
}

// 数据落地（局部更新）：只动 total 文案、grid 数据、pager——DOM/事件/焦点/滚动全保留。
function applyData(host, st, first = false) {
  const C = cmx()
  const root = host && (host.renderRoot || host.shadowRoot); if (!root) return
  const t = root.querySelector('#smTotal')
  if (t) t.textContent = `订阅列表（共 ${st.total} 条）`
  const pager = root.querySelector('#smPager')
  if (pager) { pager.total = st.total; pager.page = st.page; pager.pageSize = st.pageSize }
  const grid = st.grid
  if (!grid) return
  const rows = st.rows.map(decorate)
  const fill = () => {
    if (C.CmxDataSet) { const ds = new C.CmxDataSet({ datasetId: 'sm-list' }); ds.setRows(rows); grid.setDataSet(ds) }
    else grid.setDataSet?.(rows)
    grid.refreshLayout?.()
  }
  if (first) requestAnimationFrame(() => requestAnimationFrame(fill))
  else fill()
}

async function reload(host, st) { await loadRows(st); applyData(host, st) }

// ── 行操作 ────────────────────────────────────────────────────────────────
async function doAction(host, st, act, row) {
  const M = cmx()
  const id = Number(row.id)
  const label = row.name || row.target_sys || `#${id}`
  try {
    if (act === 'edit') { openEditDialog(host, st, st.rows.find((r) => Number(r.id) === id) || row) }
    else if (act === 'test') {
      const t = (await apiPost('/api/mdm/subscriptions/test', { id }, st.dbId)) || {}
      if (t.ok) showToast(`测试通过（${t.latencyMs ?? '-'} ms）`)
      else M.cmxError?.(`测试失败：${t.detail || '未知原因'}`)
    }
    else if (act === 'enable' || act === 'disable') {
      const to = act === 'enable'
      const msg = to
        ? `确认启用订阅「${label}」？启用后新事件将实时分发。`
        : `确认停用订阅「${label}」？当前积压 ${row.stat_backlog ?? 0} 条，停用期间不再产生新投递（存量投递不再重试）。`
      const ok = await M.cmxConfirm?.({ title: to ? '启用订阅' : '停用订阅', message: msg, danger: !to })
      if (ok === false) return
      await apiPost('/api/mdm/subscriptions/set-active', { id, active: to }, st.dbId)
      showToast(to ? `订阅「${label}」已启用` : `订阅「${label}」已停用`)
      await reload(host, st)
    }
    else if (act === 'dispatch') {
      openTab(host, st, `分发监控·${label}`, 'portal.mdm.dispatch-monitor',
        { subscriptionId: id, subscriptionName: label, ...coordCtx(st) })
    }
    else if (act === 'republish') { openPublishDialog(st, { subscriptionId: id, dictCode: row.dict_code, title: label }) }
    else if (act === 'delete') {
      const ok = await M.cmxConfirm?.({
        title: '删除订阅', danger: true,
        message: `确认删除订阅「${label}」？删除后其投递流水将保留审计，不可恢复。`,
      })
      if (ok === false) return
      await apiPost('/api/mdm/subscriptions/delete', { id }, st.dbId)
      showToast(`订阅「${label}」已删除（投递流水已保留）`)
      await reload(host, st)
    }
  } catch (e) { M.cmxError?.(`操作失败：${e.message}`) }
}

// ── 补发小对话框（POST /mdm/publish 重建 pending 投递实例）────────────────
function openPublishDialog(st, preset) {
  const M = cmx()
  if (!customElements.get('cmx-floating-dialog')) { M.cmxError?.('弹框组件未就绪'); return }
  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title: `手动补发${preset && preset.title ? `·${preset.title}` : ''}`, icon: 'restart',
    confirmText: '补发', cancelText: '取消', dialogWidth: '440px',
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
        return true
      } catch (e) { M.cmxError?.(`补发失败：${e.message}`); return false }
    },
  })
  const wrap = document.createElement('div')
  wrap.style.cssText = 'display:flex;flex-direction:column;gap:10px;font-size:13px;'   // padding 由 .dlg-content 默认提供
  wrap.innerHTML = `
    <div class="hint">按订阅/字典 + 事件 seq 范围重建待投递实例（上限 5000 行）。不勾 force 时已送达的不重发。</div>
    <div style="display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">订阅 id</label>
      <ui5-input id="pbSubId" placeholder="数字 id（可从列表行查看）" value="${esc(preset && preset.subscriptionId ? String(preset.subscriptionId) : '')}"></ui5-input></div>
    <div style="display:flex;flex-direction:column;gap:4px;"><label style="font-size:12px;color:var(--sapContent_LabelColor);">字典</label>
      <ui5-input id="pbDict" placeholder="如 supplier（与订阅 id 至少填一项）" value="${esc(preset && preset.dictCode ? String(preset.dictCode) : '')}"></ui5-input></div>
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

// ── 编辑对话框（cmx-floating-dialog + 手写分区表单）───────────────────────
const EVT_TYPES = [
  { k: 'created', label: 'created 新增' },
  { k: 'updated', label: 'updated 变更' },
  { k: 'merged', label: 'merged 合并' },
]
const OPS = [['eq', '等于'], ['ne', '不等于'], ['in', '属于(逗号分隔)'], ['like', '模糊']]

// 32 位随机 hex（签名秘钥）
function randomSecret() {
  const buf = new Uint8Array(16)
  ;(globalThis.crypto || {}).getRandomValues ? crypto.getRandomValues(buf) : buf.forEach((_, i) => { buf[i] = Math.floor(Math.random() * 256) })
  return Array.from(buf, (b) => b.toString(16).padStart(2, '0')).join('')
}

function condRowHtml(c) {
  return `<div class="rule-row">
    <ui5-input class="sc-field" placeholder="字段名（可输关键字）" show-suggestions value="${esc(c.field)}" style="min-width:150px;flex:1 1 150px;"></ui5-input>
    <ui5-select class="sc-op" style="min-width:130px;">
      ${OPS.map(([v, t]) => `<ui5-option value="${v}" ${(c.op || 'eq') === v ? 'selected' : ''}>${t}</ui5-option>`).join('')}
    </ui5-select>
    <ui5-input class="sc-val" placeholder="值" value="${esc(c.value)}" style="min-width:120px;flex:1 1 120px;"></ui5-input>
    <ui5-button icon="add" class="sc-add" design="Transparent" title="加一行"></ui5-button>
    <ui5-button icon="delete" class="sc-del" design="Transparent" title="删本行"></ui5-button>
  </div>`
}

function openEditDialog(host, st, sub) {
  const C = cmx(); const M = C
  if (!customElements.get('cmx-floating-dialog')) { M.cmxError?.('弹框组件未就绪'); return }
  const isNew = !sub
  const cfg = parseObj(sub && sub.channel_config)
  const fmp = parseObj(sub && sub.field_map)
  const fConds = parseObj(sub && sub.filter).conditions
  const fm = {
    id: sub ? Number(sub.id) : null,
    name: (sub && sub.name) || '',
    target_sys: (sub && sub.target_sys) || '',
    dict_code: (sub && sub.dict_code) || '',
    description: (sub && sub.description) || '',
    channel: (sub && sub.channel) || 'webhook',
    active: sub ? !!sub.active : true,
    eventTypes: (parseEvts(sub && sub.event_types).length ? parseEvts(sub.event_types) : ['created', 'updated', 'merged']),
    conditions: (Array.isArray(fConds) ? fConds : []).map((c) => ({
      field: c.field || '', op: c.op || 'eq', value: c.value == null ? '' : String(c.value),
    })),
    url: cfg.url || '',
    secret: cfg.secret || '',
    timeoutMs: (sub && sub.timeout_ms) != null ? sub.timeout_ms : (cfg.timeout_ms != null ? cfg.timeout_ms : 10000),
    consumerId: cfg.consumerId || cfg.consumer_id || '',
    retryMax: (sub && sub.retry_max) != null ? sub.retry_max : 8,
    batchSize: (sub && sub.batch_size) != null ? sub.batch_size : 50,
    fmInclude: Array.isArray(fmp.include) ? fmp.include.join(',') : '',
    fmRename: fmp.rename && typeof fmp.rename === 'object' ? Object.entries(fmp.rename).map(([k, v]) => `${k}:${v}`).join(',') : '',
    fmMask: Array.isArray(fmp.mask) ? fmp.mask.join(',') : '',
  }
  let metaFields = []   // [{name,caption}] 来自 /api/dct/meta（无坐标时空 → 纯手输）

  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title: isNew ? '新建订阅' : `编辑订阅·${fm.name || fm.target_sys}`, icon: 'settings',
    dialogWidth: '760px', dialogHeight: '82vh',
    showConfirm: false, showCancel: false,   // 底部按钮自绘（取消/保存并测试/保存）
  })
  // 布局契约：#dlg-body 已 position:relative（absolute 只能锚内容区，盖不到标题栏）；
  // setContent 自动包 .dlg-content 标准容器。本弹框用非对称 padding + 内部 sm-scroll 滚动，
  // 故 padding:false 关默认 padding，wrap 自带 padding 并写 flex:1;min-height:0 填满契约。
  const wrap = document.createElement('div')
  wrap.style.cssText = 'flex:1;min-height:0;padding:6px 18px 14px;display:flex;flex-direction:column;'
  wrap.innerHTML = `<style>
    .sm-dlg { display:flex; flex-direction:column; flex:1 1 auto; min-height:0; font-size:13px; }
    .sm-scroll { flex:1; min-height:0; overflow-y:auto; display:flex; flex-direction:column; gap:10px;
      padding:2px 6px 8px 0; }
    .sm-dlg label { font-size:12px; color:var(--sapContent_LabelColor); }
    .sec-title { font-size:13px; font-weight:600; color:var(--sapTitleColor); margin-top:8px; padding-bottom:4px;
      border-bottom:1px solid var(--sapList_BorderColor,#e5e5e5); }
    .grid2 { display:grid; grid-template-columns:1fr 1fr; gap:10px 14px; }
    .grid3 { display:grid; grid-template-columns:repeat(3,minmax(0,1fr)); gap:10px 14px; }
    .f { display:flex; flex-direction:column; gap:4px; min-width:0; }
    .chk-row { display:flex; flex-wrap:wrap; gap:8px 22px; }
    .hint { font-size:12px; color:var(--sapContent_LabelColor); }
    .cond-box { display:flex; flex-direction:column; gap:6px; }
    .rule-row { display:flex; gap:6px; align-items:center; padding:5px 8px; border-radius:4px;
      background:var(--sapList_Background); border:1px solid var(--sapGroup_ContentBorderColor,#e9e9e9); }
    .dlg-foot { display:flex; justify-content:flex-end; gap:8px; padding:10px 6px 4px 0; flex-shrink:0;
      border-top:1px solid var(--sapList_BorderColor,#e5e5e5); }
    cmx-combo-box { display:block; }
  </style>
  <div class="sm-dlg">
   <div class="sm-scroll">
    <div class="sec-title">① 基本信息</div>
    <div class="grid2">
      <div class="f"><label>名称 *</label><ui5-input id="smName" placeholder="如：WMS 供应商同步" value="${esc(fm.name)}"></ui5-input></div>
      <div class="f"><label>目标系统 *（英文标识）</label><ui5-input id="smTarget" placeholder="如 wms" value="${esc(fm.target_sys)}"></ui5-input></div>
      <div class="f"><label>字典 *（来自激活映射）</label><cmx-combo-box id="smDict"></cmx-combo-box></div>
      <div class="f"><label>描述</label><ui5-input id="smDesc" placeholder="用途说明（可空）" value="${esc(fm.description)}"></ui5-input></div>
    </div>
    <ui5-checkbox id="smActive" text="启用（停用后不再产生新投递）" ${fm.active ? 'checked' : ''}></ui5-checkbox>

    <div class="sec-title">② 事件与过滤</div>
    <div class="chk-row">
      ${EVT_TYPES.map((e) => `<ui5-checkbox data-evt="${e.k}" text="${e.label}" ${fm.eventTypes.includes(e.k) ? 'checked' : ''}></ui5-checkbox>`).join('')}
      <span class="hint">全不选 = 订阅全部事件类型</span>
    </div>
    <div class="cond-box">
      <div class="hint">过滤条件（字段取值为记录快照字段；多条件之间为 AND；in 的值用逗号分隔）</div>
      <div id="smCondRows">${(fm.conditions.length ? fm.conditions : [{}]).map(condRowHtml).join('')}</div>
      <div><ui5-button icon="add" id="smCondAdd" design="Transparent">加条件</ui5-button></div>
    </div>

    <div class="sec-title">③ 通道配置</div>
    <div class="f" style="max-width:280px;"><label>通道</label>
      <ui5-select id="smChannel">${st.channels.map((c) => `<ui5-option value="${esc(c.type)}" ${fm.channel === c.type ? 'selected' : ''}>${esc(c.label || c.type)}</ui5-option>`).join('')}</ui5-select>
    </div>
    <div id="smChannelBox"></div>

    <div class="sec-title">④ 投递与字段映射</div>
    <div class="grid3">
      <div class="f"><label>最大重试次数</label><ui5-input id="smRetry" value="${esc(String(fm.retryMax))}"></ui5-input></div>
      <div class="f"><label>批量大小</label><ui5-input id="smBatch" value="${esc(String(fm.batchSize))}"></ui5-input></div>
      <div class="hint" style="align-self:end;">重试超限进入死信，可在「分发监控」批量处理</div>
    </div>
    <div class="grid3">
      <div class="f"><label>仅投递字段（include）</label><ui5-input id="smFmInclude" placeholder="逗号分隔，留空=全部字段" value="${esc(fm.fmInclude)}"></ui5-input></div>
      <div class="f"><label>字段改名（rename）</label><ui5-input id="smFmRename" placeholder="old:new，逗号分隔多对" value="${esc(fm.fmRename)}"></ui5-input></div>
      <div class="f"><label>字段脱敏（mask）</label><ui5-input id="smFmMask" placeholder="逗号分隔字段名" value="${esc(fm.fmMask)}"></ui5-input></div>
    </div>

   </div><!-- /sm-scroll -->
    <div class="dlg-foot">
      <ui5-button id="smCancel">取消</ui5-button>
      <ui5-button id="smSaveTest">保存并测试</ui5-button>
      <ui5-button design="Emphasized" id="smSave">保存</ui5-button>
    </div>
  </div>`
  dlg.setContent(wrap, { padding: false })   // 非对称 padding（6px 18px 14px）自管，关默认
  document.body.appendChild(dlg)

  // —— 字典下拉（cmx-combo-box list 模式，选项 = 激活映射 target_dict 去重）——
  const combo = wrap.querySelector('#smDict')
  if (combo && C.CmxDataSet) {
    combo.setMode('list')
    combo.setPlaceholder('选择字典（可输入过滤）')
    const ds = new C.CmxDataSet({ datasetId: 'sm-dicts' })
    ds.setRows(st.dicts.map((d) => ({ id: d, name: d })))
    combo.setDataSet(ds)
    if (fm.dict_code) { try { combo.setValue(fm.dict_code, { silent: true }) } catch { /* 列表缺项时静默 */ } }
    combo.addEventListener('cmx-combo-value-change', (e) => {
      fm.dict_code = (e.detail && e.detail.id) || ''
      loadMetaFields(fm.dict_code)
    })
  }

  // —— 过滤字段候选（/api/dct/meta with_props；无坐标/失败时静默降级为纯手输）——
  async function loadMetaFields(dictCode) {
    metaFields = []
    if (!dictCode || !st.coord || !(st.coord.domain && st.coord.application)) return
    try {
      const m = await apiGet(`/api/dct/meta?${coordQs(st, { dict: dictCode })}&with_props=true`, st.dbId)
      metaFields = ((m && m.columns) || []).map((c) => ({ name: c.name, caption: (c.caption && (c.caption.zh_CN || c.caption)) || c.name }))
    } catch { metaFields = [] }
    refreshSuggestions()
  }
  function refreshSuggestions() {
    wrap.querySelectorAll('.sc-field').forEach((input) => {
      Array.from(input.children).forEach((c) => { if (c.tagName && c.tagName.toLowerCase() === 'ui5-suggestion-item') c.remove() })
      const q = String(input.value || '').toLowerCase()
      for (const f of metaFields) {
        if (q && !f.name.toLowerCase().includes(q) && !String(f.caption).toLowerCase().includes(q)) continue
        const o = document.createElement('ui5-suggestion-item')
        o.setAttribute('text', f.name === f.caption ? f.name : `${f.name} · ${f.caption}`)
        o.dataset.field = f.name
        input.appendChild(o)
      }
    })
  }
  function bindCondRow(row) {
    const field = row.querySelector('.sc-field')
    if (field) {
      field.addEventListener('input', () => refreshSuggestions())
      field.addEventListener('suggestion-item-select', (ev) => {
        const it = ev.detail && ev.detail.item
        const v = (it && (it.dataset.field || it.getAttribute('data-field'))) || ''
        if (v) field.value = v
      })
    }
    row.querySelector('.sc-add')?.addEventListener('click', () => {
      const nr = document.createElement('div'); nr.innerHTML = condRowHtml({ field: '', op: 'eq', value: '' })
      const el = nr.firstElementChild; row.after(el); bindCondRow(el)
    })
    row.querySelector('.sc-del')?.addEventListener('click', () => {
      const box = wrap.querySelector('#smCondRows')
      if (box && box.children.length > 1) row.remove()
      else { row.querySelectorAll('input,select').forEach((el) => { el.value = '' }) }
    })
  }
  wrap.querySelectorAll('#smCondRows .rule-row').forEach(bindCondRow)
  wrap.querySelector('#smCondAdd')?.addEventListener('click', () => {
    const box = wrap.querySelector('#smCondRows'); if (!box) return
    const nr = document.createElement('div'); nr.innerHTML = condRowHtml({ field: '', op: 'eq', value: '' })
    const el = nr.firstElementChild; box.appendChild(el); bindCondRow(el); el.querySelector('.sc-field')?.focus?.()
  })
  if (fm.dict_code) loadMetaFields(fm.dict_code)

  // —— 通道配置区（webhook / rest_pull / 未启用）——
  function renderChannelBox() {
    const box = wrap.querySelector('#smChannelBox'); if (!box) return
    if (fm.channel === 'webhook') {
      box.innerHTML = `<div class="grid3">
        <div class="f"><label>URL *</label><ui5-input id="smUrl" placeholder="https://wms.example.com/api/cmx" value="${esc(fm.url)}"></ui5-input></div>
        <div class="f"><label>签名秘钥 *</label>
          <div style="display:flex;gap:6px;align-items:center;">
            <ui5-input id="smSecret" style="flex:1;" value="${esc(fm.secret)}" ${isNew ? '' : 'placeholder="*** 表示未变更"'}></ui5-input>
            <ui5-button icon="initialize" id="smGenSecret" design="Transparent" title="随机生成 32 位 hex">随机生成</ui5-button>
          </div>
          <span class="hint">编辑时显示 *** 表示沿用库内原秘钥；重填则覆盖</span></div>
        <div class="f"><label>超时（ms）</label><ui5-input id="smTimeout" value="${esc(String(fm.timeoutMs))}"></ui5-input></div>
      </div>`
      box.querySelector('#smGenSecret')?.addEventListener('click', () => {
        const s = box.querySelector('#smSecret')
        if (s) s.value = randomSecret()
      })
    } else if (fm.channel === 'rest_pull') {
      box.innerHTML = `<div class="grid2">
        <div class="f"><label>消费者标识（consumerId）</label><ui5-input id="smConsumer" placeholder="如 wms-consumer" value="${esc(fm.consumerId)}"></ui5-input></div>
        <div class="hint" style="align-self:end;">rest_pull 仅登记拉取消费者（游标见「分发监控」），不主动投递</div>
      </div>`
    } else {
      box.innerHTML = `<div class="hint">通道 ${esc(fm.channel)} 未启用，仅登记配置，不会产生投递。</div>`
    }
  }
  renderChannelBox()
  wrap.querySelector('#smChannel')?.addEventListener('change', (e) => {
    fm.channel = e.target.value || 'webhook'
    renderChannelBox()
  })

  // —— 收集 + 校验 + 组装 body（字段 snake_case 对齐后端）——
  function collect() {
    const val = (sel) => ((wrap.querySelector(sel) || {}).value || '').trim()
    const name = val('#smName')
    const target = val('#smTarget')
    const dict = (combo && combo.getValue && combo.getValue()) || fm.dict_code
    if (!name) return { err: '请填写名称' }
    if (!target) return { err: '请填写目标系统' }
    if (!dict) return { err: '请选择字典' }
    const channel = (wrap.querySelector('#smChannel') || {}).value || fm.channel || 'webhook'
    let cc = {}
    let timeoutMs = fm.timeoutMs
    if (channel === 'webhook') {
      const url = val('#smUrl'); const secret = val('#smSecret'); const tStr = val('#smTimeout')
      if (!url) return { err: 'webhook 通道需填写 URL' }
      if (!secret) return { err: 'webhook 通道需填写签名秘钥' }
      timeoutMs = Number(tStr) || 10000
      cc = { url, secret, timeout_ms: timeoutMs }
    } else if (channel === 'rest_pull') {
      cc = { consumerId: val('#smConsumer') }
    }
    const evts = []
    wrap.querySelectorAll('[data-evt]').forEach((ck) => { if (ck.checked) evts.push(ck.dataset.evt) })
    const conds = []
    wrap.querySelectorAll('#smCondRows .rule-row').forEach((row) => {
      const f = ((row.querySelector('.sc-field') || {}).value || '').trim()
      const op = (row.querySelector('.sc-op') || {}).value || 'eq'
      const v = ((row.querySelector('.sc-val') || {}).value || '').trim()
      if (f && v !== '') conds.push({ field: f, op, value: v })
    })
    // field_map：三输入 → {include,rename,mask}（有值才放键）
    const splitList = (s) => s.split(/[,，]/).map((x) => x.trim()).filter(Boolean)
    const include = splitList(val('#smFmInclude'))
    const mask = splitList(val('#smFmMask'))
    const rename = {}
    splitList(val('#smFmRename')).forEach((pair) => {
      const i = pair.indexOf(':')
      if (i > 0) rename[pair.slice(0, i).trim()] = pair.slice(i + 1).trim()
    })
    const fmk = {}
    if (include.length) fmk.include = include
    if (Object.keys(rename).length) fmk.rename = rename
    if (mask.length) fmk.mask = mask
    return {
      body: {
        id: fm.id || undefined,
        name, target_sys: target, dict_code: dict,
        channel, active: !!(wrap.querySelector('#smActive') || {}).checked,
        description: val('#smDesc'),
        event_types: evts,
        filter: conds.length ? { conditions: conds, logic: 'and' } : null,
        field_map: Object.keys(fmk).length ? fmk : null,
        channel_config: cc,
        retry_max: Number(val('#smRetry')) || 8,
        timeout_ms: Number(timeoutMs) || 10000,
        batch_size: Number(val('#smBatch')) || 50,
      },
    }
  }

  let saving = false
  async function doSave(withTest) {
    if (saving) return
    const r = collect()
    if (r.err) { M.cmxWarn?.(r.err); return }
    saving = true
    try {
      const saved = (await apiPost('/api/mdm/subscriptions', r.body, st.dbId)) || {}
      const newId = Number(saved.id) || fm.id
      if (isNew) showToast(`订阅已创建（#${newId}）。新订阅自新事件起分发，需补投历史请用「补发」`)
      else showToast('订阅已保存')
      if (withTest) {
        try {
          const t = (await apiPost('/api/mdm/subscriptions/test', { id: newId }, st.dbId)) || {}
          if (t.ok) showToast(`测试通过（${t.latencyMs ?? '-'} ms）`)
          else M.cmxError?.(`测试失败：${t.detail || '未知原因'}`)
        } catch (e) { M.cmxError?.(`测试失败：${e.message}`) }
      }
      dlg.close('confirm', { force: true })
      await reload(host, st)
    } catch (e) {
      M.cmxError?.(`保存失败：${e.message}`)
    } finally { saving = false }
  }

  wrap.querySelector('#smCancel')?.addEventListener('click', () => dlg.close('cancel'))
  wrap.querySelector('#smSave')?.addEventListener('click', () => doSave(false))
  wrap.querySelector('#smSaveTest')?.addEventListener('click', () => doSave(true))

  dlg.openModal().then(() => dlg.remove())
}

// ── 事件绑定与视图生命周期 ────────────────────────────────────────────────
function bind(host, st, root) {
  const reload2 = () => reload(host, st)
  root.querySelector('#smAdd')?.addEventListener('click', () => openEditDialog(host, st, null))
  root.querySelector('#smReload')?.addEventListener('click', reload2)
  const doSearch = () => {
    st.fTarget = (root.querySelector('#smFTarget') || {}).value || ''
    st.fDict = (root.querySelector('#smFDict') || {}).value || ''
    st.fChannel = (root.querySelector('#smFChannel') || {}).value || ''
    st.fActive = (root.querySelector('#smFActive') || {}).value || ''
    st.page = 1
    reload2()
  }
  root.querySelector('#smSearch')?.addEventListener('click', doSearch)
  root.querySelector('#smReset')?.addEventListener('click', () => {
    st.fTarget = ''; st.fDict = ''; st.fChannel = ''; st.fActive = ''; st.page = 1
    const i1 = root.querySelector('#smFTarget'); if (i1) i1.value = ''
    const i2 = root.querySelector('#smFDict'); if (i2) i2.value = ''
    const s1 = root.querySelector('#smFChannel'); if (s1) s1.value = ''
    const s2 = root.querySelector('#smFActive'); if (s2) s2.value = ''
    reload2()
  })
  ;['#smFTarget', '#smFDict'].forEach((sel) => {
    root.querySelector(sel)?.addEventListener('keydown', (e) => { if (e.key === 'Enter') doSearch() })
  })
  const pager = root.querySelector('#smPager')
  if (pager) {
    pager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== st.pageSize) { st.pageSize = d.pageSize; st.page = 1 }
      else st.page = d.page || 1
      reload2()
    })
  }
  buildListGrid(host, st)
  applyData(host, st, true)
}
function whenRendered(host, sel, cb, t) {
  const n = t == null ? 60 : t
  const root = host && (host.renderRoot || host.shadowRoot)
  if (root && root.querySelector(sel)) { cb(root); return }
  if (n <= 0) return
  requestAnimationFrame(() => whenRendered(host, sel, cb, n - 1))
}

// 预取下拉数据源：通道枚举 + 激活映射字典去重（失败均静默降级）。
async function loadLookups(st) {
  try {
    const d = (await apiGet('/api/mdm/subscriptions/channels', st.dbId)) || {}
    const list = (d && d.list) || []
    st.channels = list.map((c) => ({ type: c.type || c, label: c.label || c.type || c }))
  } catch { st.channels = [] }
  if (!st.channels.length) st.channels = [{ type: 'webhook', label: 'webhook' }, { type: 'rest_pull', label: 'rest_pull' }]
  try {
    const acts = (await apiGet('/api/mdm/activations', st.dbId)) || []
    const seen = []
    for (const a of acts) { const d = a.target_dict || a.targetDict; if (d && !seen.includes(d)) seen.push(d) }
    st.dicts = seen.sort()
  } catch { st.dicts = [] }
}

export default {
  defaultView: 'content',
  views: {
    async content(ctx) {
      const host = ctx && ctx.host
      const st = getState(host)
      st.coord = readCoord(ctx)
      st.dbId = st.coord.dbId || ((ctx && ctx.props && (ctx.props.dbId || ctx.props.db_id)) || '')
      try {
        await Promise.all([loadLookups(st), loadRows(st)])
      } catch (e) { console.error('[subscription-manager] init fail', e); cmx().cmxError?.(`初始化失败：${e.message}`) }
      if (host) whenRendered(host, '.pg', (r) => bind(host, st, r))
      return `<style>${styleCss()}</style>${viewHtml(st)}`
    },
  },
}
