/**
 * MDM 数据管家工作台（native-page · 双 tab 重设计）。
 *
 * 布局：页头（字典下拉 + tab 切换）→
 *   tab1「查重发现项」：zone-bar（待评审/已合并/已忽略）+ 发现项列表 + 详情弹层（字段对比 + 合并/忽略）
 *   tab2「合并历史」：zone-bar（待审/已合并/已驳回/已还原）+ 队列表格 + 评审弹层（红线 diff + 逐字段裁决）
 *
 * 发现项（md_match_scan）是系统全库扫描出的重复簇，管家评审载体；
 * 合并历史（md_merge_record）是已确认的合并事务记录。两者职责分离。
 *
 * 数据装载（v2 重构，告别全量拉取 + 前端 filter）：
 *   - zone 计数：调 GET /api/mdm/workbench/summary（一次拿两表各状态计数），不再前端 filter().length。
 *   - 列表：按当前 zone（status）向后端分页请求（match-scan / merge-requests 均支持 status+page+pageSize+total），
 *     配 cmx-pager 分页条；不再 pageSize=500/200 全量拉取。
 *   - 下拉：选的是「数据字典」（表只有 dict_code 维度，无规则 id），显示字典中文名。
 * 提示统一 cmxInfo/cmxError/cmxConfirm；禁 alert/confirm。
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
async function apiPost(url, payload, dbId) {
  const h = { 'Content-Type': 'application/json', Accept: 'application/json' }; if (dbId) h.db_id = dbId
  const r = await fetch(url, { method: 'POST', headers: h, credentials: 'same-origin', body: JSON.stringify(payload || {}) })
  return unwrap(r, await r.json().catch(() => null))
}

// 字典坐标四元组（domain/application/module/dbId），来自 ctx.props 或 workspace.context。
// 仅用于加载 DCT 字典中文名映射；缺失时字典名回退显示 dictCode。
let coord = null
function coordQs(extra = {}) {
  if (!coord) return new URLSearchParams(extra).toString()
  return new URLSearchParams({
    domain: coord.domain, application: coord.application, module: coord.module,
    ...extra,
  }).toString()
}
function readCoord(ctx) {
  const p = (ctx && ctx.props) || {}
  const wctx = ctx && ctx.host && ctx.host.workspace && ctx.host.workspace.context
  const get = (k) => (wctx && typeof wctx.get === 'function' ? wctx.get(k) : undefined)
  const c = {
    domain: get('domain') || p.domain || '',
    application: get('application') || p.application || '',
    module: get('module') || p.module || '',
    dbId: p.dbId || p.db_id || '',
  }
  return (c.domain && c.application && c.module) ? c : null
}

// 合并记录 tab 的 zone（md_merge_record.status）。方案 B 下合并即 reviewed，无 pending/rejected；
// 管家可对已合并记录撤销（undo）→ unmerged。
const ZONES = [
  { code: 'reviewed', name: '已合并', tone: 'success' },
  { code: 'unmerged', name: '已还原', tone: 'neutral' },
]
// 发现项 tab 的 zone（md_match_scan.status）
const FINDING_ZONES = [
  { code: 'pending', name: '待评审', tone: 'warning' },
  { code: 'resolved', name: '已合并', tone: 'success' },
  { code: 'ignored', name: '已忽略', tone: 'neutral' },
]
const state = {
  dbId: '',
  dicts: [],            // [{dictCode, dictName}] 有查重规则的字典（match-configs ∩ DCT 中文名）
  dictConfigMap: {},    // dictCode → match_config（含 survive_fields，供字段对比动态取列）
  dictCode: '',         // 当前选中字典（init 后默认 dicts[0]）
  tab: 'findings',
  // 计数（来自 summary 接口，替代前端 filter 计数）：{findings:{status:n}, merges:{status:n}}
  counts: { findings: {}, merges: {} },
  // 发现项（md_match_scan，按 zone + 分页）
  findingsZone: 'pending',
  findingsPage: 1, findingsPageSize: 20, findingsTotal: 0, findings: [],
  findingDetail: null,
  // 合并历史（md_merge_record，按 zone + 分页）
  zone: 'reviewed',
  page: 1, pageSize: 20, total: 0, groups: [],
  detail: null, rulings: {},
}

function styleCss() {
  return `
  .pg { height:100%; overflow:auto; box-sizing:border-box; padding:16px 20px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .pg-head { margin-bottom:14px; display:flex; align-items:center; justify-content:space-between; flex-wrap:wrap; gap:12px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor); margin-top:2px; }
  .head-tools { display:flex; align-items:center; gap:6px; }
  .head-tools label { font-size:12px; color:var(--sapContent_LabelColor); }
  .dict-sel { min-width:200px; }
  .tab-bar { display:flex; gap:4px; margin-bottom:14px; border-bottom:1px solid var(--sapList_BorderColor); }
  .tab-btn { padding:8px 16px; font-size:13px; cursor:pointer; border:none; background:transparent;
    color:var(--sapContent_LabelColor); border-bottom:2px solid transparent; }
  .tab-btn.active { color:var(--neo-cyan,#00b4d8); border-bottom-color:var(--neo-cyan,#00b4d8); font-weight:600; }
  .zone-bar { display:flex; gap:8px; margin-bottom:14px; background:var(--sapList_Background);
    border:1px solid var(--sapList_BorderColor); border-radius:8px; padding:8px; }
  .zone-tab { flex:1; display:flex; flex-direction:column; align-items:center; gap:2px; padding:8px 12px;
    border-radius:6px; cursor:pointer; border:1px solid transparent; }
  .zone-tab .z-name { font-size:13px; color:var(--sapTextColor); }
  .zone-tab:hover { background:var(--sapList_Hover_Background); }
  .zone-tab.active { border-color:var(--neo-cyan,#00b4d8); background:color-mix(in srgb, var(--neo-cyan,#00b4d8) 12%, transparent); }
  .zone-tab.active .z-name { color:var(--neo-cyan,#00b4d8); font-weight:600; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:10px 12px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor);
    border-bottom:1px solid var(--sapList_BorderColor); }
  .tbl td { padding:10px 12px; border-bottom:1px solid var(--sapList_BorderColor); vertical-align:middle; }
  .tbl tbody tr:hover td { background:var(--sapList_Hover_Background); }
  .muted { color:var(--sapContent_LabelColor); }
  .pager-row { display:flex; justify-content:flex-end; padding:8px 4px 0; }
  cmx-panel { display:block; }
  cmx-pager { display:inline-flex; }
  `
}

// ─── 数据装载 ─────────────────────────────────────────────────────────────
// DCT 字典中文名映射：dictCode → dictName。需 coord；缺失返回 {}（调用方回退 dictCode）。
let _dictNameCache = null
async function loadDictNameMap() {
  if (_dictNameCache) return _dictNameCache
  const map = {}
  if (!coord) { _dictNameCache = map; return map }
  try {
    const d = await apiGet(`/api/definitions/list?${coordQs({ kind: 'DCT' })}`, coord.dbId)
    const files = (d && d.items) || []
    await Promise.all(files.map(async (f) => {
      try {
        const cfg = await apiGet(`/api/definitions/config?${new URLSearchParams({ kind: 'DCT', domain: coord.domain, application: coord.application, module: coord.module, file: f.file }).toString()}`, coord.dbId)
        for (const t of ((cfg && cfg.dictionaryTables) || [])) {
          const m = t.dictMeta || {}
          if (m.dictCode) map[m.dictCode] = m.dictName || m.dictCode
        }
      } catch (e) { /* 单文件失败跳过 */ }
    }))
  } catch (e) { /* 整体失败返回已收集 */ }
  _dictNameCache = map
  return map
}

// 拉全部查重规则，建字典列表（含中文名）+ 配置索引（dictCode → match_config）。init 时调一次。
async function loadDicts() {
  const list = (await apiGet('/api/mdm/match-configs', state.dbId)) || []
  state.dictConfigMap = {}
  const seen = []
  for (const c of list) {
    if (c.dict_code && !state.dictConfigMap[c.dict_code]) {
      state.dictConfigMap[c.dict_code] = c
      seen.push(c.dict_code)
    }
  }
  const nameMap = await loadDictNameMap()
  state.dicts = seen.map((dc) => ({ dictCode: dc, dictName: nameMap[dc] || dc }))
  if (!state.dictCode && state.dicts.length) state.dictCode = state.dicts[0].dictCode
}
// 当前字典的字段对比列（从 match_config.survive_fields 动态取；缺失则空数组）
function diffFields() {
  return ((state.dictConfigMap[state.dictCode] || {}).survive_fields) || []
}
// zone 计数：来自 summary 接口（findings/merges 各 status 计数），不再前端 filter。
async function loadCounts() {
  if (!state.dictCode) { state.counts = { findings: {}, merges: {} }; return }
  const d = (await apiGet(`/api/mdm/workbench/summary?dictCode=${encodeURIComponent(state.dictCode)}`, state.dbId)) || {}
  state.counts = { findings: d.findings || {}, merges: d.merges || {} }
}
function findingsCount(code) { return state.counts.findings[code] ?? 0 }
function zoneCount(code) { return state.counts.merges[code] ?? 0 }

// 发现项列表：按当前 zone（status）分页请求（后端支持 status+page+pageSize+total）。
async function loadFindings() {
  const d = (await apiGet(`/api/mdm/match-scan?dictCode=${encodeURIComponent(state.dictCode)}&status=${state.findingsZone}&page=${state.findingsPage}&pageSize=${state.findingsPageSize}`, state.dbId)) || {}
  state.findings = d.list || []
  state.findingsTotal = d.total || 0
}
async function loadFindingDetail(scanId) {
  state.findingDetail = await apiGet(`/api/mdm/match-scan/detail?scanId=${scanId}`, state.dbId)
  state.rulings = {}  // 清空裁决态（发现项/合并历史弹框复用同一 rulings 字段）
}
// 合并记录列表：按当前 zone（status）分页（reviewed/unmerged）。
async function loadGroups() {
  const d = (await apiGet(`/api/mdm/merge-requests?dictCode=${encodeURIComponent(state.dictCode)}&status=${state.zone}&page=${state.page}&pageSize=${state.pageSize}`, state.dbId)) || {}
  state.groups = d.list || []
  state.total = d.total || 0
}
async function loadDetail(mergeId) {
  state.detail = await apiGet(`/api/mdm/merge-requests/detail?mergeId=${mergeId}`, state.dbId)
  state.rulings = {}
}

// ─── 页头（字典下拉 + tab） ────────────────────────────────────────────────
function headHtml() {
  return `<div class="pg-head">
    <div><div class="pg-title">数据管家工作台</div>
      <div class="pg-sub">查重发现项评审合并 · 合并记录查看与撤销</div></div>
    <div class="head-tools">
      <label>数据字典</label>
      <ui5-select class="dict-sel" data-dict>
        ${state.dicts.length
          ? state.dicts.map((d) => `<ui5-option value="${d.dictCode}" ${state.dictCode === d.dictCode ? 'selected' : ''}>${d.dictName && d.dictName !== d.dictCode ? `${d.dictName}（${d.dictCode}）` : d.dictCode}</ui5-option>`).join('')
          : '<ui5-option>（暂无查重规则）</ui5-option>'}
      </ui5-select>
    </div>
  </div>
  <div class="tab-bar">
    <button class="tab-btn ${state.tab === 'findings' ? 'active' : ''}" data-tab="findings">查重发现项</button>
    <button class="tab-btn ${state.tab === 'history' ? 'active' : ''}" data-tab="history">合并记录</button>
  </div>`
}

// ─── tab1：查重发现项 ──────────────────────────────────────────────────────
function findingsBarHtml() {
  return `<div class="zone-bar">${FINDING_ZONES.map((z) => `
    <div class="zone-tab ${state.findingsZone === z.code ? 'active' : ''}" data-fz="${z.code}">
      <span class="z-name">${z.name}</span><span class="z-count" style="font-size:16px;font-weight:600;color:var(--sapContent_LabelColor)">${findingsCount(z.code)}</span>
    </div>`).join('')}</div>`
}
function findingsQueueHtml() {
  const list = state.findings
  const rows = list.length ? list.map((s) => {
    const fz = FINDING_ZONES.find((z) => z.code === s.status) || {}
    return `<tr>
    <td class="muted">${s.id}</td>
    <td>${s.cluster_key || ''}</td>
    <td>${s.member_count || 0}</td>
    <td>${s.max_score ?? ''}</td>
    <td><cmx-status-tag tone="${fz.tone || 'neutral'}" variant="subtle" size="sm">${fz.name || s.status}</cmx-status-tag></td>
    <td>${s.status === 'pending' ? `<ui5-button design="Emphasized" icon="inspect" data-freview="${s.id}">评审</ui5-button><ui5-button design="Transparent" icon="decline" data-fignore="${s.id}">忽略</ui5-button>` : ''}</td></tr>`
  }).join('') : null
  return `<cmx-panel title="发现项 · ${state.findingsZone}" icon="search">
    ${rows
      ? `<table class="tbl"><thead><tr><th>id</th><th>簇键</th><th>成员数</th><th>最高分</th><th>状态</th><th>操作</th></tr></thead><tbody>${rows}</tbody></table>
         <div class="pager-row"><cmx-pager id="stFindingsPager" page="${state.findingsPage}" page-size="${state.findingsPageSize}" page-sizes="10,20,50" total="${state.findingsTotal}"></cmx-pager></div>`
      : `<cmx-empty-state icon="search" title="该区暂无发现项"></cmx-empty-state>`}
  </cmx-panel>`
}
function findingDiffHtml() {
  const fd = state.findingDetail || {}
  const scan = fd.scan || {}
  const members = fd.members || []
  const victimId = members[1]?.id
  const heads = members.map((m, i) => `<th>${m.id}${i === 0 ? '（master）' : i === 1 ? '（victim）' : ''}</th>`).join('')
  // 每字段裁决列：master / victim（取成员 members[1]）/ 手填。默认 master（=首个成员优先存活）。
  const rows = diffFields().map((f) => {
    const vals = members.map((m) => `<td>${m[f] ?? ''}</td>`).join('')
    const r = state.rulings[f] || { pick: 'master', text: '' }
    return `<tr><td>${f}</td>${vals}
      <td class="rule"><select data-f="${f}" data-k="pick" style="padding:5px 8px;font-size:12px;border-radius:4px;border:1px solid var(--sapField_BorderColor);background:var(--sapField_Background);color:var(--sapField_TextColor)">
        <option value="master" ${r.pick === 'master' ? 'selected' : ''}>master</option>
        <option value="victim" ${r.pick === 'victim' ? 'selected' : ''}>victim</option>
        <option value="custom" ${r.pick === 'custom' ? 'selected' : ''}>手填</option>
      </select> <input data-f="${f}" data-k="text" placeholder="手填值" value="${r.text || ''}" style="padding:5px 8px;font-size:12px;border-radius:4px;border:1px solid var(--sapField_BorderColor);background:var(--sapField_Background);color:var(--sapField_TextColor);display:${r.pick === 'custom' ? '' : 'none'}"></td></tr>`
  }).join('')
  return `<h3>发现项对比 · cluster=${scan.cluster_key || ''}（${members.length} 成员）</h3>
    <table class="tbl"><thead><tr><th>字段</th>${heads}<th>裁决</th></tr></thead><tbody>${rows}</tbody></table>
    <p class="muted" style="margin-top:12px;font-size:12px">默认每字段取 master；可逐字段改选 victim（取成员 ${victimId ?? '-'}）或手填。首个成员为 master，其余成员为 victims 被合并。</p>
    <div class="dlg-foot">
      <ui5-button design="Transparent" id="fdBack">返回</ui5-button>
      <ui5-button design="Negative" icon="decline" id="fdIgnore">忽略</ui5-button>
      <ui5-button design="Emphasized" icon="combine" id="fdMerge">按裁决合并</ui5-button>
    </div>`
}

// ─── tab2：合并历史 ────────────────────────────────────────────────────────
function zoneBarHtml() {
  return `<div class="zone-bar">${ZONES.map((z) => `
    <div class="zone-tab ${state.zone === z.code ? 'active' : ''}" data-z="${z.code}">
      <span class="z-name">${z.name}</span><span class="z-count" style="font-size:16px;font-weight:600;color:var(--sapContent_LabelColor)">${zoneCount(z.code)}</span>
    </div>`).join('')}</div>`
}
function queueHtml() {
  const list = state.groups
  const rows = list.length ? list.map((g) => {
    const gz = ZONES.find((z) => z.code === g.status) || {}
    const ops = g.status === 'reviewed'
      ? `<ui5-button design="Transparent" icon="show" data-view="${g.id}">查看</ui5-button><ui5-button design="Negative" icon="undo" data-undo="${g.id}">撤销</ui5-button>`
      : g.status === 'unmerged'
        ? `<ui5-button design="Transparent" icon="show" data-view="${g.id}">查看</ui5-button>` : ''
    return `<tr>
    <td class="muted">${g.id}</td><td class="muted">${g.master_id ?? ''}</td><td>${g.score ?? ''}</td>
    <td><cmx-status-tag tone="${gz.tone || 'neutral'}" variant="subtle" size="sm">${gz.name || g.status}</cmx-status-tag></td>
    <td>${ops}</td></tr>`
  }).join('') : null
  return `<cmx-panel title="合并记录 · ${state.zone}" icon="list">
    ${rows
      ? `<table class="tbl"><thead><tr><th>group</th><th>master</th><th>score</th><th>状态</th><th>操作</th></tr></thead><tbody>${rows}</tbody></table>
         <div class="pager-row"><cmx-pager id="stMergePager" page="${state.page}" page-size="${state.pageSize}" page-sizes="10,20,50" total="${state.total}"></cmx-pager></div>`
      : `<cmx-empty-state icon="list" title="该区暂无合并记录"></cmx-empty-state>`}
  </cmx-panel>`
}
function diffHtml() {
  const d = state.detail || {}
  const m = d.master || {}; const v = (d.victims || [])[0] || {}
  const g = d.group || {}
  // 合并留痕 survivorship_log：{fields:[{field,from,value}], reparented, deduped}（detail 接口已 parse）
  const log = (g.survivorship_log && g.survivorship_log.fields) || []
  const logMap = {}; for (const e of log) logMap[e.field] = e
  const rows = diffFields().map((f) => {
    const mv = m[f] ?? ''; const vv = v[f] ?? ''
    const differ = String(mv) !== String(vv)
    const e = logMap[f] || {}
    return `<tr class="${differ ? 'diffrow' : ''}" style="${differ ? 'background:color-mix(in srgb, var(--sapCriticalColor,#e76500) 10%, transparent)' : ''}"><td>${f}</td><td>${mv}</td><td>${vv}</td>
      <td>${e.from ?? ''}</td><td>${e.value ?? ''}</td></tr>`
  }).join('')
  const canUndo = g.status === 'reviewed'
  return `<h3>合并留痕 · group=${g.id}（${g.status || ''}）</h3>
    <table class="tbl"><thead><tr><th>字段</th><th>master(${m.id || ''})</th><th>victim(${v.id || ''})</th><th>取自</th><th>最终值</th></tr></thead><tbody>${rows}</tbody></table>
    <div class="dlg-foot">
      <ui5-button design="Transparent" id="stBack">返回</ui5-button>
      ${canUndo ? `<ui5-button design="Negative" icon="undo" id="stUndo">撤销合并</ui5-button>` : ''}
    </div>`
}

// ─── 弹层（挂 document.body，fixed 铺满视口） ──────────────────────────────
let dlgEl = null
function dlgCss() {
  return `
  .mdm-mask { position:fixed; inset:0; background:rgba(0,0,0,.45); display:flex; align-items:center; justify-content:center; z-index:999; }
  .mdm-dlg { width:860px; max-height:84vh; overflow:auto; border-radius:10px; padding:20px;
    background:var(--sapList_Background,#1a2332); color:var(--sapTextColor,#eef); border:1px solid var(--sapList_BorderColor,#334); }
  .mdm-dlg h3 { margin:0 0 14px; font-size:16px; color:var(--sapTitleColor,#fff); }
  .mdm-dlg .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .mdm-dlg .tbl th { text-align:left; padding:8px 10px; font-size:12px; color:var(--sapContent_LabelColor,#9ab); border-bottom:1px solid var(--sapList_BorderColor,#334); }
  .mdm-dlg .tbl td { padding:8px 10px; border-bottom:1px solid var(--sapList_BorderColor,#334); }
  .mdm-dlg .dlg-foot { margin-top:16px; display:flex; justify-content:flex-end; gap:8px; }
  `
}
function openDiff(html, bindFn) {
  closeDiff()
  const mask = document.createElement('div'); mask.className = 'mdm-mask'
  mask.innerHTML = `<style>${dlgCss()}</style><div class="mdm-dlg">${html}</div>`
  dlgEl = mask
  mask.addEventListener('click', (e) => { if (e.target === mask) closeDiff() })
  if (bindFn) bindFn(mask)
  document.body.appendChild(mask)
}
function closeDiff() { if (dlgEl) { dlgEl.remove(); dlgEl = null } }

function bindFindingDiff(scope) {
  // 裁决控件：select 切换显隐手填 input（与合并历史 diffHtml 同一 data-f/data-k 约定）
  scope.querySelectorAll('[data-f]').forEach((el) => el.addEventListener('change', () => {
    const f = el.dataset.f; const k = el.dataset.k
    state.rulings[f] = state.rulings[f] || { pick: 'master', text: '' }
    state.rulings[f][k] = el.value
    if (k === 'pick') {
      const inp = scope.querySelector(`input[data-f="${f}"][data-k="text"]`)
      if (inp) inp.style.display = el.value === 'custom' ? '' : 'none'
    }
  }))
  scope.querySelector('#fdBack')?.addEventListener('click', () => { closeDiff(); state.findingDetail = null })
  scope.querySelector('#fdIgnore')?.addEventListener('click', () => doFindingIgnore(state.findingDetail.scan.id))
  scope.querySelector('#fdMerge')?.addEventListener('click', async () => {
    try { await doFindingMerge() } catch (e) { cmx().cmxError?.(`合并失败：${e.message}`) }
  })
}
function bindHistoryDiff(scope) {
  // 合并记录详情只读（逐字段裁决已在发现项完成）；仅返回 + 撤销（reviewed）
  scope.querySelector('#stBack')?.addEventListener('click', () => { closeDiff(); state.detail = null })
  scope.querySelector('#stUndo')?.addEventListener('click', async () => {
    try { await doUndo(state.detail.group.id) } catch (e) { cmx().cmxError?.(`撤销失败：${e.message}`) }
  })
}

// ─── 操作 ─────────────────────────────────────────────────────────────────
// 合并结果摘要文案（迁移/去重明细数，来自后端 MergeStats 响应）
function mergeSummary(d) {
  const r = (d && typeof d === 'object') ? d : {}
  const rep = r.reparentedTotal ?? 0
  const ded = r.dedupedTotal ?? 0
  return (rep === 0 && ded === 0) ? '合并成功' : `合并成功：迁移 ${rep} 条明细，去重 ${ded} 条`
}

async function doFindingMerge() {
  const M = cmx()
  const scan = (state.findingDetail || {}).scan || {}
  const members = (state.findingDetail || {}).members || []
  if (members.length < 2) { M.cmxWarn?.('成员不足 2，无法合并'); return }
  const masterId = members[0].id
  const victimIds = members.slice(1).map((m) => m.id)
  const { survivorship, overrides } = collectFindingRulings()
  // targetTable/surviveFields 不传，后端从 match_config 回填；带逐字段裁决立即合并（无 mergeId）
  const d = await apiPost('/api/mdm/merge-requests', {
    dictCode: state.dictCode, masterId, victimIds, scanId: scan.id, survivorship, overrides,
  }, state.dbId)
  M.cmxInfo?.(mergeSummary(d)); closeDiff(); state.findingDetail = null
  await loadCounts(); await loadFindings(); refresh()
}
async function doFindingIgnore(scanId) {
  const M = cmx()
  const ok = await M.cmxConfirm?.({ title: '忽略发现项', message: `确认忽略发现项 ${scanId}？`, danger: true })
  if (ok === false) return
  await apiPost('/api/mdm/match-scan/ignore', { scanId: Number(scanId) }, state.dbId)
  M.cmxInfo?.('已忽略'); closeDiff(); state.findingDetail = null
  await loadCounts(); await loadFindings(); refresh()
}

// 发现项逐字段裁决收集：master=members[0]、victim 代表=members[1]（首个 victim）。
// pick=master→survivorship[f]='master'；pick=victim→overrides[f]=members[1][f]；pick=custom→overrides[f]=手填值。
function collectFindingRulings() {
  const members = (state.findingDetail || {}).members || []
  const victim = members[1] || {}
  const survivorship = {}; const overrides = {}
  for (const f of diffFields()) {
    const r = state.rulings[f]; if (!r) continue
    if (r.pick === 'master') survivorship[f] = 'master'
    else if (r.pick === 'victim') overrides[f] = victim[f] ?? null
    else overrides[f] = r.text
  }
  return { survivorship, overrides }
}
// 撤销合并（reviewed→unmerged）。后端一次只恢复 member_ids 中非 master 的首个 victim
// （merge.rs:420），多 victim 簇需多次撤销。二次确认避免误操作。
async function doUndo(mergeId) {
  const M = cmx()
  const ok = await M.cmxConfirm?.({ title: '撤销合并', message: `确认撤销合并记录 ${mergeId}？将恢复一个 victim（多 victim 簇需多次撤销）。`, danger: true })
  if (ok === false) return
  await apiPost('/api/mdm/merge-requests/undo', { mergeId: Number(mergeId) }, state.dbId)
  M.cmxInfo?.('已撤销合并'); closeDiff(); state.detail = null
  await loadCounts(); await loadGroups(); refresh()
}

// ─── 渲染 / 绑定 ──────────────────────────────────────────────────────────
function viewHtml() {
  const body = state.tab === 'findings'
    ? `${findingsBarHtml()}${findingsQueueHtml()}`
    : `${zoneBarHtml()}${queueHtml()}`
  return `<div class="pg">${headHtml()}${body}</div>`
}

async function reloadCurrent() {
  if (state.tab === 'findings') await loadFindings()
  else await loadGroups()
}

function bindPager(root, sel, onPage) {
  const p = root.querySelector(sel)
  if (!p) return
  p.addEventListener('page-change', (e) => {
    const { page, pageSize } = e.detail || {}
    onPage(page, pageSize)
    reloadCurrent().then(refresh)
  })
}

function bind(root) {
  // 字典切换：计数 + 列表都重载，页码归 1
  root.querySelector('[data-dict]')?.addEventListener('change', async (e) => {
    state.dictCode = e.target.value; state.findingsPage = 1; state.page = 1
    await loadCounts(); await reloadCurrent(); refresh()
  })
  // tab 切换
  root.querySelectorAll('[data-tab]').forEach((b) => b.addEventListener('click', async () => {
    state.tab = b.dataset.tab; await reloadCurrent(); refresh()
  }))
  // 发现项 zone 切换：换 status 重置页码，重新分页请求（不再前端 filter）
  root.querySelectorAll('[data-fz]').forEach((k) => k.addEventListener('click', async () => {
    state.findingsZone = k.dataset.fz; state.findingsPage = 1; await reloadCurrent(); refresh()
  }))
  // 发现项分页
  bindPager(root, '#stFindingsPager', (page, pageSize) => { state.findingsPage = page; state.findingsPageSize = pageSize })
  // 发现项操作
  root.querySelectorAll('[data-freview]').forEach((b) => b.addEventListener('click', async () => {
    await loadFindingDetail(b.dataset.freview); openDiff(findingDiffHtml(), bindFindingDiff)
  }))
  root.querySelectorAll('[data-fignore]').forEach((b) => b.addEventListener('click', () => doFindingIgnore(b.dataset.fignore)))
  // 合并历史 zone 切换
  root.querySelectorAll('[data-z]').forEach((k) => k.addEventListener('click', async () => {
    state.zone = k.dataset.z; state.page = 1; state.detail = null; await reloadCurrent(); refresh()
  }))
  // 合并历史分页
  bindPager(root, '#stMergePager', (page, pageSize) => { state.page = page; state.pageSize = pageSize })
  // 合并记录操作：查看（只读留痕）/ 撤销
  root.querySelectorAll('[data-view]').forEach((b) => b.addEventListener('click', async () => {
    await loadDetail(b.dataset.view); openDiff(diffHtml(), bindHistoryDiff)
  }))
  root.querySelectorAll('[data-undo]').forEach((b) => b.addEventListener('click', () => doUndo(b.dataset.undo)))
}

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
      coord = readCoord(ctx)
      state.dbId = (ctx && ctx.props && (ctx.props.dbId || ctx.props.db_id)) || (coord && coord.dbId) || ''
      try { await loadDicts(); await loadCounts(); await reloadCurrent() } catch (e) { console.error('[steward] init fail', e); cmx().cmxError?.(`初始化失败：${e.message || e}`) }
      if (host) whenRendered(host, '.pg', (r) => bind(r))
      return `<style>${styleCss()}</style>${viewHtml()}`
    },
  },
}
