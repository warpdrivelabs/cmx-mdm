/**
 * MDM 主数据·通用列表页（native-page，元数据驱动公共页）。
 *
 * 每种主数据挂**独立菜单节点**，差异经菜单节点 props 注入（见 mdm-menu.json）：
 *   dictCode（必填，主数据字典码）/ docType（必填，CR 单据类型）/ title（必填）/
 *   entityName（可选，按钮文案）/ icon（可选）/ columns（可选，列子集/顺序）/ searchPlaceholder（可选）。
 * 列模型从 `GET /api/dct/meta?dict=…&with_props=true` 派生：默认剔除平台/审计/治理列并尊重
 * visible:false；props.columns 有值则以其为最终显示清单。数据走 `POST /api/dct/data/search`。
 *
 * 左树三态（元数据推导，浏览语义——与 data-editor 维护页的懒加载树不同，这里一次拉全量
 * 内存建树，点节点 = 「自身 + 全部子孙」IN 过滤，适合浏览场景）：
 *   - 自分级态（dictMeta.selfHierarchy）：树 = 本字典层级（组织单元/部门/会计科目等），
 *     过滤字段 = pk；
 *   - 分组态（存在 edit.mode=cmx-dict-select 且 refDict 指向 selfHierarchy 字典的字段，
 *     如 物料.class_id→material_class）：树 = 目标分类字典，过滤字段 = 该引用字段；
 *   - 平铺态（两者皆无）：无树全宽（供应商/客户/币种等）。
 *
 * 查询条件区（cmx-filter-bar 默认 slot）：按元数据生成下拉——枚举字段
 * （edit.mode=select + enumValues）与字典引用字段（edit.mode=cmx-dict-select + refDict），
 * 上限 4 个；树占用的字段不重复生成。值等值过滤，数字列按 dataType 转 Number。
 * （条件生成器与 dct/data-editor.js 内联副本保持同步——native page 为 Blob URL 模块，
 * 页面间无法相对 import 共享代码。）
 *
 * 并列门户标签页（关闭互不影响）：
 *   新增   → portal.mdm.cr-form（mode=create + docType，每次新开——单例会在提交后复用旧表单）
 *   变更   → portal.mdm.cr-form（mode=update + docType + targetId）
 *   详情   → portal.mdm.master-detail（dictCode + recordId，透传 columns 保持一致）
 *
 * 多实例安全：state 按 host 隔离（WeakMap），同页多菜单节点/多 tab 并存互不串数据。
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }。
 */

const cmx = () => (typeof globalThis !== 'undefined' && globalThis.__cmxDataComp) || {}

// 平台/审计/治理/scope/系统列默认隐藏集合（props.columns 可覆盖）。业务列 code/name/status 不在其中。
const PLATFORM_COLS = new Set([
  'id', 'sort_no',
  'create_by', 'create_time', 'update_by', 'update_time',
  'lifecycle_status', 'published_version', 'effective_date', 'effective_from', 'effective_to',
  'disabled_reason', 'disabled_time',
  'scope_type', 'entity_id', 'is_system',
  'level_no', 'full_path', 'is_leaf', 'parent_id', 'parent_code',
])

// 元数据驱动条件下拉上限：filter-bar 行宽有限，多了挤爆。
const MAX_COND_FIELDS = 4

const { apiGet, apiPost } = globalThis.__cmxDataComp // 共享 fetch 封装（cmx-data-comp/lib/cmx-page-helpers.js；信封解包+结构化错误）

// ── 按 host 隔离的 state（多实例安全）──────────────────────────────────────
const _hostState = new WeakMap()
function initState() {
  return {
    coord: null, dbId: '',
    dictCode: '', docType: '', title: '', entityName: '', icon: '',
    columns: null, searchPlaceholder: '',
    dictMeta: null, rows: [], kw: '', page: 1, pageSize: 20, total: 0, cfgErr: '', grid: null,
    // 左树（三态见文件头注释；树装载失败自动降级无树，不阻塞列表）
    treeMode: '',        // '' | 'self' | 'group'
    treeField: '',       // 过滤字段名（self=pk / group=引用字段）
    treeMeta: null,      // 树字典元数据（self=本字典 / group=分类字典）
    treeRows: [],        // 树字典全量行
    treeTotal: 0,
    treeChildren: null,  // Map(String(parentKey)) -> rows[]
    treeRowsById: null,  // Map(String(pk)) -> row
    treeDesc: null,      // Map(String(nodeId)) -> ids[]（自身+子孙，惰性缓存）
    treeSel: '__all__',  // 选中节点 id | '__all__'
    treeExpanded: null,  // Set(String(nodeId)) 展开态
    // 查询条件
    condFields: [],      // [{ name, caption, dataType, options:[{value,label}] }]
    conds: {},           // 字段名 -> 值（等值）
  }
}
function getState(host) { if (host && !_hostState.has(host)) _hostState.set(host, initState()); return host ? _hostState.get(host) : null }

// 坐标四元组：统一 cr-form 版本（module 回退 mdm，dbId 兼读 workspace.context）。
function readCoord(ctx) {
  const p = (ctx && ctx.props) || {}
  const wctx = ctx && ctx.host && ctx.host.workspace && ctx.host.workspace.context
  const get = (k) => (wctx && typeof wctx.get === 'function') ? wctx.get(k) : undefined
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

// 打开并列门户标签页。key 含 recordId/targetId（同记录去重），无标识（如新增）每次新开。
function openTab(host, st, caption, nativePage, context) {
  let app = null
  try { app = document.querySelector('cmx-portal-app') } catch { app = null }
  if (!app || typeof app.openNode !== 'function') {
    let n = host
    for (let i = 0; i < 6 && n; i++) {
      if (typeof n.openNode === 'function') { app = n; break }
      const r = n.getRootNode && n.getRootNode(); n = r && r.host
    }
  }
  if (!app || typeof app.openNode !== 'function') { console.warn('[master-list] 未找到 portal-app.openNode'); return }
  const ctxKey = (context && (context.crId || context.recordId || (context.target && context.target.id) || context.targetId)) || ''
  const key = ctxKey || Date.now()
  const c = st.coord || {}
  app.openNode({
    id: `${nativePage}-${key}`, name: nativePage, caption, type: 'workspace-node',
    domainCode: c.domain || '', applicationCode: c.application || '',
    workspace: { content: { caption, views: [{ type: 'native_pages', native_page: nativePage, view: 'content' }] } },
  }, { initialContext: context })
}

// ── 元数据与列模型 ────────────────────────────────────────────────────────
async function loadDictMeta(st) {
  const m = await apiGet(`/api/dct/meta?${coordQs(st, { dict: st.dictCode })}&with_props=true`, st.dbId)
  return (m && m.columns) ? m : null
}
// 全量列 → 显示列：props.columns 为最终清单；否则默认过滤平台列 + visible!==false。
function buildColumns(st) {
  const C = cmx()
  if (!C.metaTableFieldsToColumns || !st.dictMeta) return []
  const c = st.coord || {}
  let cols = C.metaTableFieldsToColumns(st.dictMeta.columns || [], {
    kind: 'DCT', pk: st.dictMeta.pk, codeField: st.dictMeta.codeField, selfHierarchy: st.dictMeta.selfHierarchy,
    parentField: st.dictMeta.parentField, dictCode: st.dictMeta.dictCode, labelField: st.dictMeta.labelField,
    domain: c.domain, application: c.application, module: c.module,
  }, {
    respectOrder: true,
    coord: { domain: c.domain, application: c.application, module: c.module, ...(c.dbId ? { dbId: c.dbId } : {}) },
  })
  if (Array.isArray(st.columns) && st.columns.length) {
    cols = st.columns.map((id) => cols.find((col) => col.id === id)).filter(Boolean)
  } else {
    cols = cols.filter((col) => !PLATFORM_COLS.has(col.id) && col.visible !== false)
  }
  return cols
}
// docType 启动校验：须存在 source_doc_type=docType 的激活映射（配置脱节快速暴露）。
async function validateConfig(st) {
  if (!st.docType) { st.cfgErr = '缺少 props.docType（CR 单据类型）'; return }
  const list = (await apiGet(`/api/mdm/activations?targetDict=${encodeURIComponent(st.dictCode)}`, st.dbId)) || []
  const hit = list.some((a) => a.source_doc_type === st.docType)
  if (!hit) st.cfgErr = `未找到 dictCode=${st.dictCode} 且 source_doc_type=${st.docType} 的激活映射，请先在「激活映射配置器」配置。`
}

// ── 左树：元数据推导 + 全量内存树 ──────────────────────────────────────────
// 判定三态并装载树数据（fail-soft：任何失败 → treeMode='' 降级无树）。
async function resolveTree(st) {
  st.treeMode = ''; st.treeField = ''
  if (!st.dictMeta) return
  if (st.dictMeta.selfHierarchy) {
    st.treeMode = 'self'
    st.treeField = st.dictMeta.pk || 'id'
    await loadTreeDict(st, st.dictMeta)
    return
  }
  // 分组态：第一个 refDict 指向分级字典的引用字段
  const fl = (st.dictMeta.columns || []).find((c) => c.edit && c.edit.mode === 'cmx-dict-select' && c.refDict)
  if (!fl) return
  let m = null
  try { m = await apiGet(`/api/dct/meta?${coordQs(st, { dict: fl.refDict })}&with_props=true`, st.dbId) } catch { m = null }
  if (!m || !m.selfHierarchy) return
  st.treeMode = 'group'
  st.treeField = fl.name
  await loadTreeDict(st, m)
}

// 一次拉全量树字典（pageSize 上限 5000），内存建 children/索引映射。
async function loadTreeDict(st, meta) {
  st.treeMeta = meta
  const d = await apiPost(`/api/dct/data/search?${coordQs(st, { dict: meta.dictCode })}`, { page: 1, pageSize: 5000 }, st.dbId)
  const rows = (d && d.rows) || []
  st.treeRows = rows
  st.treeTotal = Number(d && d.total) || rows.length
  if (st.treeTotal > rows.length) {
    console.warn(`[master-list] 树字典 ${meta.dictCode} 共 ${st.treeTotal} 行，仅装载前 ${rows.length} 行（截断）`)
  }
  const pk = meta.pk || 'id'
  const pf = meta.parentField || 'parent_id'
  const children = new Map()
  const byId = new Map()
  for (const r of rows) {
    byId.set(String(r[pk]), r)
    const k = String(r[pf] == null ? 'null' : r[pf])
    if (!children.has(k)) children.set(k, [])
    children.get(k).push(r)
  }
  st.treeChildren = children
  st.treeRowsById = byId
  st.treeDesc = new Map()
  st.treeSel = '__all__'
  st.treeExpanded = new Set()
}

// 节点「自身 + 全部子孙」id 集（IN 过滤用；惰性缓存；seen 防脏数据环）。
function descIds(st, id) {
  const key = String(id)
  if (st.treeDesc.has(key)) return st.treeDesc.get(key)
  const pk = st.treeMeta.pk || 'id'
  const out = []
  const seen = new Set()
  const walk = (row) => {
    const k = String(row[pk])
    if (seen.has(k)) return
    seen.add(k)
    out.push(row[pk])
    for (const ch of (st.treeChildren.get(k) || [])) walk(ch)
  }
  const root = st.treeRowsById.get(key)
  if (root) walk(root)
  st.treeDesc.set(key, out)
  return out
}

function treeLabelOf(st, n) {
  const m = st.treeMeta || {}
  const lbl = m.labelField ? n[m.labelField] : ''
  const code = m.codeField ? n[m.codeField] : ''
  return String(lbl || code || n[m.pk || 'id'] || '')
}

// 树节点 HTML（递归：仅展开态节点渲染子层内容，收起态留空容器——大树不全量渲染 DOM）。
function treeNodeHtml(st, rows) {
  const pk = st.treeMeta.pk || 'id'
  return (rows || []).map((n) => {
    const id = String(n[pk])
    const kids = st.treeChildren.get(id) || []
    const code = st.treeMeta.codeField ? n[st.treeMeta.codeField] : ''
    const metaHtml = code !== '' ? ` <span class="ml-tree-code">${esc(code)}</span>` : ''
    const label = treeLabelOf(st, n)
    if (!kids.length) {
      return `<div class="ml-tree-node${st.treeSel === id ? ' active' : ''}" data-node-id="${escAttr(id)}" title="${escAttr(label)}">
        <span class="ml-tree-leaf">●</span><span class="ml-tree-label">${esc(label)}${metaHtml}</span></div>`
    }
    const open = st.treeExpanded.has(id)
    return `<div class="ml-tree-node${st.treeSel === id ? ' active' : ''}" data-node-id="${escAttr(id)}" title="${escAttr(label)}">
        <span class="ml-tree-toggle" data-toggle="${escAttr(id)}">${open ? '▾' : '▸'}</span>
        <span class="ml-tree-label">${esc(label)}${metaHtml}</span></div>
      <div class="ml-children" data-children-of="${escAttr(id)}" style="${open ? '' : 'display:none'}">${open ? treeNodeHtml(st, kids) : ''}</div>`
  }).join('')
}

// 整树渲染（数据在内存，重渲染不丢展开态——展开集驱动渲染；保留滚动位置）。
function renderTree(st, root) {
  const body = root.querySelector('#mlTreeBody')
  if (!body) return
  const keepScroll = body.scrollTop
  const roots = st.treeChildren.get('null') || []
  const allActive = st.treeSel === '__all__'
  body.innerHTML = `
    <div class="ml-tree-virtual${allActive ? ' active' : ''}" data-node-id="__all__" title="全量（跨所有层级）">⊕ 全部 <span class="ml-tree-count">${st.treeTotal}</span></div>
    ${treeNodeHtml(st, roots)}`
  body.scrollTop = keepScroll
}

function treeHeadTitle(st) {
  if (st.treeMode === 'self') return '字典结构'
  const n = (st.treeMeta && st.treeMeta.dictName) || '分类'
  return `按${n}筛选`
}

// ── 查询条件：元数据 → 下拉控件（与 data-editor 内联副本同步）───────────────
function colCaptionOf(c) {
  const cap = c && c.caption
  return (cap && (cap.zh_CN || cap)) || (c && c.name) || ''
}

// 数字列（BIGINT/INT/NUMERIC…）值转 Number：PG 对 INT 列传字符串参数会报类型不匹配。
function numify(v, dataType) {
  if (typeof dataType === 'string' && /INT|NUM|DEC|FLOAT|DOUBLE|SERIAL|BOOL|BIT/i.test(dataType)) {
    const n = Number(v)
    if (Number.isFinite(n)) return n
  }
  return v
}

// 拉引用字典选项（值列 = refField||'code'，与 cmx-data-comp init-page-models 的缺省一致）。
async function loadDictOptions(st, c) {
  const valueField = c.refField || 'code'
  const labelField = c.displayField || 'name'
  const d = await apiPost(`/api/dct/data/search?${coordQs(st, { dict: c.refDict })}`, { page: 1, pageSize: 500 }, st.dbId)
  const rows = (d && d.rows) || []
  return rows
    .map((r) => ({ value: String(r[valueField] ?? ''), label: String(r[labelField] ?? r[valueField] ?? '') }))
    .filter((o) => o.value !== '')
}

// 生成条件字段描述（枚举 + 字典引用；树占用字段排除；上限 MAX_COND_FIELDS）。
// filterable 开关（field-edit-display-modes §四 governance 键，with_props 铺在列顶层）：
// 显式 false 一票否决且**占坑不候补**（否决意图是精简条件区，剔除后不让后续字段顶替）；
// true/未填按默认推导。
async function buildCondFields(st) {
  st.condFields = []
  const cols = (st.dictMeta && st.dictMeta.columns) || []
  const out = []
  let used = 0 // 已消耗名额 = 已生成 + 被显式否决
  for (const c of cols) {
    if (used >= MAX_COND_FIELDS) break
    if (!c.name || PLATFORM_COLS.has(c.name) || c.visible === false) continue
    if (st.treeMode === 'group' && c.name === st.treeField) continue // 树已占用，不重复
    if (c.filterable === false) { used++; continue }
    const mode = c.edit && c.edit.mode
    if (mode === 'select' && Array.isArray(c.enumValues) && c.enumValues.length) {
      out.push({
        name: c.name, dataType: c.dataType, caption: colCaptionOf(c),
        options: c.enumValues.map((e) => ({ value: String(e.value), label: String(e.label ?? e.value) })),
      })
      used++
    } else if (mode === 'cmx-dict-select' && c.refDict) {
      let opts = null
      try { opts = await loadDictOptions(st, c) } catch { opts = null }
      if (opts && opts.length) {
        out.push({ name: c.name, dataType: c.dataType, caption: colCaptionOf(c), options: opts })
        used++
      }
    }
  }
  st.condFields = out
}

// 条件胶囊：原生 select + 字段名作首项占位（未选=字段名 ▾，选中=值）。
// 相比「外挂标签 + ui5-select」横排省约 40% 宽度——4 个条件常规视口一行放下，
// 不再依赖折叠兜底。选中态 JS 切 active 类（cyan 强调）。
function condFieldHtml(cf) {
  // kv 方式（标签 + ui5-select，下拉菜单走 UI5/neo 主题，选中后标签仍在、可辨识筛的是什么）
  const capShort = (cf.caption || '').replace(/[（(].*$/, '').trim() || cf.caption
  const opts = [`<ui5-option value="" selected>全部</ui5-option>`]
    .concat(cf.options.map((o) => `<ui5-option value="${escAttr(o.value)}">${esc(o.label)}</ui5-option>`))
  return `<label class="ml-cond" title="${escAttr(cf.caption)}"><span class="ml-cond-cap">${esc(capShort)}</span><ui5-select data-cond="${escAttr(cf.name)}">${opts.join('')}</ui5-select></label>`
}

// ── 数据装载（树过滤 + 条件过滤 + 关键字合并；数组=IN）───────────────────────
async function loadRows(st) {
  if (!st.coord || !st.dictCode) { st.rows = []; st.total = 0; return }
  const filters = {}
  if (st.treeMode && st.treeSel !== '__all__') {
    const ids = descIds(st, st.treeSel)
    if (ids.length) filters[st.treeField] = ids
  }
  for (const cf of st.condFields) {
    const v = st.conds[cf.name]
    if (v == null || v === '') continue
    filters[cf.name] = numify(v, cf.dataType)
  }
  const d = (await apiPost(`/api/dct/data/search?${coordQs(st, { dict: st.dictCode })}`, {
    page: st.page, pageSize: st.pageSize, q: st.kw || '',
    sort: { field: 'create_time', order: 'desc' },
    ...(Object.keys(filters).length ? { filters } : {}),
  }, st.dbId)) || {}
  st.rows = d.rows || []
  st.total = Number(d.total) || 0
}

// 搜索占位：props.searchPlaceholder 优先；否则按 labelField/codeField 拼通用中文文案
// 「搜索名称 / 编码…」，并把 searchable 标记列（governance 键，后端 q 会对其模糊匹配）
// 的中文 caption 一并带出（上限再补 3 个，防文案过长）。
// 不用列 caption 做名称/编码——meta 下发的 caption 常为元数据模板词（如「字典项编码」）。
function placeholderOf(st) {
  if (st.searchPlaceholder) return st.searchPlaceholder
  const dm = st.dictMeta || {}
  const capOf = (fid) => {
    const col = (dm.columns || []).find((x) => x.name === fid || x.id === fid)
    const cap = col && col.caption
    const zh = cap && typeof cap === 'object' ? (cap.zh_CN || '') : String(cap || '')
    return zh || (col && col.name) || fid
  }
  const parts = []
  if (dm.labelField) parts.push('名称')
  if (dm.codeField && dm.codeField !== dm.labelField) parts.push('编码')
  for (const c of (dm.columns || [])) {
    if (parts.length >= 5) break
    if (c.searchable !== true) continue
    if (c.name === dm.labelField || c.name === dm.codeField) continue
    const cap = capOf(c.name)
    if (cap && !parts.includes(cap)) parts.push(cap)
  }
  return parts.length ? `搜索${parts.join(' / ')}…` : '搜索…'
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
  /* 左树态：card 变横向容器，树卡与主区各自成卡 */
  .card.has-tree { flex-direction:row; padding:0; gap:12px; align-items:stretch; }
  .card.has-tree .ml-main { background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor);
    border-radius:8px; padding:12px 14px; }
  .ml-main { flex:1; min-width:0; min-height:0; display:flex; flex-direction:column; }
  .ml-tree { flex:0 0 240px; display:flex; flex-direction:column; min-height:0;
    background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px; overflow:hidden; }
  .ml-tree-head { display:flex; align-items:center; gap:7px; padding:9px 12px; font-size:12px; font-weight:600;
    color:var(--sapTitleColor); border-bottom:1px solid var(--sapList_BorderColor);
    background:color-mix(in srgb,var(--sapBackgroundColor) 75%,#000 0%); }
  .ml-tree-head .tick { width:6px; height:6px; border-radius:50%; background:var(--neo-cyan,#00b4d8); flex-shrink:0; }
  .ml-tree-body { flex:1; overflow:auto; padding:6px; }
  .ml-tree-node { display:flex; align-items:center; gap:4px; padding:4px 6px; cursor:pointer; border-radius:4px;
    font-size:12px; border-left:2px solid transparent; }
  .ml-tree-node:hover { background:color-mix(in srgb,var(--neo-cyan,#00b4d8) 8%,var(--sapList_Background,#fff)); }
  .ml-tree-node.active { background:color-mix(in srgb,var(--neo-cyan,#00b4d8) 14%,var(--sapList_Background,#fff));
    color:var(--neo-cyan,#00b4d8); font-weight:600; border-left-color:var(--neo-cyan,#00b4d8); }
  .ml-tree-toggle { cursor:pointer; user-select:none; width:14px; display:inline-block; flex-shrink:0;
    color:var(--sapContent_LabelColor); font-size:10px; }
  .ml-tree-leaf { width:14px; display:inline-block; flex-shrink:0; color:var(--neo-mint,#10b981);
    font-size:7px; text-align:center; line-height:14px; opacity:.75; }
  .ml-tree-label { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .ml-tree-code { font-size:10px; color:var(--sapContent_LabelColor); opacity:.8; }
  .ml-tree-virtual { display:flex; align-items:center; gap:4px; font-size:12px; padding:5px 6px; cursor:pointer;
    border-radius:4px; color:var(--sapContent_LabelColor); position:relative; border:1px solid transparent; margin-bottom:2px; }
  .ml-tree-virtual:hover { background:color-mix(in srgb,var(--neo-mint,#10b981) 10%,var(--sapList_Background,#fff)); }
  .ml-tree-virtual.active { color:var(--neo-mint,#10b981); font-weight:700;
    background:color-mix(in srgb,var(--neo-mint,#10b981) 14%,var(--sapList_Background,#fff));
    border-color:color-mix(in srgb,var(--neo-mint,#10b981) 35%,transparent);
    box-shadow:inset 3px 0 0 var(--neo-mint,#10b981); }
  .ml-tree-count { margin-left:auto; font-size:10px; color:var(--sapContent_LabelColor);
    background:color-mix(in srgb,var(--neo-mint,#10b981) 12%,transparent); padding:1px 6px; border-radius:8px; }
  .ml-tree-virtual.active .ml-tree-count { color:var(--neo-mint,#10b981); font-weight:600;
    background:color-mix(in srgb,var(--neo-mint,#10b981) 20%,transparent); }
  .ml-children { margin-left:14px; }
  .card-hd { display:flex; justify-content:space-between; align-items:center; gap:8px; margin-bottom:10px; }
  .card-title { font-size:15px; font-weight:600; color:var(--sapTitleColor); }
  .tbl-wrap { flex:1; min-height:0; overflow:hidden; display:flex; flex-direction:column; margin-top:10px; }
  .tbl-wrap cmx-revo-grid { display:flex; width:100%; flex:1 1 0%; min-width:0; min-height:0; flex-direction:column; }
  .cfg-err { padding:24px; color:var(--sapNegativeTextColor,#b00); font-size:13px; }
  cmx-toolbar, cmx-filter-bar { display:block; }
  /* 条件控件（kv：标签 + ui5-select，filter-bar slot 内 light DOM）。
     标签去括号尾巴后 4~6 字（44~66px）+ 控件 min 84px，4 个条件常规视口一行放下。 */
  .ml-cond { display: inline-flex; align-items: center; gap: 5px; margin: 0; flex: 0 0 auto; }
  .ml-cond-cap { font-size: 11px; color: var(--sapContent_LabelColor); white-space: nowrap; }
  .ml-cond ui5-select { min-width: 84px; max-width: 150px; }
  .ml-cond.active .ml-cond-cap { color: var(--neo-cyan, #00b4d8); font-weight: 600; }
  .ml-cond.active ui5-select { border-color: color-mix(in srgb, var(--neo-cyan, #00b4d8) 45%, transparent); }
  `
}

function viewHtml(st) {
  const ent = st.entityName || st.title || '主数据'
  if (st.cfgErr) return `<div class="pg"><div class="pg-head"><div class="pg-title">${esc(st.title || '主数据列表')}</div></div><div class="card"><div class="cfg-err">⚠ ${esc(st.cfgErr)}</div></div></div>`
  const treePart = st.treeMode ? `
    <div class="ml-tree">
      <div class="ml-tree-head"><span class="tick"></span><span>${esc(treeHeadTitle(st))}</span></div>
      <div class="ml-tree-body" id="mlTreeBody"></div>
    </div>` : ''
  const conds = st.condFields.map(condFieldHtml).join('')
  return `<div class="pg">
    <div class="pg-head"><div class="pg-title">${esc(st.title || '主数据列表')}</div>
      <div class="pg-sub">浏览已发布${esc(ent)}；新增/变更/详情以并列标签页打开</div></div>
    <div class="card${st.treeMode ? ' has-tree' : ''}">
      ${treePart}
      <div class="ml-main">
        <div class="card-hd"><div class="card-title" id="mlTotal">${esc(st.title || '主数据列表')}（共 ${st.total} 条）</div>
          <cmx-toolbar><ui5-button design="Emphasized" icon="add" id="mlAdd">新增${esc(ent)}</ui5-button><ui5-button design="Transparent" icon="refresh" slot="actions" id="mlReload">刷新</ui5-button></cmx-toolbar></div>
        <cmx-filter-bar id="mlFilter" search-label="关键字" search-placeholder="${esc(placeholderOf(st))}" collapse-overflow>${conds}</cmx-filter-bar>
        <div class="tbl-wrap"><cmx-revo-grid id="mlGrid"></cmx-revo-grid></div>
        <cmx-pager id="mlPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
      </div>
    </div></div>`
}
const { escHtml: esc, escAttr } = globalThis.__cmxDataComp // 共享转义（cmx-data-comp/lib/cmx-page-helpers.js；最严格五字符集合，文本/属性上下文皆安全）

// 列表 grid（元数据列 + 操作列）：仅建列模型与事件（bind 时一次）；
// 数据填充由 applyData 负责——页面局部更新，不整页重绘（保留输入框文字/焦点/滚动/列宽）。
function buildListGrid(host) {
  const st = getState(host); if (!st) return
  const C = cmx(); const root = host && (host.renderRoot || host.shadowRoot)
  const wrap = root && root.querySelector('.tbl-wrap'); if (!wrap) return
  // 复用模板里的 grid 壳（.tbl-wrap 内唯一），仅配列模型/选项/事件——不新建，避免双框。
  const grid = wrap.querySelector('cmx-revo-grid')
  if (!grid) return
  grid.setAttribute('data-cmx-fill-height', '')
  grid.setAttribute('data-cmx-options', '{"editable":false,"showTotals":false,"showRequiredMark":false}')
  grid.classList.add('cmx-grid-neo')
  st.grid = grid
  if (C.CmxColumnModel && C.CmxColumn) {
    const cm = new C.CmxColumnModel({ datasetId: 'master-list' })
    const cols = buildColumns(st)
    cols.push(new C.CmxColumn({ id: '_action', caption: '操作', dataType: 'VARCHAR', width: '150px', frozen: 'right', edit: { mode: 'readonly' },
      display: { mode: 'actions', actions: [
        { text: '查看详情', actionRef: 'view', icon: 'show' },
        { text: '变更', actionRef: 'edit', icon: 'edit' },
      ] } }))
    cm.setMembers(cols)
    grid.setColumnModel(cm)
  }
  grid.setOptions?.({ selectionMode: 'none', fillHeight: true, showRowIndex: true, showTotals: false })
  grid.addEventListener('cmx-cell-link-click', (e) => {
    const d = e.detail || {}; const ds = grid._ds
    const row = (ds && ds.rows && !isNaN(parseInt(d.rowId, 10))) ? ds.rows[parseInt(d.rowId, 10)] : null
    const rec = row ? (row.toPlainObject ? row.toPlainObject() : row) : null
    if (!rec) return
    const s = getState(host); if (!s) return
    const label = rec[(s.dictMeta && s.dictMeta.labelField) || 'name'] || ''
    if (d.actionRef === 'view') openTab(host, s, `${s.entityName || ''}·${label}`, 'portal.mdm.master-detail', { dictCode: s.dictCode, recordId: rec.id, title: s.title, icon: s.icon, columns: s.columns, ...coordCtx(s) })
    else if (d.actionRef === 'edit') openTab(host, s, `变更·${label}`, 'portal.mdm.cr-form', { mode: 'update', docType: s.docType, crType: 'update', targetId: rec.id, targetName: label, ...coordCtx(s) })
  })
}

// 数据落地（局部更新）：只动 total 文案、grid 数据、pager 属性——DOM/事件/焦点/滚动/列宽全保留。
// first=true（bind 后首帧）双 rAF 等 grid 布局就绪再填，其后直接填。
function applyData(host, first = false) {
  const st = getState(host); if (!st) return
  const C = cmx()
  const root = host && (host.renderRoot || host.shadowRoot); if (!root) return
  const t = root.querySelector('#mlTotal')
  if (t) t.textContent = `${st.title || '主数据列表'}（共 ${st.total} 条）`
  const pager = root.querySelector('#mlPager')
  if (pager) { pager.total = st.total; pager.page = st.page; pager.pageSize = st.pageSize }
  const grid = st.grid
  if (!grid) return
  const fill = () => {
    if (C.CmxDataSet) { const ds = new C.CmxDataSet({}); ds.setRows(st.rows); grid.setDataSet(ds) }
    else grid.setDataSet?.(st.rows)
    grid.refreshLayout?.()
  }
  if (first) requestAnimationFrame(() => requestAnimationFrame(fill))
  else fill()
}

// 左树事件（bind 一次，事件委托）：toggle 展开子层（内存数据按需渲染）、点节点过滤。
function bindTree(host, st, root) {
  const body = root.querySelector('#mlTreeBody')
  if (!body) return
  body.addEventListener('click', (ev) => {
    const t = ev.target
    if (!(t instanceof Element)) return
    const tg = t.closest('[data-toggle]')
    if (tg) {
      ev.stopPropagation()
      const id = String(tg.dataset.toggle)
      const box = body.querySelector(`[data-children-of="${CSS.escape(id)}"]`)
      if (!box) return
      if (st.treeExpanded.has(id)) {
        st.treeExpanded.delete(id)
        tg.textContent = '▸'
        box.style.display = 'none'
      } else {
        st.treeExpanded.add(id)
        tg.textContent = '▾'
        if (!box.innerHTML) box.innerHTML = treeNodeHtml(st, st.treeChildren.get(String(id)) || [])
        box.style.display = 'block'
      }
      return
    }
    const node = t.closest('[data-node-id]')
    if (!node) return
    const id = String(node.dataset.nodeId)
    if (!id || id === st.treeSel) return
    st.treeSel = id
    st.page = 1
    renderTree(st, root) // 展开态在 Set，重渲染不丢；滚动位置由 renderTree 保留
    loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) })
  })
}

function bind(host, root) {
  const st = getState(host); if (!st) return
  root.querySelector('#mlAdd')?.addEventListener('click', () => openTab(host, st, `新增${st.entityName || ''}`, 'portal.mdm.cr-form', { mode: 'create', docType: st.docType, crType: 'create', ...coordCtx(st) }))
  root.querySelector('#mlReload')?.addEventListener('click', () => { loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) }) })
  root.querySelector('#mlFilter')?.addEventListener('cmx-filter-search', (e) => { st.kw = e.detail?.text || ''; st.page = 1; loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) }) })
  root.querySelector('#mlFilter')?.addEventListener('cmx-filter-reset', () => {
    st.kw = ''; st.conds = {}; st.page = 1
    // 条件控件重置：重建 slot 内容（首项「全部」选中态由重建保证）；树选中保留（导航语义）
    const fbEl = root.querySelector('#mlFilter')
    if (fbEl) fbEl.innerHTML = st.condFields.map(condFieldHtml).join('')
    loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) })
  })
  // 条件胶囊 change（委托）：即时过滤（下拉操作成本低，无需等点搜索）
  root.addEventListener('change', (ev) => {
    const sel = ev.target
    if (!(sel instanceof Element) || !sel.hasAttribute('data-cond')) return
    const opt = ev.detail && ev.detail.selectedOption
    const v = opt ? (opt.getAttribute('value') || '') : ''
    const lab = sel.closest('.ml-cond')
    if (lab) lab.classList.toggle('active', v !== '')
    st.conds[sel.getAttribute('data-cond')] = v
    st.page = 1
    loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) })
  })
  const pager = root.querySelector('#mlPager')
  if (pager) {
    pager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== st.pageSize) { st.pageSize = d.pageSize; st.page = 1 }
      else st.page = d.page || 1
      loadRows(st).then(() => applyData(host)).catch((e) => { console.warn('[master-list] 装载失败', e); cmx().cmxError?.(`列表装载失败：${e.message || e}`) })
    })
  }
  if (st.treeMode) { renderTree(st, root); bindTree(host, st, root) }
  buildListGrid(host)
  applyData(host, true)
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
      const p = (ctx && ctx.props) || {}
      const st = getState(host)
      st.coord = readCoord(ctx)
      st.dbId = st.coord.dbId || p.dbId || p.db_id || ''
      st.dictCode = p.dictCode || ''
      st.docType = p.docType || ''
      st.title = p.title || '主数据列表'
      st.entityName = p.entityName || ''
      st.icon = p.icon || ''
      st.columns = Array.isArray(p.columns) ? p.columns : null
      st.searchPlaceholder = p.searchPlaceholder || ''
      try {
        if (!st.dictCode) { st.cfgErr = '缺少 props.dictCode（主数据字典码）' }
        else {
          await validateConfig(st)
          if (!st.cfgErr) {
            st.dictMeta = await loadDictMeta(st)
            // 左树三态推导（fail-soft 降级无树）→ 条件字段（依赖 treeField 排除占用）
            try { await resolveTree(st) } catch (e) { console.warn('[master-list] 左树装载失败，降级无树', e); st.treeMode = '' }
            try { await buildCondFields(st) } catch (e) { console.warn('[master-list] 条件字段生成失败', e); st.condFields = [] }
            await loadRows(st)
          }
        }
      } catch (e) { st.cfgErr = `初始化失败：${e.message}`; console.error('[master-list] init fail', e) }
      if (host && !st.cfgErr) whenRendered(host, '.pg', (r) => bind(host, r))
      return `<style>${styleCss()}</style>${viewHtml(st)}`
    },
  },
}
