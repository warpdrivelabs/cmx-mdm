/**
 * MDM 健康检查 / 模块总览（native-page · 企业级）。
 *
 * 作用：验证 MDM 后端各端点可用性 + 展示主数据治理概览（主数据数/变更申请/合并请求）。
 * 调用真实端点：/api/mdm/health、/api/mdm/activations、/api/mdm/change-requests、/api/mdm/merge-requests。
 *
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }。
 */

const cmx = () => (typeof globalThis !== 'undefined' && globalThis.__cmxDataComp) || {}

function unwrap(res, body) {
  if (body && typeof body === 'object' && typeof body.code === 'number') {
    if (body.code !== 0) { const e = new Error(body.msg || `业务错误 ${body.code}`); e.body = body; throw e }
    return body.data
  }
  if (!res.ok) { const e = new Error((body && body.error) || `HTTP ${res.status}`); e.status = res.status; throw e }
  return body
}
async function apiGet(url, dbId) {
  const h = { Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { headers: h, credentials: 'same-origin' })
  return unwrap(r, await r.json().catch(() => null))
}

const state = { dbId: '', checks: [], stats: null }

function styleCss() {
  return `
  .pg { height:100%; overflow:auto; box-sizing:border-box; padding:16px 20px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .pg-head { display:flex; justify-content:space-between; align-items:flex-start; margin-bottom:14px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor); margin-top:2px; }
  .kpi-row { display:grid; grid-template-columns:repeat(auto-fit,minmax(160px,1fr)); gap:12px; margin-bottom:14px; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:10px 12px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor); border-bottom:1px solid var(--sapList_BorderColor); }
  .tbl td { padding:10px 12px; border-bottom:1px solid var(--sapList_BorderColor); }
  .muted { color:var(--sapContent_LabelColor); }
  cmx-panel, cmx-toolbar { display:block; }
  .pg-body { display:flex; flex-direction:column; gap:14px; }
  `
}

async function runChecks() {
  const checks = []
  const t = (name, fn) => fn().then((d) => ({ name, ok: true, detail: d }))
    .catch((e) => ({ name, ok: false, detail: e.message }))
  // 轻量检查：pageSize=1 只取 total，避免全量拉取（数据量大时性能安全）
  checks.push(await t('模块健康 /api/mdm/health', async () => { const d = await apiGet('/api/mdm/health', state.dbId); return `status=${d.status}` }))
  checks.push(await t('激活映射 /api/mdm/activations', async () => { const d = await apiGet('/api/mdm/activations', state.dbId); return `${(d || []).length} 条映射` }))
  checks.push(await t('变更申请 /api/mdm/change-requests', async () => { const d = await apiGet('/api/mdm/change-requests?page=1&pageSize=1', state.dbId); return `共 ${d?.total ?? 0} 条` }))
  checks.push(await t('合并请求 /api/mdm/merge-requests', async () => { const d = await apiGet('/api/mdm/merge-requests?page=1&pageSize=1', state.dbId); return `共 ${d?.total ?? 0} 条` }))
  checks.push(await t('变更历史 /api/mdm/audit', async () => { const d = await apiGet('/api/mdm/audit?page=1&pageSize=1', state.dbId); return `共 ${d?.total ?? 0} 条` }))
  checks.push(await t('事件 /api/mdm/events', async () => { const d = await apiGet('/api/mdm/events?page=1&pageSize=1', state.dbId); return `共 ${d?.total ?? 0} 条` }))
  checks.push(await t('订阅 /api/mdm/subscriptions', async () => { const d = await apiGet('/api/mdm/subscriptions?page=1&pageSize=1', state.dbId); return `共 ${d?.total ?? 0} 条` }))
  state.checks = checks

  // 概览统计：按状态分页取 total（不拉全量）
  try {
    const tot = async (qs) => { const d = await apiGet(`/api/mdm/change-requests${qs}&pageSize=1`, state.dbId); return d?.total ?? 0 }
    state.stats = {
      cr: await tot('?page=1'), draft: await tot('?docStatus=draft'), approving: await tot('?docStatus=approving'),
      activated: await tot('?docStatus=activated'), rejected: await tot('?docStatus=rejected'),
    }
  } catch (e) { state.stats = null; console.warn('[mdm-health] 概览统计加载失败', e); cmx().cmxWarn?.(`概览统计加载失败：${e.message || e}`) }
}

function viewHtml() {
  const M = state.stats
  const rows = state.checks.map((c) => `<tr><td>${c.name}</td>
    <td><cmx-status-tag tone="${c.ok ? 'success' : 'danger'}" variant="subtle" dot size="sm">${c.ok ? '正常' : '异常'}</cmx-status-tag></td>
    <td class="muted">${c.detail || ''}</td></tr>`).join('')
  return `<div class="pg">
    <div class="pg-head"><div><div class="pg-title">主数据健康检查</div>
      <div class="pg-sub">验证 MDM 后端端点可用性并展示治理概览</div></div>
      <cmx-toolbar><ui5-button design="Default" icon="refresh" id="mhReload">重新检查</ui5-button></cmx-toolbar></div>
    <div class="pg-body">
      ${M ? `<div class="kpi-row">
        <cmx-kpi-card variant="card" label="变更申请" value="${M.cr}" tone="info"></cmx-kpi-card>
        <cmx-kpi-card variant="card" label="草稿" value="${M.draft}" tone="neutral"></cmx-kpi-card>
        <cmx-kpi-card variant="card" label="审批中" value="${M.approving}" tone="warning"></cmx-kpi-card>
        <cmx-kpi-card variant="card" label="已激活" value="${M.activated}" tone="success"></cmx-kpi-card>
        <cmx-kpi-card variant="card" label="已驳回" value="${M.rejected}" tone="danger"></cmx-kpi-card>
      </div>` : ''}
      <cmx-panel title="端点检查" icon="health">
        <table class="tbl"><thead><tr><th>端点</th><th>状态</th><th>详情</th></tr></thead><tbody>${rows || '<tr><td colspan="3" class="muted">检查中…</td></tr>'}</tbody></table>
      </cmx-panel>
    </div>
  </div>`
}

function bind(root) {
  rootEl = root
  root.querySelector('#mhReload')?.addEventListener('click', async () => { await runChecks(); refresh() })
}
let rootEl = null
function refresh() {
  const host = currentHost; if (!host) return
  const root = host.renderRoot || host.shadowRoot; if (!root) return
  root.innerHTML = `<style>${styleCss()}</style>${viewHtml()}`
  bind(root)
}
let currentHost = null
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
      const host = ctx && ctx.host; currentHost = host
      state.dbId = (ctx && ctx.props && (ctx.props.dbId || ctx.props.db_id)) || ''
      if (host) whenRendered(host, '.pg', (r) => bind(r))
      await runChecks(); refresh()
      return `<style>${styleCss()}</style>${viewHtml()}`
    },
  },
}
