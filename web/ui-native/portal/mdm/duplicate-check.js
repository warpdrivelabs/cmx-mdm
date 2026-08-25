/**
 * MDM 查重候选台（native-page · 企业级重设计 v4 · 双模式）。
 *
 * 双模式（页头 toggle 切换 state.mode，规则配置区共享）：
 *  - anchor（锚点查重，默认）：选目标记录 → `POST /api/mdm/records/find-duplicates`（不落库）
 *    → 候选列表 + 字段对比 → 勾选 victim 执行合并（落库）。整套逻辑保留不变。
 *  - scan（全库扫描，新增）：无需目标记录 → `POST /api/mdm/match-scan/run`（落评审队列）
 *    → 摘要卡（新发现 / 去重跳过 / 待评审总数）+ 「去工作台评审」跳转数据管家工作台。
 *
 * 设计要点（Neo 主题 + 换肤 + 克制视觉）：
 *  - 三区垂直（条件→候选→历史），`.neo-panel` 卡片分区，颜色全 `var(--sap*|--neo-*)` 派生，不硬编码。
 *  - cmx 组件不写 data-cmx-skin 即走门户默认 Neo；light/dark 自动跟随。
 *  - 单一签名：字段对比表差异行红底高亮 + 一致行弱化。
 *
 * 业务流程（锚点查重预览不落库 → 用户确认合并才落库）：
 *  ① 选数据字典 → ② 选/编辑查重规则（内嵌维护，无独立管理页）→ ③ 选目标记录 → ④ 查重（不落库）
 *  ⑤ 候选列表 + 选中展开字段对比 → ⑥ 勾选 victim 执行合并（落库）→ ⑦ 历史区可还原。
 *
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }；CMX 类经 globalThis.__cmxDataComp。
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

// 中文状态映射（后端 status 全英文，前端统一中文展示）
const STATUS_CN = {
  pending: '待处理', reviewed: '已合并', rejected: '已驳回', unmerged: '已还原',
  automerge: '自动合并', review: '待评审', nomatch: '不匹配',
  // survivorship_log 字段来源
  master: '主记录', victim: '被合并方', override: '人工裁决',
}
const DECISION_META = {
  AutoMerge: { name: '自动合并', tone: 'success' },
  Review: { name: '待评审', tone: 'warning' },
  NoMatch: { name: '不匹配', tone: 'neutral' },
}
const statusCn = (s) => STATUS_CN[s] || s || ''

// 字典坐标四元组（domain/application/module/dbId），全部来自 ctx.props，代码中不写死。
// 参照 data-editor.js 的 readDef：缺值返回 null，调用方据提示而非兜底默认值。
let coord = null
// 拼接 domain/application/module query 段（与 data-editor qs 一致）
function coordQs(extra = {}) {
  if (!coord) return new URLSearchParams(extra).toString()
  return new URLSearchParams({
    domain: coord.domain, application: coord.application, module: coord.module,
    ...(coord.dbId ? {} : {}), ...extra,
  }).toString()
}

// 全局状态
const state = {
  dictCode: '', dictName: '', dictMeta: null,  // 选中的字典 + 其 meta（columns）
  rule: null,                              // 当前查重规则（来自 match-config 或用户新建）
  rules: [],                               // 该字典已有规则列表
  ruleDirty: false,                        // 规则编辑器有未保存改动
  targetId: null, targetRow: null,         // 目标记录
  result: null,                            // 查重结果 {targetFields,candidates,thresholds}
  selCand: null,                           // 当前选中对比的候选
  victimIds: [],                           // 勾选待合并的 victim id
  // 历史区
  histDict: '', histKw: '', histPage: 1, histPageSize: 10, histList: [], histTotal: 0, allDicts: [],
  histDetailId: null, histDetail: null,   // 详情查看：选中 mergeId + detail 数据
  activeTab: 'dup',                       // 当前 tab（dup/hist），refresh 后保持不跳回
  // 双模式（页头 toggle 切换）：anchor=锚点查重（默认，按目标记录比对候选）；
  // scan=全库扫描（无目标记录，整库跑匹配、新发现落评审队列，不在此页合并）
  mode: 'anchor',
  scanning: false,                        // 全库扫描进行中（按钮置 loading/disabled，防重复点击）
  scanResult: null,                       // 全库扫描结果 { newFindings, skipped, pendingTotal }
}

function styleCss() {
  return `
  .pg { height:100%; overflow:auto; box-sizing:border-box; padding:12px 20px 16px;
    background:var(--sapBackgroundColor,#f7f7f7); color:var(--sapTextColor,#1d2d3e);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .pg-head { margin-bottom:12px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor,#1d2d3e); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); margin-top:2px; }
  .neo-panel { background:var(--sapList_Background,#fff); border:1px solid var(--neo-border,var(--sapGroup_ContentBorderColor,#d9d9d9));
    border-radius:6px; overflow:hidden; margin-bottom:12px; }
  .neo-panel-head { display:flex; align-items:center; justify-content:space-between; gap:8px; padding:8px 14px;
    background:var(--sapList_HeaderBackground,#f5f6f7); border-bottom:1px solid var(--neo-border-subtle,#e9e9e9); }
  .neo-panel-head .pt { font-size:14px; font-weight:600; color:var(--sapTitleColor,#1d2d3e); display:flex; align-items:center; gap:6px; }
  .neo-panel-head .pt ui5-icon { color:var(--neo-cyan,#00b4d8); }
  .neo-panel-body { padding:12px 14px; }
  .muted { color:var(--sapContent_LabelColor,#6a6d70); }
  .bar { display:flex; gap:10px; align-items:flex-end; flex-wrap:wrap; }
  .bar .f-item { display:flex; flex-direction:column; gap:4px; min-width:200px; flex:1 1 200px; }
  /* 字典选择框：下拉型，限制宽度避免过宽 */
  .bar .f-item.f-dict { flex:0 0 280px; max-width:300px; }
  .bar .f-item.f-rec { flex:0 0 320px; max-width:360px; }
  .bar label { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); }
  cmx-dict-select { display:block; }
  .hint { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); margin-top:6px; }
  /* 规则编辑器 */
  .rule-bar { display:flex; gap:8px; align-items:center; flex-wrap:wrap; margin-bottom:10px; }
  .rule-fields { display:flex; flex-direction:column; gap:6px; margin-top:6px; }
  .rule-row { display:flex; gap:8px; align-items:center; padding:5px 8px; border-radius:4px;
    background:var(--sapList_Background,#fff); border:1px solid var(--sapGroup_ContentBorderColor,#e9d9d9); }
  .rule-row .rf-name { min-width:120px; font-size:13px; }
  .rule-row ui5-select { min-width:120px; }
  .survive-row { margin-top:8px; }
  .survive-row .chk-grid { display:flex; flex-wrap:wrap; gap:8px 18px; margin-top:4px; }
  /* tab 布局（面板显隐由 cmx-view-tabs 组件用 inline style 接管，此处只管 tab 按钮样式） */
  cmx-view-tabs { display:flex; flex-direction:column; flex:1 1 auto; min-height:0; }
  .dc-tab-bar { display:flex; gap:4px; border-bottom:1px solid var(--sapGroup_ContentBorderColor,#d9d9d9); margin-bottom:10px; }
  .dc-tab { appearance:none; border:none; background:transparent; cursor:pointer; padding:8px 16px;
    font-size:13px; color:var(--sapContent_LabelColor,#6a6d70); border-bottom:2px solid transparent; }
  .dc-tab.active { color:var(--neo-cyan,#00b4d8); border-bottom-color:var(--neo-cyan,#00b4d8); font-weight:600; }
  .dc-panel { flex:1 1 auto; min-height:0; }
  /* 候选区 */
  .cand-wrap { min-height:200px; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:9px 12px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor,#6a6d70);
    border-bottom:1px solid var(--sapList_BorderColor,#e5e5e5); background:var(--sapList_HeaderBackground,#f5f6f7); }
  .tbl td { padding:9px 12px; border-bottom:1px solid var(--sapList_BorderColor,#e5e5e5); vertical-align:middle; }
  .tbl tbody tr { cursor:pointer; }
  .tbl tbody tr:hover td { background:var(--sapList_Hover_Background,#f5f5f5); }
  .tbl tbody tr.sel td { background:var(--sapInformationBackground,#eaf4ff); }
  .tbl tbody tr.diff td { background:var(--sapErrorBackground,#ffebeb); }
  .tbl tbody tr.same td { color:var(--sapContent_LabelColor,#6a6d70); }
  .score { font-weight:600; }
  .empty { padding:30px 12px; text-align:center; color:var(--sapContent_LabelColor,#6a6d70); font-size:13px; }
  /* 对比表 */
  .cmp-tip { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); padding:8px 0; }
  cmx-toolbar { display:flex; gap:8px; }
  /* 模式 toggle（锚点查重 / 全库扫描）：页头 segmented 控件，active 用 neo-cyan 填充 */
  .dc-mode-bar { display:inline-flex; gap:4px; margin-top:10px; padding:3px; border-radius:6px;
    background:var(--sapList_HeaderBackground,#f5f6f7); border:1px solid var(--neo-border-subtle,#e9e9e9); }
  .dc-mode-btn { appearance:none; cursor:pointer; border:none; background:transparent;
    padding:5px 14px; font-size:13px; color:var(--sapContent_LabelColor,#6a6d70); border-radius:4px; }
  .dc-mode-btn.active { background:var(--neo-cyan,#00b4d8); color:var(--sapButton_Emphasized_TextColor,#fff); font-weight:600; }
  .dc-mode-btn:disabled { opacity:0.6; cursor:default; }
  /* 全库扫描摘要卡（scan 模式替代候选区） */
  .scan-summary { display:grid; grid-template-columns:repeat(3,minmax(0,1fr)); gap:12px; }
  .scan-card { padding:14px 16px; border:1px solid var(--neo-border,var(--sapGroup_ContentBorderColor,#d9d9d9));
    border-radius:6px; background:var(--sapList_Background,#fff); }
  .scan-card .sc-num { font-size:24px; font-weight:600; color:var(--neo-cyan,#00b4d8); }
  .scan-card .sc-lbl { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); margin-top:4px; }
  .scan-actions { display:flex; gap:8px; margin-top:14px; }
  @media (max-width:640px) { .scan-summary { grid-template-columns:1fr; } }
  `
}

// ── 字典选择 ────────────────────────────────────────────────────────────
// 字典列表缓存（dictCode → {dictCode,dictName,targetTable}），避免每次搜索重复请求
let _dictListCache = null
async function loadDictList() {
  if (_dictListCache) return _dictListCache
  if (!coord) return []
  const out = []
  try {
    // 1. 取该域所有 DCT 定义文件（domain/application/module 来自 coord，不写死）
    const d = await apiGet(`/api/definitions/list?${coordQs({ kind: 'DCT' })}`, coord.dbId)
    const files = (d && d.items) || []
    // 2. 对每个文件取 config，读 dictionaryTables[].dictMeta 拿 dictCode/dictName/tableName
    await Promise.all(files.map(async (f) => {
      try {
        const fCoord = {
          domain: f.domain || coord.domain, application: f.application || coord.application,
          module: f.module || coord.module, dbId: coord.dbId,
        }
        const cfg = await apiGet(`/api/definitions/config?${new URLSearchParams({ kind: 'DCT', domain: fCoord.domain, application: fCoord.application, module: fCoord.module, file: f.file }).toString()}`, fCoord.dbId)
        const tables = (cfg && cfg.dictionaryTables) || []
        for (const t of tables) {
          const m = t.dictMeta || {}
          if (m.dictCode) out.push({ dictCode: m.dictCode, dictName: m.dictName || m.dictCode, targetTable: m.tableName || '' })
        }
      } catch (e) { /* 单文件失败跳过 */ }
    }))
  } catch (e) { console.warn('[dup-check] loadDictList 整体失败（返回空目录）:', e && e.message || e) }
  _dictListCache = out
  return out
}

function dictSource() {
  // 走 /api/definitions/list?kind=DCT 取该域所有 DCT 定义文件，再对每个文件取 config 读
  // dictionaryTables[].dictMeta 聚合出 dictCode/dictName/tableName 列表（缓存在 _dictListCache）。
  return {
    keyField: 'dictCode', labelField: 'dictName', pageSize: 100,
    search: async (query) => {
      const all = await loadDictList()
      const q = (query || '').toLowerCase().trim()
      if (!q) return all
      return all.filter((x) => (x.dictName || '').toLowerCase().includes(q) || (x.dictCode || '').toLowerCase().includes(q))
    },
    loadByKeys: async (keys) => {
      const all = await loadDictList()
      return all.filter((x) => keys.includes(x.dictCode))
    },
  }
}

function recordSource() {
  if (!state.dictCode || !coord) return null
  return {
    keyField: 'id', labelField: 'name', pageSize: 50,
    search: async (query, o) => {
      const d = await apiPost('/api/dct/data/search?' + coordQs({ dict: state.dictCode }), { page: (o && o.page) || 1, pageSize: (o && o.pageSize) || 50, q: query || '' }, coord.dbId)
      return (d && d.rows) || []
    },
    loadByKeys: async (keys) => {
      if (!keys || !keys.length) return []
      const d = await apiPost('/api/dct/data/search?' + coordQs({ dict: state.dictCode }), { page: 1, pageSize: Math.max(20, keys.length), filters: { id: keys } }, coord.dbId)
      return (d && d.rows) || []
    },
  }
}

async function loadDictMeta() {
  if (!state.dictCode) { state.dictMeta = null; return }
  state.dictMeta = await apiGet('/api/dct/meta?' + coordQs({ dict: state.dictCode, with_props: 'true' }), coord && coord.dbId)
}

async function loadRules() {
  if (!state.dictCode) { state.rules = []; return }
  state.rules = (await apiGet(`/api/mdm/match-configs?dictCode=${encodeURIComponent(state.dictCode)}`, coord && coord.dbId)) || []
  // 默认选第一条
  if (state.rules.length && !state.rule) state.rule = normalizeRule(state.rules[0])
  else if (!state.rules.length) state.rule = null
}

// 拉全部查重规则，提取 dict_code 去重（历史筛选下拉用，不依赖当前 dictCode）
async function loadAllDicts() {
  const list = (await apiGet('/api/mdm/match-configs', coord && coord.dbId)) || []
  const seen = []
  for (const c of list) { if (c.dict_code && !seen.includes(c.dict_code)) seen.push(c.dict_code) }
  state.allDicts = seen
}

// 把后端规则或用户新建统一成编辑器内部结构
function normalizeRule(r) {
  if (!r) return null
  const specs = (r.specs || []).map((s) => ({ field: s.field, weight: s.weight ?? 0, kind: s.kind || 'Exact' }))
  return {
    id: r.id || '', ruleName: r.rule_name || r.ruleName || '',
    dictCode: r.dict_code || r.dictCode || state.dictCode,
    targetTable: r.target_table || r.targetTable || (state.dictMeta && state.dictMeta.tableName) || '',
    specs, clusterKeys: r.cluster_keys || r.clusterKeys || specs.map((s) => s.field),
    surviveFields: r.survive_fields || r.surviveFields || [],
    thresholds: r.thresholds || { auto_merge: 95, review: 80 },
  }
}

// ── 渲染：查重条件区 ────────────────────────────────────────────────────
function condHtml() {
  const C = cmx()
  const dictCfg = {
    dictCode: '_selector', idCol: 'dictCode', labelCol: 'dictName',
    helpLayout: 'grid', dataSource: dictSource(), dictTitle: '选择数据字典',
    columns: [
      C.CmxColumn && new C.CmxColumn({ id: 'dictCode', caption: '字典码', dataType: 'VARCHAR', width: '140px' }),
      C.CmxColumn && new C.CmxColumn({ id: 'dictName', caption: '字典名称', dataType: 'VARCHAR' }),
    ].filter(Boolean),
  }
  const recSel = (state.dictCode && state.mode === 'anchor') ? `<div class="f-item f-rec">
      <label>目标记录</label>
      <cmx-dict-select id="dcRecord" ${state.targetRow ? `value="${state.targetRow.id}"` : ''}></cmx-dict-select>
    </div>` : ''
  // 动作按钮随模式切换：anchor=查重（需字典+目标记录+规则字段）；scan=开始扫描（需字典+已选/新建含查重字段的规则）
  const actionBtn = state.mode === 'scan'
    ? `<ui5-button design="Emphasized" icon="background-process" id="dcScan" ${(!state.dictCode || !ruleHasFields() || state.scanning) ? 'disabled' : ''}>${state.scanning ? '扫描中…' : '开始扫描'}</ui5-button>`
    : `<ui5-button design="Emphasized" icon="search" id="dcFind" ${(!state.dictCode || !state.targetId || !ruleHasFields()) ? 'disabled' : ''}>查重</ui5-button>`
  return `<section class="neo-panel">
    <div class="neo-panel-head"><div class="pt"><ui5-icon name="filter"></ui5-icon>查重条件</div></div>
    <div class="neo-panel-body">
      ${!coord ? '<div class="hint">页面坐标缺失：请在菜单节点 props 中配置 domain / application / module。</div>' : `
      <div class="bar">
        <div class="f-item f-dict">
          <label>数据字典</label>
          <cmx-dict-select id="dcDict" ${state.dictCode ? `value="${state.dictCode}"` : ''}></cmx-dict-select>
        </div>
        ${recSel}
        ${actionBtn}
      </div>
      ${state.dictCode ? ruleHtml() : '<div class="hint">请先选择数据字典</div>'}
      ${state.dictCode && state.mode === 'scan' ? '<div class="hint">请先选择或新建查重规则（需含查重字段）：扫描按所选规则执行，未配置查重字段时无法启动。</div>' : ''}`}
    </div>
  </section>`
}

function ruleHtml() {
  if (!state.dictMeta) return '<div class="hint">加载字典字段中…</div>'
  const r = state.rule
  const curId = r ? String(r.id) : ''
  const ruleOpts = state.rules.map((x) => `<ui5-option value="${x.id}" ${String(x.id) === curId ? 'selected' : ''}>${x.rule_name}</ui5-option>`).join('')
  const hasRule = !!r
  return `<div class="rule-bar">
    <label style="font-size:12px;color:var(--sapContent_LabelColor,#6a6d70);">查重规则</label>
    <ui5-select id="dcRule" style="min-width:220px;">${ruleOpts || '<ui5-option value="">（暂无规则，请新建）</ui5-option>'}</ui5-select>
    <ui5-button design="Transparent" icon="add" id="dcRuleNew">新建</ui5-button>
    <ui5-button design="Transparent" icon="edit" id="dcRuleEdit" ${!hasRule ? 'disabled' : ''}>编辑</ui5-button>
    <ui5-button design="Transparent" icon="delete" id="dcRuleDel" ${!hasRule ? 'disabled' : ''}>删除</ui5-button>
  </div>`
}

function newBlankRule() {
  // 新规则：存活字段默认全选（合并时保留所有业务字段值，减少用户困惑）
  const allFields = pickableFields().map((f) => f.name)
  return { id: '', ruleName: '新规则', dictCode: state.dictCode, targetTable: (state.dictMeta && state.dictMeta.tableName) || '', specs: [], clusterKeys: [], surviveFields: allFields, thresholds: { auto_merge: 95, review: 80 } }
}

function pickableFields() {
  if (!state.dictMeta || !state.dictMeta.columns) return []
  return state.dictMeta.columns.filter((c) => {
    if (c.isPrimaryKey) return false
    if (c.visible === false) return false
    // 过滤审计/治理列
    if (['create_by', 'create_time', 'update_by', 'update_time', 'lifecycle_status', 'published_version', 'sort_no', 'code'].includes(c.name)) return false
    return true
  })
}

// 目标记录帮助弹框的列/主键/标签列，全部从字典元数据构建（不写死）。
// 列取：codeField + labelField + 前几个可选字段（上限 5 列），用各列 caption/dataType。
function recordColumns() {
  const C = cmx()
  const m = state.dictMeta || {}
  const colsAll = m.columns || []
  const idCol = m.idField || m.pk || 'id'
  const labelCol = m.labelField || 'name'
  const codeCol = m.codeField || 'code'
  const byName = (n) => colsAll.find((c) => c.name === n)
  const picked = []
  const push = (c, w) => { if (c && !picked.some((p) => p.id === c.name)) picked.push(new C.CmxColumn({ id: c.name, caption: c.caption || c.name, dataType: c.dataType || 'VARCHAR', width: w })) }
  push(byName(codeCol), '140px')
  push(byName(labelCol), '200px')
  for (const f of pickableFields()) {
    if (picked.length >= 5) break
    if (f.name === codeCol || f.name === labelCol) continue
    push(f, '140px')
  }
  return { cols: picked, idCol, labelCol }
}

function ruleHasFields() { return !!(state.rule && state.rule.specs && state.rule.specs.length) }

// ── 渲染：候选结果区 ────────────────────────────────────────────────────
function candHtml() {
  if (!state.result) {
    return `<section class="neo-panel"><div class="neo-panel-head"><div class="pt"><ui5-icon name="duplicate"></ui5-icon>查重候选</div></div>
      <div class="neo-panel-body"><div class="empty">${state.dictCode ? '选择目标记录后点击「查重」' : '请先选择数据字典'}</div></div></section>`
  }
  const cands = (state.result.candidates) || []
  const thr = state.result.thresholds || { auto_merge: 95, review: 80 }
  const rows = cands.map((c) => {
    const m = DECISION_META[c.decision] || { name: c.decision, tone: 'neutral' }
    const ck = state.victimIds.includes(c.recordId)
    const sel = state.selCand && String(state.selCand.recordId) === String(c.recordId)
    const rec = c.fields || {}
    const lbl = state.dictMeta ? (rec[state.dictMeta.labelField || 'name'] ?? '') : (rec.name || '')
    const cod = state.dictMeta ? (rec[state.dictMeta.codeField || 'code'] ?? '') : (rec.code || '')
    return `<tr data-cand="${c.recordId}" class="${sel ? 'sel' : ''}">
      <td><ui5-checkbox ${ck ? 'checked' : ''} data-victim="${c.recordId}"></ui5-checkbox></td>
      <td class="muted">${c.recordId}</td>
      <td>${lbl}</td><td class="muted">${cod}</td>
      <td class="score">${c.score}</td>
      <td><cmx-status-tag tone="${m.tone}" variant="subtle" dot size="sm">${m.name}</cmx-status-tag></td>
    </tr>`
  }).join('')
  // 表头用字典元数据的标签列/代码列 caption
  const capOf = (n, fb) => { const c = (state.dictMeta && state.dictMeta.columns || []).find((x) => x.name === n); return (c && c.caption) || fb }
  const lblCap = capOf(state.dictMeta ? state.dictMeta.labelField : 'name', '候选名称')
  const codCap = capOf(state.dictMeta ? state.dictMeta.codeField : 'code', '代码')
  const candList = cands.length
    ? `<table class="tbl"><thead><tr><th></th><th>ID</th><th>${lblCap}</th><th>${codCap}</th><th>score</th><th>裁决</th></tr></thead><tbody>${rows}</tbody></table>`
    : `<div class="empty">未发现重复候选</div>`
  return `<section class="neo-panel">
    <div class="neo-panel-head">
      <div class="pt"><ui5-icon name="duplicate"></ui5-icon>查重候选（${cands.length}）</div>
      <cmx-toolbar>
        <ui5-button design="Emphasized" icon="combine" id="dcMerge" ${state.victimIds.length === 0 ? 'disabled' : ''}>执行合并（${state.victimIds.length}）</ui5-button>
      </cmx-toolbar>
    </div>
    <div class="neo-panel-body">
      <div class="cand-wrap">${candList}</div>
      ${state.selCand ? cmpHtml() : ''}
    </div>
  </section>`
}

function cmpHtml() {
  const cand = state.selCand
  const targetF = (state.result && state.result.targetFields) || {}
  const candF = cand.fields || {}
  const r = state.rule || {}
  // 字段集 = specs 字段 ∪ surviveFields
  const specFields = (r.specs || []).map((s) => s.field)
  const fieldSet = Array.from(new Set([...specFields, ...(r.surviveFields || [])]))
  const allCols = (state.dictMeta && state.dictMeta.columns) || []
  const caption = (f) => (allCols.find((c) => c.name === f) || {}).caption || f
  const rows = fieldSet.map((f) => {
    const tv = fmt(targetF[f]); const cv = fmt(candF[f])
    const diff = !eqVal(tv, cv)
    return `<tr class="${diff ? 'diff' : 'same'}"><td>${caption(f)}</td><td>${tv}</td><td>${cv}</td>
      <td>${diff ? '<cmx-status-tag tone="negative" variant="subtle" size="sm">差异</cmx-status-tag>' : '<cmx-status-tag tone="positive" variant="subtle" size="sm">一致</cmx-status-tag>'}</td></tr>`
  }).join('')
  const lblF = state.dictMeta ? (state.dictMeta.labelField || 'name') : 'name'
  const candLabel = candF[lblF] || cand.recordId
  return `<div style="margin-top:14px;">
    <div class="cmp-tip">字段对比：当前目标记录 vs 候选记录「${candLabel}」。目标记录默认为<b>主记录(master)</b>，勾选候选作<b>被合并方(victim)</b>，其值按存活规则并入主记录。</div>
    <table class="tbl"><thead><tr><th>字段</th><th>当前目标记录</th><th>候选记录</th><th>状态</th></tr></thead><tbody>${rows}</tbody></table>
  </div>`
}
const fmt = (v) => (v === null || v === undefined || v === '') ? '<span class="muted">—</span>' : String(v)

// 记录展示标签：id + code + 名称（合并确认框/候选展示用），字段列来自字典元数据
function recLabel(fields, id) {
  const f = fields || {}
  const lblF = state.dictMeta ? (state.dictMeta.labelField || 'name') : 'name'
  const codF = state.dictMeta ? (state.dictMeta.codeField || 'code') : 'code'
  const parts = [id != null ? `#${id}` : '', f[codF] || '', f[lblF] || ''].filter(Boolean)
  return parts.join(' ') || String(id ?? '')
}
function eqVal(a, b) { return String(a) === String(b) }

// ── 渲染：全库扫描摘要区（scan 模式替代候选区）─────────────────────────
function scanHtml() {
  const r = state.scanResult
  if (!r) {
    return `<section class="neo-panel"><div class="neo-panel-head"><div class="pt"><ui5-icon name="background-process"></ui5-icon>扫描结果</div></div>
      <div class="neo-panel-body"><div class="empty">${state.dictCode ? '点击「开始扫描」对全库执行匹配，新发现将进入待评审队列' : '请先选择数据字典'}</div></div></section>`
  }
  const cards = [
    { num: r.newFindings ?? 0, lbl: '本次新发现（进入待评审）' },
    { num: r.skipped ?? 0, lbl: '去重跳过（已存在评审）' },
    { num: r.pendingTotal ?? 0, lbl: '当前待评审总数' },
  ].map((c) => `<div class="scan-card"><div class="sc-num">${c.num}</div><div class="sc-lbl">${c.lbl}</div></div>`).join('')
  return `<section class="neo-panel">
    <div class="neo-panel-head"><div class="pt"><ui5-icon name="background-process"></ui5-icon>扫描结果</div></div>
    <div class="neo-panel-body">
      <div class="scan-summary">${cards}</div>
      <div class="scan-actions">
        <ui5-button design="Emphasized" icon="forward" id="dcGotoSteward">去工作台评审</ui5-button>
      </div>
    </div>
  </section>`
}

// ── 渲染：合并历史区 ────────────────────────────────────────────────────
function histHtml() {
  const rows = state.histList.map((g) => {
    const members = g.memberNames || []
    // master：优先后端回填的 masterName；还原后 master_id 可能为 NULL，回退 member[0]（member_ids 首元素即 master）
    const master = members.find((m) => String(m.id) === String(g.master_id)) || members[0]
    const masterLabel = g.masterName || (master && (master.name || master.code)) || (g.master_id != null ? g.master_id : '') || '—'
    const victims = members.filter((m) => m !== master).map((m) => m.name || m.code || m.id).join('、')
    const st = statusCn(g.status)
    const canUndo = g.status === 'reviewed'
    const sel = String(state.histDetailId) === String(g.id)
    return `<tr data-mid="${g.id}" class="hist-row ${sel ? 'sel' : ''}"><td>${masterLabel}</td><td>${victims}</td>
      <td><cmx-status-tag tone="${g.status === 'reviewed' ? 'success' : (g.status === 'unmerged' ? 'neutral' : 'negative')}" variant="subtle" dot size="sm">${st}</cmx-status-tag></td>
      <td>${g.score ?? ''}</td><td class="muted">${fmtTime(g.created_at)}</td>
      <td>${canUndo ? `<ui5-button design="Transparent" icon="reset" data-undo="${g.id}">还原</ui5-button>` : ''}</td></tr>`
  }).join('')
  const totalPages = Math.max(1, Math.ceil(state.histTotal / state.histPageSize))
  return `<section class="neo-panel">
    <div class="neo-panel-head"><div class="pt"><ui5-icon name="history"></ui5-icon>合并历史（共 ${state.histTotal} 条）</div></div>
    <div class="neo-panel-body">
      <div class="bar" style="margin-bottom:10px;">
        <ui5-select id="dcHistDict" style="min-width:160px;">
          <ui5-option value="" ${state.histDict === '' ? 'selected' : ''}>全部字典</ui5-option>
          ${state.allDicts.map((d) => `<ui5-option value="${d}" ${state.histDict === d ? 'selected' : ''}>${d}</ui5-option>`).join('')}
        </ui5-select>
        <ui5-input id="dcHistKw" placeholder="${state.histDict ? '搜索主记录/被合并方名称' : '请先选择字典'}" value="${state.histKw}" ${state.histDict ? '' : 'disabled'} style="min-width:240px;flex:1 1 240px;"></ui5-input>
        <ui5-button design="Default" icon="search" id="dcHistSearch" ${state.histDict ? '' : 'disabled'}>查询</ui5-button>
      </div>
      ${state.histList.length
        ? `<table class="tbl"><thead><tr><th>主记录</th><th>被合并方</th><th>状态</th><th>score</th><th>合并时间</th><th>操作</th></tr></thead><tbody>${rows}</tbody></table>`
        : '<div class="empty">暂无合并记录，点击行可查看合并详情</div>'}
      <div style="display:flex;justify-content:space-between;align-items:center;margin-top:10px;">
        <span class="muted" style="font-size:12px;">第 ${state.histPage} / ${totalPages} 页</span>
        <div style="display:flex;gap:6px;">
          <ui5-button design="Transparent" icon="nav-left" id="dcHistPrev" ${state.histPage <= 1 ? 'disabled' : ''}>上一页</ui5-button>
          <ui5-button design="Transparent" icon="nav-right" id="dcHistNext" ${state.histPage >= totalPages ? 'disabled' : ''}>下一页</ui5-button>
        </div>
      </div>
      ${state.histDetail ? histDetailHtml() : ''}
    </div>
  </section>`
}

// 合并详情：master/victims 字段对比 + survivorship 日志
function histDetailHtml() {
  const d = state.histDetail || {}
  const master = d.master || {}
  const victims = d.victims || []
  const slog = (d.group && d.group.survivorship_log) || {}
  const fields = slog.fields || []
  const fieldSet = Array.from(new Set([...Object.keys(master), ...victims.flatMap((v) => Object.keys(v))]))
    .filter((k) => !['id', 'update_time', 'create_time', 'lifecycle_status', 'published_version'].includes(k))
  const rows = fieldSet.map((f) => {
    const mv = fmt(master[f])
    const vvs = victims.map((v) => fmt(v[f])).join(' / ')
    const diff = victims.some((v) => !eqVal(fmt(master[f]), fmt(v[f])))
    const log = fields.find((x) => x.field === f)
    const from = log ? statusCn(log.from) || log.from : ''
    return `<tr class="${diff ? 'diff' : 'same'}"><td>${f}</td><td>${mv}</td><td>${vvs}</td>
      <td>${from ? `<cmx-status-tag tone="${log.from === 'master' ? 'info' : (log.from === 'override' ? 'warning' : 'positive')}" variant="subtle" size="sm">${from}</cmx-status-tag>` : ''}</td></tr>`
  }).join('')
  const reparented = slog.reparented ? Object.entries(slog.reparented).map(([t, ids]) => `${t}: ${(ids || []).length} 行`).join('，') : ''
  const deduped = slog.deduped ? Object.entries(slog.deduped).map(([t, ids]) => `${t}: ${(ids || []).length} 行`).join('，') : ''
  const lblF = state.dictMeta ? (state.dictMeta.labelField || 'name') : 'name'
  const masterLabel = master[lblF] || master.id || ''
  const victimLabels = victims.map((v) => v[lblF] || v.id).join('、')
  return `<div style="margin-top:14px;border-top:1px solid var(--sapGroup_ContentBorderColor,#d9d9d9);padding-top:10px;">
    <div class="cmp-tip">合并详情：主记录「${masterLabel}」vs 被合并方（${victimLabels}）。来源列说明存活值取自 master/victim/override。</div>
    <table class="tbl"><thead><tr><th>字段</th><th>主记录</th><th>被合并方</th><th>存活来源</th></tr></thead><tbody>${rows || '<tr><td colspan="4" class="muted">无字段差异</td></tr>'}</tbody></table>
    ${reparented ? `<div class="cmp-tip">明细行迁移：${reparented}</div>` : ''}
    ${deduped ? `<div class="cmp-tip">明细去重（软删重复行）：${deduped}</div>` : ''}
  </div>`
}
// 合并结果摘要文案（迁移/去重明细数，来自后端 MergeStats 响应）
function mergeSummary(d) {
  const r = (d && typeof d === 'object') ? d : {}
  const rep = r.reparentedTotal ?? 0
  const ded = r.dedupedTotal ?? 0
  return (rep === 0 && ded === 0) ? '合并成功' : `合并成功：迁移 ${rep} 条明细，去重 ${ded} 条`
}

const fmtTime = (s) => { if (!s) return ''; try { return new Date(s).toLocaleString('zh-CN', { hour12: false }) } catch { return s } }

function viewHtml() {
  // 页头模式 toggle（锚点查重 / 全库扫描）：切换 state.mode 后 refresh 重渲，规则配置区两模式共享
  const modeBar = `<div class="dc-mode-bar" role="tablist" aria-label="查重模式">
    <button class="dc-mode-btn ${state.mode === 'anchor' ? 'active' : ''}" data-mode="anchor" role="tab" ${state.scanning ? 'disabled' : ''}>锚点查重</button>
    <button class="dc-mode-btn ${state.mode === 'scan' ? 'active' : ''}" data-mode="scan" role="tab" ${state.scanning ? 'disabled' : ''}>全库扫描</button>
  </div>`
  return `<div class="pg">
    <div class="pg-head"><div class="pg-title">主数据查重</div>
      <div class="pg-sub">识别一物多码：锚点查重按目标记录比对候选并合并；全库扫描整库跑匹配、新发现进入待评审队列</div>
      ${modeBar}</div>
    <cmx-view-tabs active="${state.activeTab}" id="dcTabs">
      <div slot="tabs" class="dc-tab-bar">
        <button class="dc-tab" data-view="dup">${state.mode === 'scan' ? '扫描结果' : '查重候选'}</button>
        <button class="dc-tab" data-view="hist">合并历史</button>
      </div>
      <div data-view-panel="dup" class="dc-panel">${condHtml()}${state.mode === 'scan' ? scanHtml() : candHtml()}</div>
      <div data-view-panel="hist" class="dc-panel">${histHtml()}</div>
    </cmx-view-tabs>
  </div>`
}

// ── 事件绑定 ────────────────────────────────────────────────────────────
function bind(root) {
  const C = cmx()
  // tab 切换：记录当前 tab（refresh 后保持不跳回）；切到「合并历史」时按需加载
  root.querySelector('#dcTabs')?.addEventListener('cmx-view-change', (e) => {
    const v = (e.detail && e.detail.view) || 'dup'
    state.activeTab = v
    if (v === 'hist' && state.histList.length === 0) {
      loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message}`))
    }
  })
  // 字典选择
  const dcDict = root.querySelector('#dcDict')
  if (dcDict && C.CmxColumn) {
    dcDict.configure({
      dictCode: '_selector', idCol: 'dictCode', labelCol: 'dictName',
      helpLayout: 'grid', dataSource: dictSource(), dictTitle: '选择数据字典',
      helpDialogWidth: '40vw', helpDialogHeight: '70vh',
      columns: [new C.CmxColumn({ id: 'dictCode', caption: '字典码', dataType: 'VARCHAR', width: '140px' }), new C.CmxColumn({ id: 'dictName', caption: '字典名称', dataType: 'VARCHAR' })],
    })
    dcDict.addEventListener('cmx-dict-change', (e) => { const d = e.detail || {}; onDictChange(d) })
    // 回显当前已选字典（value= HTML 属性不触发内部 _value，需 setValue 程序化）
    if (state.dictCode && typeof dcDict.setValue === 'function') {
      dcDict.setValue(state.dictCode, { silent: true, displayText: state.dictName || state.dictCode }).catch(() => {})
    }
  }
  // 目标记录选择（列/主键/标签列全部来自字典元数据，不写死）
  const dcRecord = root.querySelector('#dcRecord')
  if (dcRecord && C.CmxColumn && state.dictCode && state.dictMeta) {
    const ds = recordSource()
    if (ds) {
      const { cols, idCol, labelCol } = recordColumns()
      dcRecord.configure({ dictCode: state.dictCode, idCol, labelCol, helpLayout: 'grid', dataSource: ds, dictTitle: '选择目标记录', helpDialogWidth: '60vw', helpDialogHeight: '80vh', columns: cols })
      dcRecord.addEventListener('cmx-dict-change', (e) => { const d = e.detail || {}; onRecordChange(d) })
      // 回显当前已选目标记录（显示文本取 labelCol/codeCol）
      if (state.targetId != null && typeof dcRecord.setValue === 'function') {
        const row = state.targetRow || {}
        const txt = row[labelCol] || row[state.dictMeta.codeField] || String(state.targetId)
        dcRecord.setValue(String(state.targetId), { silent: true, displayText: txt }).catch(() => {})
      }
    }
  }
  // 查重按钮
  root.querySelector('#dcFind')?.addEventListener('click', () => runFind().catch((e) => cmx().cmxError?.(`查重失败：${e.message}`)))
  // 模式 toggle（锚点查重 / 全库扫描）：扫描进行中禁止切换
  root.querySelectorAll('.dc-mode-btn').forEach((b) => b.addEventListener('click', () => {
    const m = b.dataset.mode
    if (m && m !== state.mode && !state.scanning) { state.mode = m; refresh() }
  }))
  // 全库扫描按钮
  root.querySelector('#dcScan')?.addEventListener('click', () => runScan().catch((e) => cmx().cmxError?.(`扫描失败：${e.message}`)))
  // 扫描结果 → 去工作台评审（单例 tab，重复点击复用同一标签页）
  root.querySelector('#dcGotoSteward')?.addEventListener('click', () => {
    openTab(currentHost, '数据管家工作台', 'portal.mdm.steward', {}, { single: true })
  })
  // 规则下拉切换
  root.querySelector('#dcRule')?.addEventListener('change', (e) => {
    const id = e.target.value; const r = state.rules.find((x) => String(x.id) === String(id))
    if (r) { state.rule = normalizeRule(r); refresh() }
  })
  // 新建/编辑 → 弹框；删除 → 二次确认
  root.querySelector('#dcRuleNew')?.addEventListener('click', () => openRuleDialog(newBlankRule()))
  root.querySelector('#dcRuleEdit')?.addEventListener('click', () => { if (state.rule) openRuleDialog(JSON.parse(JSON.stringify(state.rule))) })
  root.querySelector('#dcRuleDel')?.addEventListener('click', () => deleteRule().catch((e) => cmx().cmxError?.(`删除规则失败：${e.message}`)))
  // 候选行点击对比 + 勾选 victim
  root.querySelectorAll('tr[data-cand]').forEach((tr) => {
    tr.addEventListener('click', (e) => {
      if (e.target.closest('ui5-checkbox')) return // 点 checkbox 不触发对比
      const id = tr.dataset.cand
      const c = (state.result.candidates || []).find((x) => String(x.recordId) === String(id))
      state.selCand = c || null; refresh()
    })
  })
  root.querySelectorAll('[data-victim]').forEach((ck) => ck.addEventListener('change', (e) => {
    const id = Number(ck.dataset.victim)
    if (ck.checked) { if (!state.victimIds.includes(id)) state.victimIds.push(id) }
    else { state.victimIds = state.victimIds.filter((x) => x !== id) }
    refresh()
  }))
  // 执行合并
  root.querySelector('#dcMerge')?.addEventListener('click', () => doMerge().catch((e) => cmx().cmxError?.(`合并失败：${e.message}`)))
  // 历史
  root.querySelector('#dcHistDict')?.addEventListener('change', (e) => { state.histDict = e.target.value; state.histPage = 1; loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message || err}`)) })
  const hk = root.querySelector('#dcHistKw')
  hk?.addEventListener('change', (e) => { state.histKw = e.target.value })
  hk?.addEventListener('keydown', (e) => { if (e.key === 'Enter') { state.histPage = 1; loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message || err}`)) } })
  root.querySelector('#dcHistSearch')?.addEventListener('click', () => { state.histPage = 1; loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message || err}`)) })
  root.querySelector('#dcHistPrev')?.addEventListener('click', () => { if (state.histPage > 1) { state.histPage--; loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message || err}`)) } })
  root.querySelector('#dcHistNext')?.addEventListener('click', () => { state.histPage++; loadHist().then(refresh).catch((err) => cmx().cmxError?.(`加载历史失败：${err.message || err}`)) })
  root.querySelectorAll('[data-undo]').forEach((b) => b.addEventListener('click', (e) => { e.stopPropagation(); doUndo(b.dataset.undo).catch((err) => cmx().cmxError?.(`还原失败：${err.message}`)) }))
  // 历史行点击 → 加载合并详情
  root.querySelectorAll('tr.hist-row').forEach((tr) => tr.addEventListener('click', () => loadHistDetail(tr.dataset.mid).catch((err) => cmx().cmxError?.(`加载详情失败：${err.message}`))))
}

async function loadHistDetail(mid) {
  if (!mid) return
  // 同一行再次点击 → 收起详情
  if (String(state.histDetailId) === String(mid)) {
    state.histDetailId = null; state.histDetail = null; refresh(); return
  }
  state.histDetailId = mid; state.histDetail = null
  refresh()
  const d = await apiGet(`/api/mdm/merge-requests/detail?mergeId=${encodeURIComponent(mid)}`, coord && coord.dbId)
  state.histDetail = d
  refresh()
}

// 从 UI 控件同步规则到 state.rule
// ── 规则编辑弹框（cmx-floating-dialog）────────────────────────────────────
// 字段勾选区只在弹框内出现，主页面保持简洁（下拉 + 新建/编辑/删除按钮）。
function ruleDialogHtml(rule) {
  const fields = pickableFields()
  const rows = fields.map((f) => {
    const sel = rule.specs.find((s) => s.field === f.name)
    const checked = !!sel
    const weight = sel ? sel.weight : ''
    const kind = sel ? sel.kind : 'Exact'
    return `<div class="rule-row">
      <ui5-checkbox ${checked ? 'checked' : ''} data-field="${f.name}" class="rf-chk"></ui5-checkbox>
      <span class="rf-name" title="${f.name}">${f.caption}</span>
      <ui5-select data-field="${f.name}" class="rf-kind" ${!checked ? 'disabled' : ''}>
        <ui5-option value="Exact" ${kind === 'Exact' ? 'selected' : ''}>精确匹配</ui5-option>
        <ui5-option value="EditDistance" ${kind === 'EditDistance' ? 'selected' : ''}>相似度</ui5-option>
      </ui5-select>
      <ui5-number-input data-field="${f.name}" class="rf-wt" value="${weight}" min="0" max="100" step="5" ${!checked ? 'disabled' : ''} style="width:90px;"></ui5-number-input>
    </div>`
  }).join('')
  const surviveChks = fields.map((f) => `<ui5-checkbox ${rule.surviveFields.includes(f.name) ? 'checked' : ''} data-sv="${f.name}">${f.caption}</ui5-checkbox>`).join('')
  return `<div class="rule-dlg">
    <div class="rule-dlg-row"><label>规则名</label><ui5-input id="rdName" value="${rule.ruleName || ''}" placeholder="如：供应商默认查重"></ui5-input></div>
    <div class="rule-dlg-sec">查重字段（勾选参与比较的字段，配置比较方式与权重）</div>
    <div class="rule-fields">${rows || '<div class="hint">该字典无可选字段</div>'}</div>
    <div class="rule-dlg-sec">合并保留字段（合并时主记录保留这些字段的值；未勾选的字段合并时不参与存活裁决，保留主记录原值）</div>
    <div class="chk-grid">${surviveChks}</div>
  </div>`
}

// 从弹框 DOM 收集规则数据，返回 {rule, ok, msg}
function collectFromDialog(dlgRoot, baseRule) {
  const name = (dlgRoot.querySelector('#rdName')?.value || '').trim()
  if (!name) return { ok: false, msg: '请填写规则名' }
  const fields = pickableFields()
  const specs = []
  fields.forEach((f) => {
    const ck = dlgRoot.querySelector(`.rf-chk[data-field="${f.name}"]`)
    if (ck && ck.checked) {
      const kindSel = dlgRoot.querySelector(`.rf-kind[data-field="${f.name}"]`)
      const wtInput = dlgRoot.querySelector(`.rf-wt[data-field="${f.name}"]`)
      specs.push({ field: f.name, weight: Number((wtInput && wtInput.value) || 0), kind: (kindSel && kindSel.value) || 'Exact' })
    }
  })
  if (!specs.length) return { ok: false, msg: '请至少勾选一个查重字段' }
  const surviveFields = []
  dlgRoot.querySelectorAll('[data-sv]').forEach((ck) => { if (ck.checked) surviveFields.push(ck.dataset.sv) })
  return {
    ok: true,
    rule: {
      ...baseRule,
      ruleName: name,
      specs, clusterKeys: specs.map((s) => s.field), surviveFields,
      targetTable: (state.dictMeta && state.dictMeta.tableName) || baseRule.targetTable,
    },
  }
}

function openRuleDialog(baseRule) {
  const C = cmx()
  if (!customElements.get('cmx-floating-dialog')) { C.cmxError?.('弹框组件未就绪'); return }
  const dlg = document.createElement('cmx-floating-dialog')
  dlg.configure({
    title: baseRule.id ? '编辑查重规则' : '新建查重规则',
    icon: 'settings',
    confirmText: '保存',
    cancelText: '取消',
    dialogWidth: '640px',
    dialogHeight: '80vh',
    beforeClose: async (ctx) => {
      if (ctx.action !== 'confirm') return true
      // 校验 + 落盘；失败拦截关闭
      const collected = collectFromDialog(dlg, baseRule)
      if (!collected.ok) { C.cmxWarn?.(collected.msg); return false }
      try {
        const payload = { id: collected.rule.id || 0, ruleName: collected.rule.ruleName, dictCode: state.dictCode, targetTable: collected.rule.targetTable, specs: collected.rule.specs, clusterKeys: collected.rule.clusterKeys, surviveFields: collected.rule.surviveFields, thresholds: collected.rule.thresholds }
        const saved = await apiPost('/api/mdm/match-configs', payload, coord && coord.dbId)
        if (saved && saved.id) collected.rule.id = saved.id
        state.rule = collected.rule
        await loadRules()
        // loadRules 会重置 state.rule 为第一条，需回选刚保存的
        state.rule = state.rules.find((x) => String(x.id) === String(collected.rule.id)) ? normalizeRule(state.rules.find((x) => String(x.id) === String(collected.rule.id))) : collected.rule
        C.cmxInfo?.('规则已保存')
        refresh()
        return true
      } catch (e) {
        C.cmxError?.(`保存失败：${e.message}`)
        return false
      }
    },
  })
  const wrap = document.createElement('div')
  wrap.style.cssText = 'padding:14px 18px;'
  wrap.innerHTML = `<style>
    .rule-dlg { display:flex; flex-direction:column; gap:10px; }
    .rule-dlg-row { display:flex; flex-direction:column; gap:4px; }
    .rule-dlg-row label { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); }
    .rule-dlg-sec { font-size:12px; color:var(--sapContent_LabelColor,#6a6d70); margin-top:4px; }
    .rule-fields { display:flex; flex-direction:column; gap:6px; max-height:300px; overflow:auto; }
    .rule-row { display:flex; gap:8px; align-items:center; padding:5px 8px; border-radius:4px; background:var(--sapList_Background,#fff); border:1px solid var(--sapGroup_ContentBorderColor,#d9d9d9); }
    .rule-row .rf-name { min-width:140px; font-size:13px; }
    .chk-grid { display:flex; flex-wrap:wrap; gap:8px 18px; }
  </style>` + ruleDialogHtml(baseRule)
  // 勾选字段时联动启用/禁用该行的「匹配方式」与「权重」控件
  wrap.querySelectorAll('.rf-chk').forEach((ck) => {
    ck.addEventListener('change', () => {
      const row = ck.closest('.rule-row')
      if (!row) return
      const toggle = (el) => { if (!el) return; if (ck.checked) el.removeAttribute('disabled'); else el.setAttribute('disabled', '') }
      toggle(row.querySelector('.rf-kind'))
      toggle(row.querySelector('.rf-wt'))
    })
  })
  dlg.setSingleRegion(wrap, { label: '规则配置', icon: 'settings' })
  document.body.appendChild(dlg)
  dlg.openModal().then(() => {
    // 弹框关闭后移除 DOM（无论 confirm/cancel）
    dlg.remove()
  })
}

async function deleteRule() {
  const M = cmx()
  if (!state.rule || !state.rule.id) { M.cmxWarn?.('请先选择要删除的规则'); return }
  const ok = await M.cmxConfirm?.({ title: '删除规则', message: `确认删除规则「${state.rule.ruleName}」？`, danger: true })
  if (ok === false) return
  await apiPost('/api/mdm/match-configs/delete', { configId: Number(state.rule.id) }, coord && coord.dbId)
  M.cmxInfo?.('规则已删除')
  state.rule = null
  await loadRules()
  refresh()
}

async function onDictChange(detail) {
  const dictCode = (detail && (detail.id || detail.dictCode)) || ''
  state.dictCode = dictCode
  state.dictName = (detail && (detail.text || (detail.plain && detail.plain.dictName) || (detail.row && detail.row.dictName))) || dictCode
  state.dictMeta = null; state.rule = null; state.rules = []
  state.targetId = null; state.targetRow = null; state.result = null; state.selCand = null; state.victimIds = []
  if (!dictCode) { refresh(); return }
  await loadDictMeta()
  await loadRules()
  refresh()
}

function onRecordChange(detail) {
  if (detail.id == null || detail.id === '') { state.targetId = null; state.targetRow = null }
  else { state.targetId = detail.id; state.targetRow = detail.plain || detail.row || null }
  const root = rootEl; if (root) { const b = root.querySelector('#dcFind'); if (b) b.disabled = !state.dictCode || !state.targetId || !ruleHasFields() }
}

// ── 业务动作 ────────────────────────────────────────────────────────────
async function runFind() {
  if (!state.dictCode || !state.targetId || !ruleHasFields()) { cmx().cmxWarn?.('请先选择字典、目标记录，并配置查重字段'); return }
  const r = state.rule
  const lblF = state.dictMeta ? (state.dictMeta.labelField || 'name') : 'name'
  const codF = state.dictMeta ? (state.dictMeta.codeField || 'code') : 'code'
  const payload = {
    dictCode: state.dictCode, recordId: Number(state.targetId), targetTable: r.targetTable,
    specs: r.specs, clusterKeys: r.clusterKeys, surviveFields: r.surviveFields,
    displayFields: [lblF, codF],
  }
  state.result = await apiPost('/api/mdm/records/find-duplicates', payload, coord && coord.dbId)
  state.selCand = null; state.victimIds = []
  refresh()
  cmx().cmxInfo?.(`查重完成，发现 ${((state.result && state.result.candidates) || []).length} 个候选`)
}

// 全库扫描（scan 模式）：无目标记录，对整库执行匹配，新发现落评审队列（不在此页合并）。
// 规则配置可复用 anchor 模式的值（specs/clusterKeys/surviveFields）；未配置则不传，后端从 md_match_config 读默认。
async function runScan() {
  const M = cmx()
  if (!state.dictCode) { M.cmxWarn?.('请先选择数据字典'); return }
  if (!ruleHasFields()) { M.cmxWarn?.('请先选择或新建含查重字段的规则'); return }
  if (state.scanning) return
  state.scanning = true; state.scanResult = null
  refresh()
  try {
    const r = state.rule
    const payload = { dictCode: state.dictCode }
    if (r && r.targetTable) payload.targetTable = r.targetTable
    if (ruleHasFields()) {
      payload.specs = r.specs
      payload.clusterKeys = r.clusterKeys
      payload.surviveFields = r.surviveFields
    }
    const res = await apiPost('/api/mdm/match-scan/run', payload, coord && coord.dbId)
    state.scanResult = res || { newFindings: 0, skipped: 0, pendingTotal: 0 }
    M.cmxInfo?.(`扫描完成：新发现 ${state.scanResult.newFindings ?? 0} 条，待评审 ${state.scanResult.pendingTotal ?? 0} 条`)
  } finally {
    state.scanning = false
    refresh()
  }
}

async function doMerge() {
  const M = cmx()
  if (!state.victimIds.length) { M.cmxWarn?.('请先勾选要合并的候选'); return }
  const r = state.rule; if (!r) return
  // master/victim 均展示 id + code + 名称（master 用后端返回的 targetFields，含 code/name）
  const targetName = recLabel((state.result && state.result.targetFields) || state.targetRow, state.targetId)
  const victims = state.victimIds.map((id) => {
    const c = (state.result.candidates || []).find((x) => String(x.recordId) === String(id))
    return recLabel(c && c.fields, id)
  })
  const ok = await M.cmxConfirm?.({
    title: '确认合并', danger: true,
    message: `确认执行合并？\n\n保留为主记录(master)：${targetName}\n将被废弃标记已合并(victim)：${victims.join('、')}\n\n说明：被合并方可完整还原；主记录被合并带过来的字段值不会回退，如需修正请走变更单。`,
  })
  if (ok === false) return
  const d = await apiPost('/api/mdm/merge-requests', {
    dictCode: state.dictCode, masterId: Number(state.targetId), victimIds: state.victimIds,
    targetTable: r.targetTable, surviveFields: r.surviveFields,
  }, coord && coord.dbId)
  M.cmxInfo?.(mergeSummary(d))
  // 刷新候选（剔除已合并）+ 历史
  state.victimIds = []; state.selCand = null
  await runFind().catch(() => {})
  await loadHist()
  refresh()
}

async function doUndo(mergeId) {
  const M = cmx()
  const ok = await M.cmxConfirm?.({
    title: '确认还原', message: '还原会让被合并方完整恢复（状态、明细、交叉引用）；但主记录被合并带过来的字段值不会回退。是否继续？',
  })
  if (ok === false) return
  await apiPost('/api/mdm/merge-requests/undo', { mergeId: Number(mergeId) }, coord && coord.dbId)
  M.cmxInfo?.('已还原')
  state.histDetailId = null; state.histDetail = null
  await loadHist(); refresh()
}

async function saveRule() {
  // 兼容旧调用；实际保存逻辑在 openRuleDialog 的 beforeClose 钩子内完成
  const M = cmx()
  if (!state.rule) { M.cmxWarn?.('请先新建或选择规则'); return }
  openRuleDialog(JSON.parse(JSON.stringify(state.rule)))
}

async function loadHist() {
  const q = new URLSearchParams({ page: String(state.histPage), pageSize: String(state.histPageSize) })
  if (state.histDict) q.set('dictCode', state.histDict)
  // 名称搜索走后端（D-05）：仅在选了字典时传 kw（"全部字典"时后端无法解析目标表会忽略）
  if (state.histDict && state.histKw) q.set('kw', state.histKw)
  const d = await apiGet('/api/mdm/merge-requests?' + q.toString(), coord && coord.dbId)
  state.histList = (d && d.list) || []
  state.histTotal = (d && d.total) || 0
}

// ── 渲染循环 ────────────────────────────────────────────────────────────
let rootEl = null; let currentHost = null
function refresh() {
  const host = currentHost; if (!host) return
  const root = host.renderRoot || host.shadowRoot; if (!root) return
  root.innerHTML = `<style>${styleCss()}</style>${viewHtml()}`
  rootEl = root
  bind(root)
}
function whenRendered(host, sel, cb, t) {
  const n = t == null ? 60 : t
  const root = host && (host.renderRoot || host.shadowRoot)
  if (root && root.querySelector(sel)) { cb(root); return }
  if (n <= 0) return
  requestAnimationFrame(() => whenRendered(host, sel, cb, n - 1))
}

// 从 workspace.context（框架 openNode 注入）或 ctx.props 读取字典坐标四元组，不写死默认值。
// domain/application/module 缺任一返回 null（调用方据提示，不用兜底默认）。
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

/**
 * 打开并列门户标签页（照抄 master-list.js 模式，域/应用取自 coord 不写死）。
 * opts.single=true 单例复用（如「去工作台评审」重复点击聚焦同一 tab）；默认按 context 业务 id 多开。
 * addTab 按 id 去重：同 id 复用并同步 context，不同 id 新开。
 */
function openTab(host, caption, nativePage, context, opts = {}) {
  let app = null
  try { app = document.querySelector('cmx-portal-app') } catch { app = null }
  if (!app || typeof app.openNode !== 'function') {
    let n = host
    for (let i = 0; i < 6 && n; i++) {
      if (typeof n.openNode === 'function') { app = n; break }
      const r = n.getRootNode && n.getRootNode(); n = r && r.host
    }
  }
  if (!app || typeof app.openNode !== 'function') { console.warn('[duplicate-check] 未找到 portal-app.openNode'); return }
  const ctxKey = (context && (context.mergeId || context.recordId || context.crId)) || ''
  const key = opts.single ? 'single' : (ctxKey || Date.now())
  app.openNode({
    id: `${nativePage}-${key}`, name: nativePage, caption, type: 'workspace-node',
    // 带上域/应用（来自当前页 ctx.props 经 coord 解析，不写死）：F5 重建动态页时据此切换左侧菜单与右上角域。
    domainCode: (coord && coord.domain) || '', applicationCode: (coord && coord.application) || '',
    workspace: { content: { caption, views: [{ type: 'native_pages', native_page: nativePage, view: 'content' }] } },
  }, { initialContext: context })
}

export default {
  defaultView: 'content',
  views: {
    async content(ctx) {
      const host = ctx && ctx.host; currentHost = host
      coord = readCoord(ctx)
      // 预加载全部查重字典（历史筛选下拉用，不依赖当前 dictCode）
      try { await loadAllDicts() } catch (e) { console.error('[dup-check] loadAllDicts', e); cmx().cmxWarn?.(`查重字典目录加载失败：${e.message || e}`) }
      // 历史改为切到「合并历史」tab 时按需加载（loadHist 在 bind 的 cmx-view-change 里触发）
      if (host) whenRendered(host, '.pg', (r) => { rootEl = r; bind(r) })
      // coord 缺失时仍渲染页面，条件区提示「请配置菜单 props 的 domain/application/module」
      return `<style>${styleCss()}</style>${viewHtml()}`
    },
  },
}
