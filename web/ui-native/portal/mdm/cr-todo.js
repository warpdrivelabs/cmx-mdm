/**
 * MDM 单据列表台（native-page · 企业级重设计）——通用页，按菜单 props 参数化：
 *   docType  过滤单据类型（= 激活映射 source_doc_type，如 gys）；缺省=聚合模式
 *   title    页面标题（缺省「单据列表」）
 *
 * 聚合模式（不传 docType，菜单「变更申请单」节点）：全类型一张列表，表格多一列「类型」，
 * 筛选栏多一个类型下拉。类型选项以「激活映射」为唯一真源（设计思想详见 loadTypeOptions）：
 * 字典出现在下拉的充要条件是配了激活映射（= 能独立走 CR 审批落字典）；新域接入只需在
 * activation-mapper 配映射，本页零改动自动出现。
 *
 * 布局：页头 → 列表面板（cmx-filter-bar + 企业表格 + 行内操作）→ 详情整页（cr-form）。
 * 纯发起人视角：提交 / 撤回 / 驳回重提 / 作废；审批办理在流程待办中心，本页不承载。
 * 提示统一 cmxInfo/cmxWarn/cmxError/cmxConfirm（禁 alert/confirm/prompt）。
 *
 * 契约：export default { defaultView:'content', views:{ async content(ctx) } }。
 */

const cmx = () => (typeof globalThis !== 'undefined' && globalThis.__cmxDataComp) || {}

const { apiGet, apiPost } = globalThis.__cmxDataComp // 共享 fetch 封装（cmx-data-comp/lib/cmx-page-helpers.js；信封解包+结构化错误）

// 详情/编辑为整页+面包屑（不用弹框），便于后续叠加流程展示。
let rootEl = null

const STATUS_META = {
  draft: { name: '草稿', tone: 'neutral' },
  approving: { name: '审批中', tone: 'warning' },
  activating: { name: '激活中', tone: 'info' },
  approved: { name: '已通过', tone: 'info' },
  activated: { name: '已激活', tone: 'success' },
  rejected: { name: '已驳回', tone: 'danger' },
  aborted: { name: '已作废', tone: 'neutral' },
}
const state = { dbId: '', docType: '', title: '单据列表', filter: 'all', keyword: '', list: [], domain: '', application: '', page: 1, pageSize: 20, total: 0,
  // 聚合模式专用三件套（构造逻辑见 loadTypeOptions）：typeFilter=下拉选中类型码；
  // typeMap=类型码→显示名（下拉与「类型」列共用）；typeOptions=下拉选项（类型码数组）。
  aggregate: false, typeFilter: '', typeMap: {}, typeOptions: [] }

const { escHtml: esc } = globalThis.__cmxDataComp // 共享转义（cmx-data-comp/lib/cmx-page-helpers.js；最严格五字符集合，文本/属性上下文皆安全）

function styleCss() {
  return `
  .pg { height:100%; overflow:hidden; display:flex; flex-direction:column; box-sizing:border-box; padding:16px 20px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor);
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  /* 列表卡片撑满剩余高度，仅表格内部滚动 */
  .list-card { display:flex; flex-direction:column; flex:1; min-height:0;
    background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px; padding:12px 14px; }
  .tbl-wrap { flex:1; min-height:0; overflow:hidden; display:flex; flex-direction:column; margin-top:10px; }
  .tbl-wrap cmx-revo-grid { display:flex; width:100%; flex:1 1 0%; min-width:0; min-height:0; flex-direction:column; }
  .tbl th { position:sticky; top:0; }
  .crumb { display:flex; align-items:center; gap:6px; font-size:13px; margin-bottom:10px; color:var(--sapContent_LabelColor); }
  .crumb a { color:var(--sapLinkColor,#0a6ed1); cursor:pointer; }
  .crumb .cur { color:var(--sapTitleColor); font-weight:600; }
  .card { background:var(--sapList_Background); border:1px solid var(--sapList_BorderColor); border-radius:8px; padding:12px 14px; margin-bottom:12px; }
  .card-title { font-size:14px; font-weight:600; color:var(--sapTitleColor); margin-bottom:8px; }
  .pg-head { margin-bottom:14px; }
  .pg-title { font-size:20px; font-weight:600; color:var(--sapTitleColor); }
  .pg-sub { font-size:12px; color:var(--sapContent_LabelColor); margin-top:2px; }
  .tbl { width:100%; border-collapse:collapse; font-size:13px; }
  .tbl th { text-align:left; padding:10px 12px; font-size:12px; font-weight:600; color:var(--sapContent_LabelColor);
    border-bottom:1px solid var(--sapList_BorderColor); background:var(--sapList_HeaderBackground,transparent); }
  .tbl td { padding:10px 12px; border-bottom:1px solid var(--sapList_BorderColor); }
  .tbl tbody tr:hover td { background:var(--sapList_Hover_Background); }
  .muted { color:var(--sapContent_LabelColor); }
  cmx-panel, cmx-toolbar, cmx-filter-bar { display:block; }
  .mask { position:fixed; inset:0; background:rgba(0,0,0,.45); display:flex; align-items:center; justify-content:center; z-index:999; }
  .dlg { width:720px; max-height:82vh; overflow:auto; border-radius:10px; padding:20px;
    background:var(--sapList_Background); color:var(--sapTextColor); border:1px solid var(--sapList_BorderColor); }
  .dlg h3 { margin:0 0 14px; font-size:16px; color:var(--sapTitleColor); }
  .dlg .sec { margin:16px 0 8px; font-size:13px; font-weight:600; color:var(--sapTitleColor); }
  `
}

function actionsHtml(r) {
  const id = r.id; const s = r.doc_status
  const b = (act, design, icon, text) => `<ui5-button design="${design}" icon="${icon}" data-act="${act}" data-id="${id}">${text}</ui5-button>`
  if (s === 'draft') return b('submit', 'Default', 'paper-plane', '提交') + b('abort', 'Transparent', 'cancel', '作废')
  if (s === 'approving') return b('approve', 'Emphasized', 'accept', '通过') + b('reject', 'Transparent', 'decline', '驳回')
  if (s === 'rejected') return b('resubmit', 'Default', 'edit', '修改重提')
  return b('view', 'Transparent', 'show', '查看')
}

function fmtTime(t) { if (!t) return ''; const s = String(t); return s.length > 19 ? s.slice(0, 19).replace('T', ' ') : s }

// function tableHtml() {
//   const rows = filtered()
//   if (!rows.length) {
//     return `<cmx-empty-state icon="document" title="暂无变更申请" description="调整过滤条件或到录入台新建申请"></cmx-empty-state>`
//   }
//   const trs = rows.map((r) => {
//     const m = STATUS_META[r.doc_status] || { name: r.doc_status, tone: 'neutral' }
//     return `<tr>
//       <td class="muted">${r.id}</td><td>${r.doc_no || ''}</td><td>${r.subject_name || ''}</td><td>${r.cr_type || ''}</td>
//       <td><cmx-status-tag tone="${m.tone}" variant="subtle" dot size="sm">${m.name}</cmx-status-tag></td>
//       <td class="muted">${fmtTime(r.create_time)}</td><td>${actionsHtml(r)}</td></tr>`
//   }).join('')
//   return `<table class="tbl"><thead><tr><th>ID</th><th>单据号</th><th>名称</th><th>类型</th><th>状态</th><th>创建时间</th><th>操作</th></tr></thead><tbody>${trs}</tbody></table>`
// }

// 页面骨架（整页仅在进页时渲染一次，之后数据变化走 applyData 局部更新）。
// 类型下拉（ctType）仅聚合模式渲染；选项显示「中文名（类型码）」双标签——中文名可读，
// 类型码保证中文名链路降级时仍可辨识；value 恒为类型码（与 docType 过滤词表一致）。
function viewHtml() {
  return `<div class="pg">
    <div class="pg-head"><div class="pg-title">${state.title}</div>
      <div class="pg-sub">提交 / 撤回 / 驳回重提 / 作废；审批通过后自动激活落字典</div></div>
    <div class="list-card">
      <div class="card-title" id="ctTotal">申请列表（共 ${state.total} 条）</div>
      <cmx-filter-bar id="ctFilter" search-placeholder="单据号/名称">
        <ui5-select id="ctStatus">
          <ui5-option value="all" ${state.filter === 'all' ? 'selected' : ''}>全部</ui5-option>
          <ui5-option value="draft" ${state.filter === 'draft' ? 'selected' : ''}>草稿</ui5-option>
          <ui5-option value="approving" ${state.filter === 'approving' ? 'selected' : ''}>待审批</ui5-option>
          <ui5-option value="rejected" ${state.filter === 'rejected' ? 'selected' : ''}>已驳回</ui5-option>
          <ui5-option value="activated" ${state.filter === 'activated' ? 'selected' : ''}>已激活</ui5-option>
          <ui5-option value="aborted" ${state.filter === 'aborted' ? 'selected' : ''}>已作废</ui5-option>
        </ui5-select>
        ${state.aggregate ? `<ui5-select id="ctType">
          <ui5-option value="" ${state.typeFilter === '' ? 'selected' : ''}>全部类型</ui5-option>
          ${state.typeOptions.map((t) => `<ui5-option value="${esc(t)}" ${state.typeFilter === t ? 'selected' : ''}>${esc(state.typeMap[t] || t)}（${esc(t)}）</ui5-option>`).join('')}
        </ui5-select>` : ''}
        <ui5-button slot="actions" design="Transparent" icon="refresh" id="ctReload">刷新</ui5-button>
      </cmx-filter-bar>
      <div class="tbl-wrap"><cmx-revo-grid id="ctGrid"></cmx-revo-grid></div>
      <cmx-pager id="ctPager" page-size="20" page-sizes="10,20,50,100"></cmx-pager>
    </div>
  </div>`
}

// 单据列表用 cmx-revo-grid（只读 + 操作列）。操作列走 display.mode='actions'，
// 按钮按 doc_status 通过 visible(model) 显隐；点击派发 cmx-cell-link-click（与 master-list 同模式）。
// 仅建列模型与事件（bind 时一次）；数据填充由 applyData 负责——页面局部更新，不整页重绘。
let listGrid = null
function buildListGrid() {
  const C = cmx(); const wrap = rootEl && rootEl.querySelector('.tbl-wrap'); if (!wrap) return
  // 复用模板里的 grid 壳（.tbl-wrap 内唯一），仅配列模型/选项/事件——不新建，避免双框。
  const grid = wrap.querySelector('cmx-revo-grid')
  if (!grid) return
  // 主内容区列表页：套 Neo 皮肤（cmx-grid-neo）+ 声明式 fill-height，与设计器列表页风格一致。
  // 不用 data-cmx-embed（那是 combo/dict 弹层内嵌场景，会跳过 Neo 皮肤导致朴素灰白外观）。
  grid.setAttribute('data-cmx-fill-height', '')
  grid.setAttribute('data-cmx-options', '{"editable":false,"showTotals":false,"showRequiredMark":false}')
  grid.classList.add('cmx-grid-neo')
  listGrid = grid
  const is = (s) => (m) => m.doc_status === s
  if (C.CmxColumnModel && C.CmxColumn) {
    const cm = new C.CmxColumnModel({ datasetId: 'crList' })
    cm.setMembers([
      new C.CmxColumn({ id: 'id', caption: 'ID', dataType: 'VARCHAR', width: '110px' }),
      new C.CmxColumn({ id: 'doc_no', caption: '单据号', dataType: 'VARCHAR', width: '150px' }),
      new C.CmxColumn({ id: 'subject_name', caption: '数据名称', dataType: 'VARCHAR', width: '150px' }),
      // 聚合模式（未挂 docType 的「变更申请单」节点）补「类型」列，区分不同主数据域的申请单。
      ...(state.aggregate ? [new C.CmxColumn({ id: 'doc_type_name', caption: '类型', dataType: 'VARCHAR', width: '120px' })] : []),
      new C.CmxColumn({ id: 'remark', caption: '业务事由', dataType: 'VARCHAR', width: '150px' }),
      new C.CmxColumn({ id: 'status_name', caption: '状态', dataType: 'VARCHAR', width: '80px' }),
      new C.CmxColumn({ id: 'create_time', caption: '创建时间', dataType: 'VARCHAR', width: '150px', display: {
        mode: 'text', format: 'datetime:YYYY-MM-DD HH:mm:ss', align: 'center',
      } }),
      new C.CmxColumn({ id: '_action', caption: '操作', dataType: 'VARCHAR', width: '200px', frozen: 'right', edit: { mode: 'readonly' },
        display: { mode: 'actions', actions: [
          // M7：审批动作上收流程待办中心（mdm_approver 候选池），本页仅保留业务视角操作。
          { text: '详情', actionRef: 'view', icon: 'detail-view' },
          { text: '提交',   actionRef: 'submit',  visible: is('draft') },
          { text: '作废',   actionRef: 'abort',   variant: 'negative', visible: is('draft') },
          { text: '撤回',   actionRef: 'withdraw', variant: 'negative', visible: is('approving') },
          { text: '修改重提', actionRef: 'resubmit',  visible: is('rejected') },
        ] } }),
    ])
    grid.setColumnModel(cm)
  }
  grid.setOptions?.({ selectionMode: 'none', fillHeight: true, showRowIndex: true, showTotals: false, allowTextSelect: true, resize: true })
  // 操作列点击：rowId 为 revo 行索引，反查真实行
  grid.addEventListener('cmx-cell-link-click', (e) => {
    const d = e.detail || {}; const ds = grid._ds
    const row = (ds && ds.rows && !isNaN(parseInt(d.rowId, 10))) ? ds.rows[parseInt(d.rowId, 10)] : null
    if (!row) return
    const r = row.toPlainObject ? row.toPlainObject() : row
    if (r.id == null) return
    doAction(d.actionRef, String(r.id))
  })
}

// 数据落地（局部更新）：只动 total 文案、grid 数据、pager 属性——DOM/事件/焦点/滚动/列宽全保留。
// first=true（bind 后首帧）双 rAF 等 grid 布局就绪再填，其后直接填。
function applyData(first = false) {
  const C = cmx()
  const t = rootEl && rootEl.querySelector('#ctTotal')
  if (t) t.textContent = `申请列表（共 ${state.total} 条）`
  const pager = rootEl && rootEl.querySelector('#ctPager')
  if (pager) { pager.total = state.total; pager.page = state.page; pager.pageSize = state.pageSize }
  const rows = state.list.map((r) => ({
    ...r,
    status_name: (STATUS_META[r.doc_status] || {}).name || r.doc_status,
    ...(state.aggregate ? { doc_type_name: state.typeMap[r.doc_type] || r.doc_type || '' } : {}),
  }))
  const grid = listGrid
  if (!grid) return
  const fill = () => {
    if (C.CmxDataSet) { const ds = new C.CmxDataSet({}); ds.setRows(rows); grid.setDataSet(ds) }
    else grid.setDataSet?.(rows)
    grid.refreshLayout?.()
  }
  if (first) requestAnimationFrame(() => requestAnimationFrame(fill))
  else fill()
}

// ── 操作 ─────────────────────────────────────────────────────────────────────
async function doAction(act, id) {
  const crId = Number(id); const M = cmx()
  try {
    if (act === 'submit') {
      const ok = await M.cmxConfirm?.({ title: '提交审批', message: `确认提交 CR-${crId}？提交后进入流程审批。`, danger: false })
      if (ok === false) return
      await apiPost('/api/mdm/change-requests/submit', { crId }, state.dbId); M.cmxInfo?.(`CR-${crId} 已提交`)
    }
    else if (act === 'withdraw') {
      // 撤回（发起人专属，后端校验）：终止当前审批实例 + CR 回草稿，修改后重提发新实例。
      const ok = await M.cmxConfirm?.({ title: '撤回申请', message: `确认撤回 CR-${crId}？当前审批将终止，单据回到草稿可修改后重新提交。`, danger: true })
      if (ok === false) return
      await apiPost('/api/mdm/change-requests/withdraw', { crId }, state.dbId)
      M.cmxInfo?.(`CR-${crId} 已撤回，回到草稿`)
    } else if (act === 'resubmit') {
      // 修改重提：驳回后在「原单据」上直接编辑重新提交——后端 submit 支持 rejected→approving，
      // 无需 clone 新 CR。打开原单据 view 页并 autoEdit 直接进编辑态；cr-form 按 rejected 状态显示编辑/提交。
      openTab(currentHost, `单据·CR-${crId}`, 'portal.mdm.cr-form',
        { mode: 'view', crId, autoEdit: true, domain: state.domain, application: state.application, module: 'mdm', dbId: state.dbId })
      return
    } else if (act === 'abort') {
      const ok = await M.cmxConfirm?.({ title: '作废', message: `确认作废 CR-${crId}？`, danger: true })
      if (ok === false) return
      await apiPost('/api/mdm/change-requests/abort', { crId }, state.dbId); M.cmxInfo?.(`CR-${crId} 已作废`)
    } else if (act === 'view') { openTab(currentHost, `单据·CR-${crId}`, 'portal.mdm.cr-form', { mode: 'view', crId, domain: state.domain, application: state.application, module: 'mdm', dbId: state.dbId }); return }
    await load(); applyData()
  } catch (e) { cmx().cmxError?.(`操作失败：${e.message}`) }
}

/**
 * 打开并列门户标签页。opts.single=true 单例复用；默认按 context.crId 多开（不同行多个详情 tab）。
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
  if (!app || typeof app.openNode !== 'function') { console.warn('[cr-todo] 未找到 portal-app.openNode'); return }
  const ctxKey = (context && context.crId) || ''
  const key = opts.single ? 'single' : (ctxKey || Date.now())
  app.openNode({
    id: `${nativePage}-${key}`, name: nativePage, caption, type: 'workspace-node',
    // 域/应用取自当前页 ctx.props（不写死）：F5 重建动态页时据此切换左侧菜单与右上角域
    domain_code: state.domain, application_code: state.application,
    workspace: { content: { caption, views: [{ type: 'native_pages', native_page: nativePage, view: 'content' }] } },
  }, { initialContext: context })
}

async function load() {
  const params = { page: state.page, pageSize: state.pageSize }
  if (state.filter !== 'all') params.docStatus = state.filter
  // 类型过滤两级归一：菜单静态 docType（单类型节点，进页即锁定）优先；聚合模式用页内
  // 类型下拉的选中值。两者最终都归一为后端 ?docType=（CR 表 doc_type 精确匹配，
  // 词表 = 激活映射 source_doc_type，后端不做模糊/翻译——所以选项集合即合法过滤值集合）。
  if (state.docType) params.docType = state.docType
  else if (state.typeFilter) params.docType = state.typeFilter
  if (state.keyword) params.keyword = state.keyword
  const d = (await apiGet(`/api/mdm/change-requests?${new URLSearchParams(params)}`, state.dbId)) || {}
  state.list = d.list || []
  state.total = Number(d.total) || 0
}

// ── 类型目录（聚合模式：类型下拉选项 + 「类型」列显示名）──────────────────────────
//
// 设计思想：类型选项以「激活映射」为唯一真源，不扫描字典目录。
//   一个字典出现在下拉的充要条件 = 配了激活映射（source_doc_type → target_dict），
//   即"能独立发起 CR 审批并激活落字典"。附属明细字典（供应商银行账户/客户地址/联系人等）
//   跟随主数据 CR 的明细行一起变更、不单独发单，天然没有映射 → 不出现在下拉，符合业务
//   语义——"下拉里少了某个字典"多数时候不是缺陷，而是它未开通独立 CR 流程（去
//   activation-mapper 配一条映射即自动出现，本页零改动）。
//
// 两段式取数（类型码与中文名职责分离）：
//   ① GET /api/mdm/activations → source_doc_type 去重 = 下拉选项 + docType 过滤值。
//      类型码与后端 CR 表 doc_type 同词表，过滤直接透传（见 load()），前端无需再映射。
//   ② DCT 定义文件 → dictionaryTables[].dictMeta 建 dictCode→dictName，仅用于把
//      target_dict 换成中文显示（目录解析与 activation-mapper loadDictCatalog 同源）。
//      此链路失败只降级显示原码（选项仍在、过滤仍可用），不阻断列表——可用性优先。
async function loadTypeOptions() {
  state.typeMap = {}; state.typeOptions = []
  const acts = (await apiGet('/api/mdm/activations', state.dbId)) || []
  const dictName = {}
  // 中文名目录：definitions/list 按 domain（DAM 坐标，见 content()）过滤后取第一个
  // module=mdm 的 DCT 文件读 config。domain 解析不到时 list 返回空 → dictName 空
  // → 全部类型降级显示英文码；find 只取第一个文件，多 DCT 文件场景需按 module 精确归属。
  try {
    const listData = await apiGet(`/api/definitions/list?domain=${encodeURIComponent(state.domain)}`, state.dbId)
    const dctItem = ((listData && listData.items) || []).find((it) => it.kind === 'DCT' && (!it.module || it.module === 'mdm'))
    if (dctItem) {
      const q = new URLSearchParams({ domain: state.domain, application: state.application, module: 'mdm', file: dctItem.file })
      const cfg = await apiGet(`/api/definitions/config?${q}`, state.dbId)
      ;(((cfg && cfg.dictionaryTables) || [])).forEach((t) => {
        const dm = t.dictMeta || {}
        if (dm.dictCode) dictName[dm.dictCode] = dm.dictName || dm.dictCode
      })
    }
  } catch (e) { console.warn('[cr-todo] dict catalog fail', e) }
  // 建类型目录：类型码 = source_doc_type；显示名三级降级 dictName[target_dict] →
  // target_dict 原码 → source_doc_type 原码，保证任何一环缺数据都有可辨识的兜底。
  // 同一类型多条映射（create/update 各一条）按 seen 去重，取首条的 target_dict。
  const seen = new Set()
  for (const a of acts) {
    const dt = a && (a.source_doc_type || a.sourceDocType)
    if (!dt || seen.has(dt)) continue
    seen.add(dt)
    const td = (a && (a.target_dict || a.targetDict)) || ''
    state.typeMap[dt] = dictName[td] || td || dt
  }
  // 按显示名中文 localeCompare 排序（近似拼音序），下拉展示稳定。
  state.typeOptions = [...seen].sort((a, b) => String(state.typeMap[a] || a).localeCompare(String(state.typeMap[b] || b), 'zh-Hans-CN'))
}

function bind(root) {
  rootEl = root
  const reload = async () => { await load(); applyData() }
  root.querySelector('#ctStatus')?.addEventListener('change', (e) => { state.filter = e.target.value || 'all'; state.page = 1; reload() })
  root.querySelector('#ctType')?.addEventListener('change', (e) => { state.typeFilter = e.target.value || ''; state.page = 1; reload() })
  // 搜索（单据号/主体名模糊）：cmx-filter-search 回车/按钮触发，reset 清空。
  // 页面局部更新（不整页重绘），输入框文字/焦点/表格滚动天然保留。
  const fb = root.querySelector('#ctFilter')
  if (fb) {
    fb.addEventListener('cmx-filter-search', (e) => {
      state.keyword = ((e.detail || {}).text || '').trim(); state.page = 1; reload()
    })
    fb.addEventListener('cmx-filter-reset', () => {
      state.keyword = ''; state.filter = 'all'; state.typeFilter = ''; state.page = 1
      const st = root.querySelector('#ctStatus'); if (st) st.value = 'all'
      const tt = root.querySelector('#ctType'); if (tt) tt.value = ''
      reload()
    })
  }
  root.querySelector('#ctReload')?.addEventListener('click', async () => { await load(); applyData() })
  // 分页（cmx-pager 独立模式）
  const pager = root.querySelector('#ctPager')
  if (pager) {
    pager.addEventListener('page-change', (e) => {
      const d = e.detail || {}
      if (d.pageSize && d.pageSize !== state.pageSize) { state.pageSize = d.pageSize; state.page = 1 }
      else state.page = d.page || 1
      reload()
    })
  }
  buildListGrid()
  applyData(true)
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
      const props = (ctx && ctx.props) || {}
      // DAM 优先从 workspace.context 读（框架 openNode 时注入），fallback props
      const wctx = ctx && ctx.host && ctx.host.workspace && ctx.host.workspace.context
      const get = (k) => (wctx && typeof wctx.get === 'function' ? wctx.get(k) : undefined)
      state.dbId = props.dbId || props.db_id || ''
      state.docType = props.docType || props.doc_type || ''
      state.title = props.title || '单据列表'
      state.aggregate = !state.docType
      state.typeFilter = ''
      // DAM 坐标（domain/application）：优先 workspace.context（框架按 shellbar 活动域注入，
      // MDM 菜单挂在 basic 域），菜单 props 兜底，页面不写死。loadTypeOptions 的中文名解析
      // 依赖 domain（definitions/list 按一层目录名精确匹配）；解析不到不报错，类型显示降级原码。
      state.domain = get('domain') || props.domain || ''
      state.application = get('application') || props.application || ''
      // 聚合模式先取类型目录（渲染筛选下拉/类型列用）；失败降级显示原始类型码，不阻断列表。
      if (state.aggregate) { try { await loadTypeOptions() } catch (e) { console.warn('[cr-todo] type options fail', e) } }
      try { await load() } catch (e) { console.error('[cr-todo] init fail', e); cmx().cmxError?.(`待办列表加载失败：${e.message || e}`) }
      if (host) whenRendered(host, '.pg', (r) => bind(r))
      return `<style>${styleCss()}</style>${viewHtml()}`
    },
  },
}
