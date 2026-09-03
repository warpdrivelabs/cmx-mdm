/**
 * MDM 通用变更申请表单（native-page · 并列标签页）。
 *
 * 双层元数据驱动，零业务字段硬编码：
 *   1) activation 配置（按 source_doc_type + cr_type 定位）→ 给出目标字典、头字段映射
 *      （header_mapping：{源字段:目标列}）、头分组（header_groups）、明细映射（line_mappings）、
 *      主体名标识（subject_name_field）、关键信息字段（key_fields → 步骤①表单；dedup=true
 *      者进 /mdm/check-key 多字段加权查重；未配置则无步骤①，直接完整表单不查重）。
 *   2) 目标字典 dct/meta → 列模型经组件库标准管线 metaTableFieldsToColumns 派生
 *      （edit.mode / refDict→cmx-dict-select / enumValues→select / required / 系统列只读）。
 *   header_mapping 的 key 即 CR 录入字段名，value（目标列）去 dct/meta 取展示属性——一份配置两处复用。
 *
 * 调用约定（列表台 openTab 经 workspace.context 传入）：
 *   { docType:'gys', crType:'create' | 'update', target?<已有字典记录> }
 *
 * 保存走平台标准单据链路：C.saveDocData → POST /doc/save（坐标 basic/dataplatform/mdm），
 * doc_no 由 cmx-code 按 codeRule 铸号，前端不传 doc_no。
 * 头表骨架（doc_status/line_no/doc_type_id/doc_date/entity_id）前端显式占位；
 * JSONB 列（payload/field_deltas/line_payload）传对象（不序列化）。
 * 流转 submit 调 MDM 专属 /mdm/change-requests/submit。
 */

const cmx = () => (typeof globalThis !== 'undefined' && globalThis.__cmxDataComp) || {}
const { apiGet, apiPost } = globalThis.__cmxDataComp // 共享 fetch 封装（cmx-data-comp/lib/cmx-page-helpers.js；信封解包+结构化错误）

// 轻量 toast（保存成功等轻反馈，3s 自动消失，对齐 activation-mapper / registry-center 范式）。
const { showCmxToast } = globalThis.__cmxDataComp // 共享 toast（cmx-data-comp/lib/cmx-toast.js；治理清单 B-05）

// 头表分组渲染样式（前端配置，不存后端）：card=卡片分区 / bar=色条+下分隔线。改此常量切换。
const HEAD_GROUP_STYLE = 'bar'
// 单据状态中文映射（doc_status 显示用；存储仍为英文枚举，由状态机管理）
const STATUS_LABEL = { draft: '草稿', approving: '审批中', approved: '已通过', activated: '已激活', rejected: '已驳回', aborted: '已作废' }
// 系统管理头字段：状态机/铸号管理，前端只读展示、不参与收集
const SYS_HEAD_FIELDS = new Set(['doc_status', 'doc_no'])
// step：create 模式初始 1（先查重），update 模式初始 2（改已有记录，跳过查重）
const state = {
  dbId: '', coord: null,
  docType: '', crType: 'create', mode: 'create',
  targetId: null, targetName: '',   // update：变更目标字典记录 id / 名称（tab 标题）
  target: null,                     // update：按 targetId 加载的头记录（扁平 search 行，供 buildHead/headInitialValue 取列值）
  targetLines: {},                  // update：各明细类型预填行 { [lineType]: rows }
  activation: null,        // 命中的 activation 配置
  dictMeta: null,          // 头字典 dct/meta
  headMap: [],             // [[srcField, tgtCol]] 按 header_mapping 顺序（数据构造用）
  headCols: [],            // CmxColumn[]（id=srcField，标准派生）—— 渲染用
  nameFieldKey: '',        // 提升到 subject_name 的录入字段 key
  nameCaption: '',         // 主体名字段 caption（查重表单标题/校验提示）
  keyDefs: [],             // 关键信息查重字段定义 [{src,tgt,weight,kind}]（key_fields 配置 + 主体名保底）
  headerGroups: [],
  lineDefs: [],            // [{lineType, targetDict, targetTable, parentIdField, meta, map:[[src,tgt]], cols:[CmxColumn]}]
  step: 1, keyVals: {}, savedCrId: null,
  crId: null, crHead: null, crLines: [],
  editing: false,              // view 模式草稿编辑态（true=表单可编辑、右侧操作区切保存/取消）
  autoEdit: false,             // 入口标志：修改重提打开 rejected/draft 原单据时直接进编辑态
  deletedLineIds: [],          // 被删的已入库明细行真实 id（仿 cmx-doc merge：deleted 增量删；未入库新建行不记）
  // M7 流程审批态：待办中心经表单绑定打开本页时注入 formMode:'approve'（bizId/taskId/instanceId
  // 随 props；宿主注入的 mode:'task' 被显式忽略——本页 mode 只认 view/update/create）。
  flowApprove: false, taskId: '', instanceId: '',
  // 退回重办态：apply 节点任务（formMode:'edit' + bizId）——审批被退回打回发起人，
  // 打开原 CR 只读查看 + 「确认并继续」办结 apply；可编辑修改后保存再确认。
  flowEdit: false,
  flowTrail: null,             // 流程轨迹组件数据（[{instance,definition,comments}] 仅最新一轮实例，见 ftLoad；渲染在 <cmx-flow-trail>）
  reviewCtx: null,             // M7.1 流程按钮数据源 { canReview, canWithdraw, taskId, instanceId }
  loading: true, loadErr: '',
  saving: false,             // 保存/提交/单据操作进行中（互斥锁+busy 态）：防连点并发——首次保存并发会插多条单
}
let rootEl = null
// init 调用令牌：每次 content 重新进入页面时自增，使旧的异步加载链在 await 后判定过期而中止，
// 避免刷新时新旧两次 init 并发，把明细重复 push 进共享 state.lineDefs（刷新翻倍 bug）。
let initToken = 0
const q = (id) => rootEl && rootEl.querySelector('#' + id)

// 字典坐标四元组（domain/application/module/dbId），来自 ctx.props / workspace.context；module 回退 mdm。
function readCoord(ctx) {
  const p = (ctx && ctx.props) || {}
  const wctx = ctx && ctx.host && ctx.host.workspace && ctx.host.workspace.context
  const get = (k) => (wctx && typeof wctx.get === 'function' ? wctx.get(k) : undefined)
  return {
    domain: get('domain') || p.domain || '',
    application: get('application') || p.application || '',
    module: get('module') || p.module || 'mdm',
    dbId: p.dbId || p.db_id || get('dbId') || get('db_id') || '',
  }
}
function coordQs(extra = {}) {
  const c = state.coord || {}
  return new URLSearchParams({
    domain: c.domain || '', application: c.application || '', module: c.module || 'mdm', ...extra,
  }).toString()
}

function styleCss() {
  return `
  .pg { height:100%; display:flex; flex-direction:column; gap:10px; box-sizing:border-box; padding:12px 16px;
    background:var(--sapBackgroundColor); color:var(--sapTextColor); overflow:auto;
    font-family:var(--sapFontFamily,'72','Segoe UI',Arial,sans-serif); }
  .sec { border:1px solid var(--sapList_BorderColor,#e0e0e0); border-radius:8px; overflow:hidden;
    background:var(--sapList_Background,#fff); }
  .sec-hd { display:flex; align-items:center; justify-content:space-between; gap:8px;
    padding:9px 14px; border-bottom:1px solid var(--sapList_BorderColor,#e0e0e0);
    background:var(--sapList_HeaderBackground,#f5f6f7); }
  .sec-hd-l { display:flex; align-items:center; gap:8px; }
  .sec-hd-r { display:flex; gap:4px; align-items:center; }
  .sec-hd ui5-icon { color:var(--neo-cyan,var(--sapInformativeTextColor,#00b4d8)); font-size:14px; }
  .sec-t { margin:0; font-weight:600; color:var(--sapTitleColor); font-size:13px; }
  .sec-bd { padding:12px 14px; box-sizing:border-box; }
  .sec-head { flex:0 0 auto; }
  .sec-grid { flex:1 1 0; display:flex; flex-direction:column; min-height:120px; }
  .sec-grid .sec-bd { flex:1; min-height:0; padding:0; display:flex; flex-direction:column; }
  .tbl-wrap { flex:1; min-height:0; display:flex; flex-direction:column; }
  .tbl-wrap cmx-revo-grid { display:flex; width:100%; flex:1 1 0%; min-width:0; min-height:0; flex-direction:column; }
  cmx-toolbar { display:block; }
  /* 头表单分组：card（卡片分区）/ bar（色条+下分隔线），由 HEAD_GROUP_STYLE 控制 */
  /* 内紧外松：字段卡片由 neo 皮肤处理（紧凑），分组之间留 14px 形成阅读节奏 */
  .grp { margin-bottom:14px; }
  .grp:last-child { margin-bottom:0; }
  .grp-title { display:flex; align-items:center; gap:6px; font-weight:600; color:var(--sapTitleColor);
    font-size:12px; letter-spacing:.01em; }
  .grp-title ui5-icon { color:var(--neo-cyan,var(--sapInformativeTextColor,#00b4d8)); font-size:13px; }
  .grp-body { box-sizing:border-box; }
  .grp-card { border:1px solid var(--sapList_BorderColor,#e0e0e0); border-radius:8px; overflow:hidden;
    background:var(--sapList_Background,#fff); }
  .grp-card .grp-title { padding:8px 12px; background:var(--sapList_HeaderBackground,#f5f6f7);
    border-bottom:1px solid var(--sapList_BorderColor,#e0e0e0); }
  .grp-card .grp-body { padding:10px 12px; }
  .grp-bar .grp-title { padding:3px 0 3px 10px; border-left:3px solid var(--neo-cyan,#00b4d8); }
  .grp-bar .grp-body { padding:10px 0 8px; border-bottom:1px solid var(--sapList_BorderColor,#e0e0e0); }
  .step-bar { display:flex; align-items:center; gap:8px; padding:9px 14px;
    background:var(--sapList_Background,#fff); border:1px solid var(--sapList_BorderColor,#e0e0e0);
    border-radius:8px; font-size:12px; color:var(--sapContent_LabelColor); }
  .step-bar .step { display:flex; align-items:center; gap:5px; }
  .step-bar .step .num { display:inline-flex; align-items:center; justify-content:center;
    width:18px; height:18px; border-radius:50%; font-size:11px; font-weight:600;
    background:var(--neo-cyan,#00b4d8); color: #fff; }
  .step-bar .step.done .num { background:var(--sapSuccessBorderColor,#2b7c2b); }
  .step-bar .step.cur .num { background:var(--neo-cyan,#00b4d8); }
  .step-bar .step.pending .num { background:var(--sapNeutralBorderColor,#899191); }
  .step-bar .sep { color:var(--sapContent_DisabledTextColor); }
  .step-actions { display:flex; gap:6px; align-items:center; }
  .line-tabs { display:flex; gap:2px; flex-wrap:wrap; }
  .line-tab { padding:6px 14px; font-size:12px; cursor:pointer; border:1px solid var(--sapList_BorderColor,#e0e0e0);
    border-bottom:none; border-radius:6px 6px 0 0; background:var(--sapList_HeaderBackground,transparent);
    color:var(--sapContent_LabelColor); }
  .line-tab.active { background:var(--sapList_Background,#fff); color:var(--sapTitleColor); font-weight:600;
    border-bottom:1px solid var(--sapList_Background,#fff); position:relative; top:1px;
    box-shadow:inset 0 -2px 0 var(--neo-cyan,#00b4d8); }
  .loading { padding:40px; text-align:center; color:var(--sapContent_LabelColor); font-size:13px; }
  .load-err { padding:24px; color:var(--sapNegativeTextColor,#b00); font-size:13px; }
  /* 保存/提交进行中：页面顶部 indeterminate 细进度条（busy 滑块动画，色取 UI5 busy 变量派生） */
  .cr-busybar { position:sticky; top:0; z-index:6; height:3px; overflow:hidden;
    background:color-mix(in srgb, var(--sapContent_BusyColor,#0a6ed1) 16%, transparent); }
  .cr-busybar::before { content:''; display:block; width:36%; height:100%;
    background:var(--sapContent_BusyColor,#0a6ed1); border-radius:2px;
    animation:cr-busy-move 1.1s ease-in-out infinite; }
  @keyframes cr-busy-move { 0% { transform:translateX(-110%);} 55% { transform:translateX(190%);} 100% { transform:translateX(290%);} }
  /* 关键信息查重引导提示条 */
  .key-tip { display:flex; align-items:flex-start; gap:8px; margin-bottom:10px; padding:8px 12px;
    border-radius:6px; font-size:12px; line-height:1.5; color:var(--sapContent_LabelColor);
    background:color-mix(in srgb, var(--neo-cyan,#00b4d8) 8%, var(--sapList_Background,#fff));
    border:1px solid color-mix(in srgb, var(--neo-cyan,#00b4d8) 24%, transparent); }
  .key-tip ui5-icon { color:var(--neo-cyan,var(--sapInformativeTextColor,#00b4d8)); font-size:14px; flex-shrink:0; margin-top:1px; }
  /* view 模式左右分栏：左主内容 + 右固定操作区（操作按钮 + 流程占位） */
  .pg-view { flex-direction:row; align-items:stretch; overflow:hidden; }
  .pg-view .pg-main { flex:1; min-width:0; display:flex; flex-direction:column; gap:10px; overflow:auto; }
  .action-panel { flex:0 0 300px; display:flex; flex-direction:column; gap:10px; overflow:auto; } /* 300px 对齐门户属性面板默认宽（--property-width） */
  .ap-card { background:var(--sapList_Background,#fff); border:1px solid var(--sapList_BorderColor,#e0e0e0);
    border-radius:8px; padding:10px 12px; }
  .ap-title { font-size:13px; font-weight:600; color:var(--sapTitleColor); margin-bottom:8px; }
  .ap-actions { display:flex; flex-direction:column; gap:8px; }
  .ap-actions ui5-button { width:100%; }
  .ap-opinion { width:100%; }
  .ap-hint { font-size:12.5px; color:var(--sapContent_LabelColor); line-height:1.6; }
  .ap-btn-row { display:flex; gap:8px; }
  .ap-btn-row ui5-button { flex:1; }
  /* 流程轨迹：<cmx-flow-trail> 组件样式内置 shadow DOM（与待办属性面板同款），页内仅保留容器布局。 */
  .fh { display:flex; flex-direction:column; }
  `
}
const { escHtml: esc } = globalThis.__cmxDataComp // 共享转义（cmx-data-comp/lib/cmx-page-helpers.js；最严格五字符集合，文本/属性上下文皆安全）

function viewHtml() {
  if (state.loading) return `<div class="pg"><div class="loading">正在加载表单元数据…</div></div>`
  if (state.loadErr) return `<div class="pg"><div class="load-err">⚠ ${esc(state.loadErr)}</div></div>`
  const mode = state.mode
  const isView = mode === 'view'
  const isEdit = mode === 'update'
  const showSteps = mode === 'create' && state.keyDefs.length > 0 // 新增且配置了关键信息字段才显示步骤条（查重 → 完整信息）
  const step = state.step
  const stepBarHtml = showSteps ? `<div class="step-bar">
      <div class="step ${step >= 1 ? (step > 1 ? 'done' : 'cur') : 'pending'}"><span class="num">1</span><span>关键信息</span></div>
      <span class="sep">→</span>
      <div class="step ${step >= 2 ? 'cur' : 'pending'}"><span class="num">2</span><span>完整信息</span></div>
    </div>` : ''
  const domainLabel = state.dictMeta?.dictName || state.activation?.target_dict || '主数据'
  const modeLabel = isView ? '查看' : (isEdit ? '变更' : '新增')
  // view 模式标题带单据号
  const titleSuffix = (isView && state.crHead?.doc_no) ? `· ${esc(state.crHead.doc_no)}` : ''
  // 查重页（create step1）操作按钮留顶部 bar；create step2 / update / view 按钮统一移右侧 action-panel
  const isKeyStep = showSteps && step === 1
  const topActions = isKeyStep
    ? `<ui5-button design="Emphasized" icon="navigation-right-arrow" id="fNext">下一步</ui5-button>`
    : ''
  const keyFormCard = (showSteps && step === 1) ? `<div class="sec sec-head">
      <div class="sec-hd"><div class="sec-hd-l">
        <ui5-icon name="add-document" design="Default" mode="Decorative"></ui5-icon>
        <ui5-title level="H6" size="H6" wrapping-type="Normal" class="sec-t">关键信息（查重）</ui5-title>
      </div></div>
      <div class="sec-bd">
        <div class="key-tip"><ui5-icon name="message-information" mode="Decorative"></ui5-icon>
          <span>填写关键信息后点击「下一步」，系统将自动查重——若已存在相似记录会提示确认，避免重复录入。</span></div>
        <div id="fKeyForm"></div>
      </div>
    </div>` : ''
  const fullVisible = !showSteps || step === 2
  const headHtml = fullVisible ? `<div id="fHeadForms"></div>` : ''
  // 明细区：view 只读无增删行按钮；view 编辑态（editing）需要增删行
  const lineToolbar = (isView && !state.editing) ? '' : `<div class="sec-hd-r">
          <ui5-button design="Default" icon="add" id="fAddRow">增行</ui5-button>
          <ui5-button design="Transparent" icon="delete" id="fDelRow">删选中</ui5-button>
        </div>`
  const lineHtml = (fullVisible && state.lineDefs.length) ? `<div class="sec sec-grid" style="flex:1 1 auto;">
      <div class="sec-hd">
        <div class="sec-hd-l">
          <ui5-icon name="accounting-document-verification" design="Default" mode="Decorative"></ui5-icon>
          <ui5-title level="H6" size="H6" wrapping-type="Normal" class="sec-t">明细</ui5-title>
        </div>
        ${lineToolbar}
      </div>
      <div class="sec-bd">
        <div id="fLineTabs" class="line-tabs"></div>
        <div id="fLinePanels" style="flex:1;min-height:0;display:flex;flex-direction:column;"></div>
      </div>
    </div>` : ''
  // 顶部 bar 只放标题（+ 查重页的「下一步」）；操作按钮在右侧 action-panel（见末尾布局判定）
  const body = `<ui5-bar design="Header" accessible-role="Toolbar">
      <ui5-label wrapping-type="Normal" style="font-weight:600;font-size:15px;color:var(--sapShellTitleColor,var(--sapTitleColor));">${modeLabel}${esc(domainLabel)} ${titleSuffix}</ui5-label>
      <div slot="endContent" style="display:flex;gap:4px;">${topActions}</div>
    </ui5-bar>
    ${stepBarHtml}
    ${keyFormCard}
    ${headHtml}
    ${lineHtml}`
  // 查重页单栏（按钮在顶部 bar）；create step2 / update / view 左右分栏（按钮在右侧 action-panel）
  if (isKeyStep) {
    return `<div class="pg">${body}</div>`
  }
  return `<div class="pg pg-view"><div class="pg-main">${body}</div><aside class="action-panel">${actionPanelHtml()}</aside></div>`
}

// 右侧操作区：所有非查重页模式统一在此渲染操作按钮（create step2 / update / view 各状态）。
// 按钮 id 与 doSave/doCrAction 逻辑完全不变，仅 HTML 容器从顶部 bar 移到右侧。
// 流程轨迹卡（flowTrailHtml 组件）常驻已进流转单据的右栏——20260902 方案修订：取消右侧独立
// 属性面板，轨迹/状态进表单页；待办办理（flowApprove）与编辑态（含退回重办）都显示。
function actionPanelHtml() {
  const mode = state.mode
  const apCard = (title, inner) => `<div class="ap-card"><div class="ap-title">${title}</div><div class="ap-actions">${inner}</div></div>`
  // view 态一律显示轨迹卡：撤回回草稿的单据保留已终止轮次轨迹，全新草稿显示「提交后展示」占位；编辑态也保留。
  const showFlow = mode === 'view'
  const flow = showFlow ? flowTrailHtml() : ''
  // create step2：上一步 / 保存草稿 / 保存并提交
  if (mode === 'create' && state.step === 2) {
    return apCard('操作', `<ui5-button design="Transparent" icon="navigation-left-arrow" id="fPrev">上一步</ui5-button>
      <ui5-button design="Default" icon="save" id="fSave2">保存草稿</ui5-button>
      <ui5-button design="Emphasized" icon="paper-plane" id="fSubmit2">保存并提交</ui5-button>`) + flow
  }
  // update 变更：保存草稿 / 保存并提交
  if (mode === 'update') {
    return apCard('操作', `<ui5-button design="Default" icon="save" id="fSave">保存草稿</ui5-button>
      <ui5-button design="Emphasized" icon="paper-plane" id="fSubmit">保存并提交</ui5-button>`) + flow
  }
  // view 编辑态：保存 / 取消；退回重办（canApply）额外提供「保存并提交」——保存 + 办结发起人
  // 确认任务一气呵成，发起人改完直接推流程（等价于查看页的 编辑修改→保存→确认并继续）。
  if (mode === 'view' && state.editing) {
    const canApply = !!(state.reviewCtx && state.reviewCtx.canApply)
    const primary = canApply
      ? `<ui5-button design="Emphasized" icon="paper-plane" id="fConfirmSave">保存并提交</ui5-button>
      <ui5-button design="Default" icon="save" id="fEditSave">保存</ui5-button>`
      : `<ui5-button design="Emphasized" icon="save" id="fEditSave">保存</ui5-button>`
    return apCard('编辑', `${primary}
      <ui5-button design="Transparent" icon="cancel" id="fEditCancel">取消</ui5-button>`) + flow
  }
  // view 非编辑态：按 doc_status 显示对应操作按钮
  if (mode === 'view') {
    const st = state.crHead?.doc_status || ''
    let actions = ''
    // draft / rejected 都可编辑 + 重新提交；draft 额外可作废（rejected 已终止，无需作废）
    if (st === 'draft' || st === 'rejected') {
      actions = `<ui5-button design="Default" icon="edit" id="fEdit">编辑</ui5-button>
        <ui5-button design="Emphasized" icon="paper-plane" id="fCrSubmit">提交</ui5-button>`
      if (st === 'draft') actions += `<ui5-button design="Transparent" icon="cancel" id="fAbort">作废</ui5-button>`
    } else if (st === 'approving') {
      // M7.1：审批动作业务封装——审批人（通过/驳回/退回+意见）与发起人（撤回）按钮组。
      actions = reviewActionsHtml()
    } else if (st === 'activating') {
      actions = `<div class="ap-hint">激活中（流程回写进行中），请稍候刷新。</div>`
    }
    return (actions ? apCard('操作', actions) : '') + flow
  }
  return ''
}

// M7.1 流程操作按钮组（详情页 approving 态与待办打开的审批态共用）。
// 可见性由后端 review-context 判定（canReview=assignee∪候选；canWithdraw=发起人），
// 服务端再校验一道（review/withdraw 端点），前端显隐仅为交互优化。
function reviewActionsHtml() {
  const rc = state.reviewCtx || {}
  let html = ''
  if (rc.canReview) {
    html += `<ui5-textarea id="fOpinion" placeholder="审批意见（同意/驳回/退回均可附言）" rows="3" class="ap-opinion"></ui5-textarea>
      <div class="ap-btn-row">
        <ui5-button design="Emphasized" icon="accept" id="fReviewApprove">通过</ui5-button>
        <ui5-button design="Negative" icon="decline" id="fReviewReject">驳回</ui5-button>
      </div>
      <ui5-button design="Transparent" icon="undo" id="fReviewReturn">退回发起人</ui5-button>`
  }
  // 退回重办：apply 任务开放且当前用户（发起人）可办——编辑修改 + 确认继续走流程。
  if (rc.canApply) {
    html += `<div class="ap-btn-row">
        <ui5-button design="Default" icon="edit" id="fEdit">编辑修改</ui5-button>
        <ui5-button design="Emphasized" icon="accept" id="fConfirmApply">确认并继续</ui5-button>
      </div>`
  }
  if (rc.canWithdraw) {
    html += `<ui5-button design="Transparent" icon="undo" id="fWithdraw">撤回</ui5-button>`
  }
  if (!html) {
    // 无任何操作权限时的状态化提示：终态单据说"办结/驳回"，避免已办结还显示"审批中"。
    const st = state.crHead?.doc_status || ''
    const hint = st === 'activated' ? '流程已办结，单据已激活落字典，无可用操作。'
      : st === 'rejected' ? '审批已驳回，可编辑修改后重新提交。'
        : st === 'activating' ? '激活中（流程回写进行中），请稍候刷新。'
          : '审批中（流程），当前用户无操作权限；可在流程待办中心查看进度。'
    html = `<div class="ap-hint">${hint}</div>`
  }
  return html
}

// ── 流程轨迹数据装载（<cmx-flow-trail> 组件已上收 cmx-data-comp）─────────────
// 20260903 上收：组件定义移至 packages/cmx-data-comp/src/components/cmx-flow-trail.js，
// barrel 全局注册一次（native 页 Blob 模块不能 import 共享代码，故归库而非页面内联副本）；
// 本页只写 <cmx-flow-trail> 标签 + bind() 里回填 el.trail，办理人用户快照由组件内部兜底拉取。
// 数据源：/api/mdm/change-requests/flow-history（各实例+意见）+ /api/flow/instances/{id}
// （tokens/tasks 全量）+ /api/flow/definitions（节点轴）。
let ftDefCache = null
async function ftDefinition (key) {
  if (ftDefCache && ftDefCache[key]) return ftDefCache[key]
  try {
    const d = await apiGet('/api/flow/definitions')
    ftDefCache = {}
    for (const item of (d && d.definitions) || []) ftDefCache[item.key] = item
  } catch { ftDefCache = ftDefCache || {} }
  return (ftDefCache && ftDefCache[key]) || null
}

// 轨迹数据加载：flow-history（实例+意见，倒序）→ 只取最新一轮实例补全量 tokens/tasks。
// 与待办属性面板同口径：不拆历史轮次（驳回/撤回重提的旧实例审批意见不展示）。
// 失败降级为空数组（卡片显示暂无流转记录），不阻断表单。
async function ftLoad () {
  if (state.crId == null) { state.flowTrail = null; return }
  try {
    const fh = await apiGet(`/api/mdm/change-requests/flow-history?crId=${state.crId}`, state.dbId)
    const inst = ((fh && fh.instances) || [])[0]
    const trail = []
    if (inst && inst.instanceId) {
      const full = await apiGet(`/api/flow/instances/${encodeURIComponent(inst.instanceId)}`).catch(() => null)
      const definitionKey = (full && full.definitionKey) || 'mdm_cr_approval'
      const definition = await ftDefinition(definitionKey)
      trail.push({ instance: full, definition, comments: inst.comments || [] })
    }
    state.flowTrail = trail
  } catch (e) { console.warn('[cr-form] 流程轨迹加载失败', e); state.flowTrail = state.flowTrail || [] }
}

// 流程轨迹卡：单挂最新一轮实例的 <cmx-flow-trail>（纯时间线，无头部/摘要——表单页已有单据号与状态；卡标题保留）。
// 渲染是静态占位，数据在 bind() 里回填（el.trail = ...）。
function flowTrailHtml () {
  if (!state.flowTrail || !state.flowTrail.length) {
    return `<div class="ap-card"><div class="ap-title">流程轨迹</div>
      <cmx-empty-state icon="process" title="暂无流转记录" description="提交后此处展示流程进度与审批轨迹"></cmx-empty-state></div>`
  }
  return `<div class="ap-card"><div class="ap-title">流程轨迹</div><div class="fh"><cmx-flow-trail></cmx-flow-trail></div></div>`
}

// ── 元数据加载 ──────────────────────────────────────────────────────────────
async function loadActivation() {
  const list = (await apiGet('/api/mdm/activations', state.dbId)) || []
  const exact = list.find((a) => a.source_doc_type === state.docType && a.cr_type === state.crType)
  if (exact) return exact
  // update 渲染回退：若无 update 配置，复用同 docType 的 create 配置（头/明细字段映射一致，
  // 仅激活器搬运走不同分支）。注意：激活器激活 update CR 仍需单独配 cr_type=update 的配置。
  if (state.crType === 'update') {
    return list.find((a) => a.source_doc_type === state.docType && a.cr_type === 'create') || null
  }
  return null
}

const _dictMetaCache = {}
async function loadDictMeta(dictCode) {
  if (!dictCode) return null
  if (_dictMetaCache[dictCode]) return _dictMetaCache[dictCode]
  const m = await apiGet(`/api/dct/meta?${coordQs({ dict: dictCode })}&with_props=true`, state.dbId)
  const data = (m && m.columns) ? m : null
  _dictMetaCache[dictCode] = data
  return data
}

// 字典全量列 → CmxColumn[]（委托组件库标准管线 metaTableFieldsToColumns，含 refDict→dict-select /
// enumValues→select / required / 系统列只读 / editSettings.coord 等完整派生）。
function metaColumns(dictMeta) {
  const C = cmx()
  if (!C.metaTableFieldsToColumns || !dictMeta) return []
  const c = state.coord || {}
  return C.metaTableFieldsToColumns(dictMeta.columns || [], {
    kind: 'DCT',
    pk: dictMeta.pk, codeField: dictMeta.codeField, selfHierarchy: dictMeta.selfHierarchy,
    parentField: dictMeta.parentField, dictCode: dictMeta.dictCode, labelField: dictMeta.labelField,
    domain: c.domain, application: c.application, module: c.module,
  }, {
    respectOrder: true,
    coord: { domain: c.domain, application: c.application, module: c.module, ...(c.dbId ? { dbId: c.dbId } : {}) },
  })
}
// cv_mdm_apply 单据头列（doc/meta）——供「目标列留空的引用字段」取展示属性。这类字段不映射主数据列
// （header_mapping 值为 null），pickAndRename 在 dct 找不到，回退到 cv_mdm_apply 顶层列（doc_no/entity_id 等）。
async function loadDocHeadFields() {
  const c = state.coord || {}
  const docMeta = await apiGet(`/api/doc/meta?${coordQs({ file: `${c.application}_doc_meta_v1.json` })}`, state.dbId)
  const layers = (docMeta && docMeta.layers) || []
  return (layers.find((l) => l.tableName === 'cv_mdm_apply') || {}).columns || []
}
function docMetaColumns(fields) {
  const C = cmx()
  if (!C.metaTableFieldsToColumns || !fields || !fields.length) return []
  const c = state.coord || {}
  return C.metaTableFieldsToColumns(fields, 'DOC', {
    respectOrder: true,
    coord: { domain: c.domain, application: c.application, module: c.module, ...(c.dbId ? { dbId: c.dbId } : {}) },
  })
}

// 按 mapping（{srcField: tgtCol}）从全量列里筛 + 把 id 从 tgtCol 改成 srcField，保持 mapping 顺序。
// 直接改实例 id（不 spread）——spread 会丢 CmxColumn 原型链，setMembers 要求 CmxColumn 实例。
// 目标列留空（tgt=null）的「引用字段」按 srcField 回退：payload 同名业务字段（dct）→ cv_mdm_apply 顶层列（doc/meta）。
// 让「只在单据展示、不写主数据」的字段也进表单——否则 buildHeadForms 按 g.fields 匹配 headCols 时
// 整组匹配不上，分组被 continue 跳过而不渲染。
function pickAndRename(allCols, mapping, refCols) {
  const out = []
  const ref = (refCols && refCols.length) ? refCols : []
  for (const srcField of Object.keys(mapping || {})) {
    const tgtCol = mapping[srcField]
    let found = allCols.find((col) => col.id === tgtCol)
    if (!found && (tgtCol == null || tgtCol === '')) {
      found = allCols.find((col) => col.id === srcField) || ref.find((col) => col.id === srcField)
    }
    if (found) { found.id = srcField; out.push(found) }
  }
  return out
}

// 字段展示顺序的载体是 header_groups[].fields 数组（jsonb 数组保序），而非 header_mapping 的 key 序——
// header_mapping 经 serde Map（BTreeMap 字母序）+ PG jsonb（key 无序）落库后 key 序必丢。
// activation-mapper 保存时把全部字段按展示序写进各组 fields，未分组字段收 groupCode='__order__' 影子组。
// 此处按 fields 数组序重建 mapping（JS 对象 key 插入序 = 数组序），未覆盖的 key 追加尾部兜底。
const ORDER_GROUP_CODE = '__order__'
function orderedHeaderMapping(a) {
  const hm = a.header_mapping || {}
  const groups = Array.isArray(a.header_groups) ? a.header_groups : []
  const out = {}
  for (const g of groups) {
    for (const f of (Array.isArray(g.fields) ? g.fields : [])) {
      if (f && Object.prototype.hasOwnProperty.call(hm, f) && !Object.prototype.hasOwnProperty.call(out, f)) out[f] = hm[f]
    }
  }
  for (const k of Object.keys(hm)) { if (!Object.prototype.hasOwnProperty.call(out, k)) out[k] = hm[k] }
  return out
}
// 明细字段同理：fields 的 key 序落库已丢，按 fieldOrder 保序数组重排（旧数据无 fieldOrder 时原样兜底）。
function orderedLineFields(lm) {
  const fields = lm.fields || {}
  const order = Array.isArray(lm.fieldOrder) ? lm.fieldOrder : []
  const out = {}
  for (const f of order) {
    if (f && Object.prototype.hasOwnProperty.call(fields, f) && !Object.prototype.hasOwnProperty.call(out, f)) out[f] = fields[f]
  }
  for (const k of Object.keys(fields)) { if (!Object.prototype.hasOwnProperty.call(out, k)) out[k] = fields[k] }
  return out
}

// 解析 activation + 字典元数据 → headMap/headCols/lineDefs
async function buildFieldModel() {
  const C = cmx()
  const a = state.activation
  if (!a) { state.loadErr = `未找到激活映射配置（source_doc_type=${state.docType}, cr_type=${state.crType}）。请在「激活映射配置器」配置后重试。`; return }
  if (typeof C.metaTableFieldsToColumns !== 'function') { state.loadErr = '组件库版本过低（缺少 metaTableFieldsToColumns），请构建最新 cmx-data-comp。'; return }
  state.dictMeta = await loadDictMeta(a.target_dict)
  if (!state.dictMeta) { state.loadErr = `目标字典元数据加载失败：${a.target_dict}`; return }
  // 头表：全量列派生 → 按 header_mapping 筛 + 改名（顺序按 header_groups[].fields 数组序重建，见上注释）
  const headAll = metaColumns(state.dictMeta)
  const refCols = docMetaColumns(await loadDocHeadFields())
  const orderedHm = orderedHeaderMapping(a)
  state.headMap = Object.keys(orderedHm).map((src) => [src, orderedHm[src]])
  state.headCols = pickAndRename(headAll, orderedHm, refCols)
  // 快照每列原始 edit.mode，供 buildHeadForms 在 view↔editing 切换时正确恢复
  // （否则反复设/清 readonly 会污染共享 headCols，导致编辑态仍只读或系统列被误解锁）
  state.headCols.forEach((c) => { c._origEditMode = c.edit ? c.edit.mode : undefined })
  // 主体名 key：header_mapping 里 value === subject_name_field（目标列名）的那个 key
  const subjField = a.subject_name_field || state.dictMeta.labelField || ''
  state.nameFieldKey = ''
  for (const [src, tgt] of state.headMap) {
    if (tgt === subjField) { state.nameFieldKey = src; break }
  }
  if (!state.nameFieldKey && state.dictMeta.labelField) {
    // 兜底：取目标字典 labelField 对应列
    const fall = state.headMap.find(([src, tgt]) => tgt === state.dictMeta.labelField)
    if (fall) state.nameFieldKey = fall[0]
  }
  const nameCol = state.headCols.find((col) => col.id === state.nameFieldKey)
  state.nameCaption = nameCol ? (nameCol.caption || subjField) : (subjField || '名称')
  // 关键信息字段 key_fields → keyDefs：{src 源字段, tgt 目标列, weight, kind, dedup}。
  // field 是目标列名，须反查 header_mapping 找到 CR 侧源字段（映射不到的配置项丢弃——表单无处填）。
  // dedup=false 仅进步骤①采集展示，不进查重请求（关键信息 ≠ 全部查重）。
  // keyDefs 完全等于配置（不强制补主体名）：**未配置则无步骤①**——create 直接进完整表单，
  // 不做查重，主体名作为普通字段在步骤②采集（required 由列元数据兜底）。
  const kfCfg = Array.isArray(a.key_fields) ? a.key_fields.filter((k) => k && k.field) : []
  const defs = []
  for (const k of kfCfg) {
    const e = state.headMap.find(([, tgt]) => tgt === k.field)
    if (!e) continue
    defs.push({ src: e[0], tgt: k.field, weight: Number(k.weight) || 100, kind: k.kind || 'EditDistance', dedup: k.dedup !== false })
  }
  state.keyDefs = defs
  if (state.mode === 'create' && !defs.length) state.step = 2
  // 影子组（__order__）仅是未分组字段顺序的载体，不作分组渲染——剥除后其字段落入「其他」组/散列
  state.headerGroups = (a.header_groups || []).filter((g) => (g.groupCode || g.group_code || '') !== ORDER_GROUP_CODE)
  // 明细：先用局部数组构建，全部就绪后一次性赋值给 state.lineDefs。
  // 不能边循环边 push 进 state.lineDefs——for 体内有 await loadDictMeta，会让出执行权；
  // 若刷新触发第二次 init 并发，两次 push 会交织，导致明细从 2 个翻倍成 4 个。
  const lineDefs = []
  for (const lmRaw of (a.line_mappings || [])) {
    const lm = normLineMapping(lmRaw)
    const meta = await loadDictMeta(lm.targetDict)
    const all = meta ? metaColumns(meta) : []
    // fields 的 key 序落库已丢（同头表），按 fieldOrder 数组序重建
    const orderedFields = orderedLineFields(lm)
    const map = Object.keys(orderedFields).map((src) => [src, orderedFields[src]])
    const cols = pickAndRename(all, orderedFields)
    // 明细列宽按比例转百分比：字典元数据固定 px 宽总和（如银行账户 3 列 ≈460px）在窄容器
    // （分栏/窄窗，主区 < 列宽总和）下 revo-grid stretch 对纯 px 列「保持原宽 → 横向滚动」，
    // 撑出页面底部横向滚动条。转百分比后 stretch 的百分比列机制随任意容器宽按比例收缩。
    {
      const total = cols.reduce((s, c) => s + (parseFloat(c.width) || 120), 0)
      if (total > 0) cols.forEach((c) => {
        const base = parseFloat(c.width) || 120
        c.width = ((base / total) * 100).toFixed(2) + '%'
      })
    }
    lineDefs.push({
      lineType: lm.lineType, targetDict: lm.targetDict,
      targetTable: lm.targetTable, parentIdField: lm.parentIdField,
      meta, map, cols,
    })
  }
  state.lineDefs = lineDefs
}

// update 变更模式：按 targetId 加载目标字典的头记录 + 各明细类型记录，供表单预填。
// 元数据驱动——target_dict / lineDef.targetDict / parentIdField 全来自 activation 配置，不写死任何主数据表名，
// 支撑未来新增其他主数据（客户/物料/组织…）复用同一页面。头与明细并发加载；每个 await 后用 stale() 守卫防刷新并发。
async function loadTargetData(tok, targetId) {
  const stale = () => tok !== initToken
  const a = state.activation
  const targetDict = a && a.target_dict
  if (targetDict) {
    const headRes = await apiPost(`/api/dct/data/search?${coordQs({ dict: targetDict })}`, { filters: { id: targetId }, pageSize: 1 }, state.dbId)
    if (stale()) return
    state.target = (headRes && headRes.rows && headRes.rows[0]) || null
  }
  // 各明细按 parentIdField 过滤（外键 = 头记录 id），并发加载
  const lineTasks = state.lineDefs.map((lm) => {
    if (!lm.targetDict) return Promise.resolve([lm.lineType, []])
    return apiPost(`/api/dct/data/search?${coordQs({ dict: lm.targetDict })}`, { filters: { [lm.parentIdField]: targetId }, pageSize: 500 }, state.dbId)
      .then((r) => [lm.lineType, (r && r.rows) || []])
  })
  const results = await Promise.all(lineTasks)
  if (stale()) return
  const targetLines = {}
  for (const [lineType, rows] of results) targetLines[lineType] = rows
  state.targetLines = targetLines
}

function normLineMapping(lm) {
  return {
    lineType: lm.lineType || lm.line_type || '',
    targetDict: lm.targetDict || lm.target_dict || '',
    targetTable: lm.targetTable || lm.target_table || '',
    parentIdField: lm.parentIdField || lm.parent_field || lm.parentId_field || '',
    fields: lm.fields || {},
    fieldOrder: Array.isArray(lm.fieldOrder) ? lm.fieldOrder : [],
  }
}

// ── 表单构建 ────────────────────────────────────────────────────────────────
let keyForm = null
const headForms = [] // 分组多卡片，每卡片一个 cmx-ui5-form
const lineGrids = [] // 每明细 tab 一个 cmx-revo-grid
let activeLineIdx = 0
let lineSeq = 0

// 关键信息表单：按 keyDefs 顺序渲染全部查重字段（多字段加权查重），初始值从 keyVals 回填。
function buildKeyForm() {
  const C = cmx(); const wrap = q('fKeyForm'); if (!wrap || !state.keyDefs.length) return
  wrap.innerHTML = ''
  const form = document.createElement('cmx-ui5-form'); form.classList.add('cmx-form-neo')
  if (C.CmxColumnModel) {
    const cm = new C.CmxColumnModel({ datasetId: 'crKey' })
    // step1 查重输入必须可编辑：克隆列实例（保留原型），按 _origEditMode 恢复，不污染 headCols
    const members = state.keyDefs
      .map((d) => state.headCols.find((c) => c.id === d.src)).filter(Boolean)
      .map((col) => {
        const m = Object.assign(Object.create(Object.getPrototypeOf(col)), col, { edit: { ...(col.edit || {}) } })
        if (col._origEditMode !== 'readonly' && m.edit.mode === 'readonly') delete m.edit.mode
        return m
      })
    cm.setMembers(members)
    form.setColumnModel(cm)
  }
  form.setLayout?.('S1 M1 L2 XL2')
  const init = {}
  state.keyDefs.forEach((d) => { init[d.src] = state.keyVals[d.src] || '' })
  form.setDataSet?.(init)
  wrap.appendChild(form); keyForm = form
}

// 头表单：单个 cmx-ui5-form，分组用 CmxColumnGroup（列模型语义）+ form.setGroupStyle(HEAD_GROUP_STYLE)。
// cmx-ui5-form 内部按 groupStyle 渲染：每分组独立 ui5-form，标题用 ui5-form::part(header)（CSS 可控 card/bar）。
function buildHeadForms() {
  const C = cmx(); const wrap = q('fHeadForms'); if (!wrap) return
  wrap.innerHTML = ''; headForms.length = 0
  const isEdit = state.mode === 'update'
  // view 只读（editing=true 时解锁为可编辑，用于草稿编辑态）
  const isView = state.mode === 'view' && !state.editing
  // 列只读处理：基于 _origEditMode 重置，避免 view↔editing 切换时 readonly 残留 / 系统列被误解锁。
  // 系统列（_origEditMode=readonly）恒只读；view 全只读；create 步骤2 关键信息字段（已查重）只读回显。
  // 用副本计算只读，不写回共享的 state.headCols：buildKeyForm 复用关键信息列，
  // 若在此改 c.edit.mode 会污染 headCols，导致回 step1 输入框只读、view↔editing 切换残留只读。
  const cols = state.headCols.map((c) => {
    const forceRo = c._origEditMode === 'readonly' || isView || (!isEdit && state.keyDefs.some((d) => d.src === c.id)) || SYS_HEAD_FIELDS.has(c.id)
    // 克隆列实例（保留 CmxColumn 原型 → toDescriptor 可用），改副本 edit.mode 不污染共享 headCols
    const cc = Object.assign(Object.create(Object.getPrototypeOf(c)), c, { edit: { ...(c.edit || {}) } })
    if (forceRo) cc.edit.mode = 'readonly'
    else if (cc.edit.mode === 'readonly') delete cc.edit.mode
    return cc
  })
  // 按 header_groups 包成 CmxColumnGroup；未归组字段：有分组配置时包「其他」组，无分组配置时散列
  const used = new Set()
  const members = []
  for (const g of state.headerGroups) {
    const items = cols.filter((c) => (g.fields || []).includes(c.id) && !used.has(c.id))
    items.forEach((c) => used.add(c.id))
    if (!items.length) continue
    if (C.CmxColumnGroup) members.push(new C.CmxColumnGroup({ caption: g.groupName || g.groupCode || '分组', members: items }))
    else members.push(...items)
  }
  const ungrouped = cols.filter((c) => !used.has(c.id))
  if (ungrouped.length) {
    if (C.CmxColumnGroup && state.headerGroups.length) members.push(new C.CmxColumnGroup({ caption: '其他', members: ungrouped }))
    else members.push(...ungrouped)
  }
  const form = document.createElement('cmx-ui5-form'); form.classList.add('cmx-form-neo')
  if (C.CmxColumnModel) {
    const cm = new C.CmxColumnModel({ datasetId: 'crHead' })
    cm.setMembers(members)
    form.setColumnModel(cm)
  }
  form.setGroupStyle?.(HEAD_GROUP_STYLE) // card/bar 由前端常量控制
  form.setLayout?.('S1 M2 L3 XL3')
  const ds = {}
  for (const c of cols) ds[c.id] = headInitialValue(c.id)
  form.setDataSet?.(ds)
  wrap.appendChild(form)
  headForms.push(form)
}

// 头字段初始值：
//   view：从 CR 头回填（subject_name 顶层 + payload[srcField] 下沉）
//   update：从 target 字典记录回填（按 tgtCol 取，兼容扁平/payload）
//   create 步骤2：关键信息字段从步骤1 缓存的 keyVals 回显
//   doc_status：始终显示中文（系统管理，不参与收集）
function headInitialValue(srcField) {
  // 单据状态：显示中文（view 取实际状态，create/update 新建为 draft）
  if (srcField === 'doc_status') {
    const raw = state.mode === 'view' ? (state.crHead?.doc_status || 'draft') : 'draft'
    return STATUS_LABEL[raw] || raw
  }
  const mode = state.mode
  const entry = state.headMap.find(([s]) => s === srcField)
  const tgtCol = entry ? entry[1] : srcField
  if (mode === 'view') {
    const cr = state.crHead || {}
    if (srcField === state.nameFieldKey) return cr.subject_name != null ? String(cr.subject_name) : ''
    // 引用字段（目标列留空，如 doc_no/doc_status）展示单据头顶层列——它们不在 payload 里
    if ((tgtCol == null || tgtCol === '') && cr[srcField] != null) return String(cr[srcField])
    const p = cr.payload || {}
    return p[srcField] != null ? String(p[srcField]) : ''
  }
  if (mode === 'update') {
    // 单据字段（目标列留空）：update 本质是新建变更单，doc_date 回填今天，其余空（remark 用户填，doc_no 铸号）
    if (tgtCol == null || tgtCol === '') {
      if (srcField === 'doc_date') return todayStr()
      return ''
    }
    // 业务字段：从 target（cm_* 主数据）回填，兼容扁平/payload
    const t = state.target || {}
    const v = t[tgtCol] != null ? t[tgtCol] : (t.payload && t.payload[tgtCol]) != null ? t.payload[tgtCol] : ''
    return v != null ? String(v) : ''
  }
  if (state.keyDefs.some((d) => d.src === srcField)) return state.keyVals[srcField] != null ? String(state.keyVals[srcField]) : ''
  // 单据字段 doc_date 默认今天（与 update 一致；base 同样以今天占位）
  if (srcField === 'doc_date') return todayStr()
  return ''
}

// 明细多 tab 渲染
function buildLineGrids() {
  const tabsHost = q('fLineTabs'); const panelsHost = q('fLinePanels')
  if (!tabsHost || !panelsHost || !state.lineDefs.length) return
  tabsHost.innerHTML = ''; panelsHost.innerHTML = ''; lineGrids.length = 0
  const C = cmx()
  const multi = state.lineDefs.length > 1
  state.lineDefs.forEach((lm, idx) => {
    if (multi) {
      const tab = document.createElement('span')
      tab.className = 'line-tab' + (idx === activeLineIdx ? ' active' : '')
      tab.textContent = lm.meta?.dictName || lm.lineType || `明细${idx + 1}`
      tab.dataset.idx = String(idx)
      tab.addEventListener('click', () => { activeLineIdx = idx; refreshLineTabsActive(); showLinePanel(idx) })
      tabsHost.appendChild(tab)
    }
    const panel = document.createElement('div')
    panel.className = 'tbl-wrap'; panel.dataset.idx = String(idx)
    panel.style.display = idx === activeLineIdx ? 'flex' : 'none'
    const grid = document.createElement('cmx-revo-grid')
    grid.setAttribute('data-cmx-fill-height', '')
    grid.setAttribute('data-cmx-options', '{"editable":true,"showTotals":false,"showRequiredMark":false}')
    grid.classList.add('cmx-grid-neo')
    panel.appendChild(grid); panelsHost.appendChild(panel)
    if (C.CmxColumnModel && lm.cols.length) {
      const cm = new C.CmxColumnModel({ datasetId: 'crLine_' + idx })
      cm.setMembers(lm.cols)
      grid.setColumnModel(cm)
    }
    const readonlyGrid = state.mode === 'view' && !state.editing
    grid.setOptions?.({ editable: !readonlyGrid, fillHeight: true, showRowIndex: true, selectionMode: readonlyGrid ? 'none' : 'multi', showTotals: false })
    const fill = () => {
      const rows = lineSeedRows(lm)
      if (!rows.length) { grid.refreshLayout?.(); return }
      if (C.CmxDataSet) { const ds = new C.CmxDataSet({}); ds.setRows(rows); grid.setDataSet(ds) }
      else grid.setDataSet?.(rows)
      grid.refreshLayout?.()
    }
    requestAnimationFrame(() => requestAnimationFrame(fill))
    lineGrids.push(grid)
  })
}
function refreshLineTabsActive() {
  const tabsHost = q('fLineTabs'); if (!tabsHost) return
  tabsHost.querySelectorAll('.line-tab').forEach((t) => { t.classList.toggle('active', +t.dataset.idx === activeLineIdx) })
}
function showLinePanel(idx) {
  const panelsHost = q('fLinePanels'); if (!panelsHost) return
  panelsHost.querySelectorAll('.tbl-wrap').forEach((p) => { p.style.display = +p.dataset.idx === idx ? 'flex' : 'none' })
}
// 新行结构：按 lineDef 的 srcField 生成空值
function newLineRow(lm) {
  lineSeq += 1
  const r = { id: `nl_${Date.now()}_${lineSeq}` }
  for (const [src] of lm.map) r[src] = ''
  return r
}
// 明细 grid 初始行：view 模式从 CR.lines 按 line_type 预填（line_payload → srcField）；
// 其余模式给一行空行待录。
function lineSeedRows(lm) {
  if (state.mode === 'view') {
    const crLines = (state.crLines || []).filter((l) => l.line_type === lm.lineType)
    if (!crLines.length) return []
    return crLines.map((l) => {
      lineSeq += 1
      const row = { id: l.id || `cr_${Date.now()}_${lineSeq}`, _savedId: l.id, lineTargetId: l.line_target_id ?? null }
      const p = (l.line_payload && typeof l.line_payload === 'object') ? l.line_payload : {}
      for (const [src] of lm.map) row[src] = p[src] != null ? String(p[src]) : ''
      return row
    })
  }
  // update 变更：从字典加载的明细行预填（目标列 tgt → 源字段 src），并保留 cm_* 明细 id 到
  // lineTargetId（激活器 diff 用：有 id=update，无 id=insert，cm_* 有但 CR 没=软删）。
  // 合成 id 无 _savedId → 保存走 insert（CR 单据统一新增 cv_mdm_apply_line，带 line_target_id）。
  if (state.mode === 'update') {
    const rows = ((state.targetLines || {})[lm.lineType] || []).map((r, i) => {
      lineSeq += 1
      const row = { id: `tg_${state.targetId}_${lm.lineType}_${i}`, lineTargetId: r.id }
      for (const [src, tgt] of lm.map) row[src] = r[tgt] != null ? String(r[tgt]) : ''
      return row
    })
    return rows.length ? rows : [newLineRow(lm)]
  }
  return [newLineRow(lm)]
}

// ── 查重 ────────────────────────────────────────────────────────────────────
// 多字段加权查重：仅 dedup !== false 的字段进请求（keyValue 按目标列名发，后端在目标表
// 空间比较；specs/clusterKeys 由 keyDefs 生成，行序 = 簇键优先级）。后端 /mdm/check-key
// 构造虚拟 target 与已发布记录比对，综合分 ≥80 即 exists=true 阻断；空值字段安全
// （blocking 孤儿块 / compare 跳过）。
async function checkKey(keyValue) {
  const a = state.activation
  const dd = state.keyDefs.filter((d) => d.dedup)
  const specs = dd.map((d) => ({ field: d.tgt, weight: d.weight, kind: d.kind }))
  const clusterKeys = [...new Set(dd.map((d) => d.tgt))]
  return apiPost('/api/mdm/check-key', {
    dictCode: a.target_dict, targetTable: a.target_table,
    keyValue, specs, clusterKeys,
  }, state.dbId)
}
function goStep(n) { state.deletedLineIds = []; state.step = n; refresh() }
async function onNext() {
  const C = cmx()
  const row = (keyForm && keyForm.getData && keyForm.getData()) || {}
  // 主体名只在它进了步骤①表单时必填；未配置进关键信息的字段由步骤②列元数据 required 兜底
  if (state.keyDefs.some((d) => d.src === state.nameFieldKey)) {
    const name = (row[state.nameFieldKey] || '').trim()
    if (!name) { C.cmxWarn?.(`${state.nameCaption}不能为空`); return }
  }
  // 收集全部关键信息字段值：keyValue（仅查重字段，目标列名空间，空值不发）
  // + keyVals（全部字段，源字段空间，步骤2 回显）
  const keyValue = {}
  const keyVals = {}
  for (const d of state.keyDefs) {
    const v = (row[d.src] != null ? String(row[d.src]) : '').trim()
    keyVals[d.src] = v
    if (d.dedup && v) keyValue[d.tgt] = v
  }
  // 全部字段都不参与查重（dedup=false）→ 跳过查重直接进步骤2
  if (!state.keyDefs.some((d) => d.dedup)) { state.keyVals = keyVals; goStep(2); return }
  try {
    const d = await checkKey(keyValue)
    if (d && d.exists) {
      C.cmxError?.(d.message || `已存在相似记录（id=${d.id ?? ''}${d.code ? '，code=' + d.code : ''}），请确认是否继续`)
      return
    }
    state.keyVals = keyVals; goStep(2)
  } catch (e) {
    C.cmxError?.(`查重失败：${e.message}`)
  }
}

// ── 收集 / 保存 ─────────────────────────────────────────────────────────────
const DOC_DEF = { domain: 'basic', application: 'dataplatform', module: 'mdm', file: 'dataplatform_doc_meta_v1.json' }
const TABLE_NAMES = ['cv_mdm_apply', 'cv_mdm_apply_line']
const HEAD_TID = 't1'
function todayStr() { const d = new Date(); const z = (n) => String(n).padStart(2, '0'); return `${d.getFullYear()}-${z(d.getMonth() + 1)}-${z(d.getDate())}` }

// 合并所有头表单 getData
function collectHeadData() {
  const merged = {}
  for (const form of headForms) {
    const row = (form && form.getData && form.getData()) || {}
    Object.assign(merged, row)
  }
  return merged
}

// 构造头表 fields。nameFieldKey 值 → subject_name；
// header_mapping 中 value=null 的「单据字段」(doc_no/remark/doc_date/doc_status 等) → cv_mdm_apply 顶层列；
// value 非空的「业务字段」→ payload。
function buildHead() {
  const data = collectHeadData()
  const isEdit = state.mode === 'update'
  const a = state.activation
  const name = (data[state.nameFieldKey] != null ? String(data[state.nameFieldKey]) : '').trim()
  const payload = {}
  // 新建默认值只随 insert 落库；更新已有单据不带这些列——doc_status 归状态机管，
  // 编辑保存把 approving/rejected 单覆盖成 draft 会造成「draft+活实例」脱节
  // （退回单编辑保存即触发：confirm-apply 状态校验失败、再提交被防孤儿拦截）。doc_date 同理不重置。
  const base = state.savedCrId != null ? {} : { line_no: 1, doc_status: 'draft', doc_type_id: 1, doc_date: todayStr(), entity_id: 1 }
  for (const [src, tgt] of state.headMap) {
    if (src === state.nameFieldKey) continue
    if (SYS_HEAD_FIELDS.has(src)) continue  // 系统字段（状态/单据号）不收集，由状态机/铸号管理
    const v = data[src] != null ? data[src] : ''
    if (tgt == null || tgt === '') {
      // 单据字段：写 cv_mdm_apply 顶层列（有值才写，避免覆盖 base 默认）
      if (v !== '' && v != null) base[src] = v
    } else {
      // 业务字段：进 payload
      payload[src] = v
    }
  }
  if (isEdit) {
    const t = state.target || {}
    const deltas = {}
    for (const [src, tgt] of state.headMap) {
      if (src === state.nameFieldKey) continue
      if (tgt == null || tgt === '') continue  // 单据字段不进 field_deltas（主数据变更追踪只记业务字段）
      const oldV = (t[tgt] != null ? t[tgt] : (t.payload && t.payload[tgt]) != null ? t.payload[tgt] : '')
      const cur = (data[src] != null ? data[src] : '')
      if (String(cur) !== String(oldV)) deltas[src] = { old: oldV, new: cur }
    }
    const oldName = (t[a.subject_name_field] != null ? t[a.subject_name_field] : (t.name != null ? t.name : ''))
    if (name !== String(oldName).trim()) deltas['subject_name'] = { old: oldName, new: name }
    return { ...base, doc_type: state.docType, cr_type: state.crType, target_dict_code: a.target_dict,
      target_record_id: Number(t.id), subject_name: name, payload, field_deltas: deltas }
  }
  return { ...base, doc_type: state.docType, cr_type: state.crType, target_dict_code: a.target_dict,
    subject_name: name, payload }
}

// 收拢未提交的行内编辑（仿 data-editor）：用户在明细单元格输入后直接点保存时，
// 编辑值仍停留在 editor 组件、未写回行数据。dispatch change + blur 触发 revo-grid flush，等两帧后保存。
function commitGridEdits(cb) {
  try {
    const deepActive = (r) => { const a = r && r.activeElement; if (a && a.shadowRoot && a.shadowRoot.activeElement) return deepActive(a.shadowRoot); return a }
    const ae = deepActive(document)
    if (ae && ae !== document.body) {
      try { ae.dispatchEvent(new Event('change', { bubbles: true })) } catch (_) {}
      if (typeof ae.blur === 'function') { try { ae.blur() } catch (_) {} }
    }
  } catch (_) {}
  requestAnimationFrame(() => requestAnimationFrame(() => { try { cb() } catch (e) { console.error('[cr-form] commitGridEdits cb fail', e) } }))
}
// 取明细 grid 行：优先 getSource（含最新编辑），回退 DataSet.toPlainRows/getRows。
function lineRows(grid) {
  if (grid && typeof grid.getSource === 'function') { const s = grid.getSource(); if (Array.isArray(s)) return s }
  const ds = grid && grid.getDataSet ? grid.getDataSet() : null
  return ds ? (ds.toPlainRows ? ds.toPlainRows() : (ds.getRows ? ds.getRows() : [])) : []
}

// 收集所有明细 tab 行为 changeset
function collectLines() {
  const inserted = []; const updated = []
  state.lineDefs.forEach((lm, idx) => {
    const grid = lineGrids[idx]; if (!grid) return
    const rows = lineRows(grid)
    rows.forEach((r, i) => {
      const hasVal = lm.map.some(([src]) => r[src] != null && String(r[src]).trim() !== '')
      if (!hasVal) return
      const payload = {}
      for (const [src] of lm.map) payload[src] = r[src] != null ? r[src] : ''
      const upperId = state.savedCrId != null ? state.savedCrId : HEAD_TID
      if (r._savedId != null) {
        updated.push({ id: r._savedId, fields: { line_no: i + 1, line_payload: payload, line_target_id: r.lineTargetId ?? null } })
      } else {
        inserted.push({ id: r.id, upper_id: upperId, line_no: i + 1, fields: {
          line_type: lm.lineType, line_action: 'insert', line_payload: payload,
          line_target_id: r.lineTargetId ?? null,
        } })
      }
    })
  })
  // 被删的已入库行（按 _savedId 记录）。仿 cmx-doc merge：deleted 为行主键 id 数组，后端 DELETE WHERE id=ANY。
  return { inserted, updated, deleted: [...state.deletedLineIds] }
}

// 写操作按钮（保存/提交/作废等）：busy 时全部禁用，避免请求在途期间换按钮继续触发。
const WRITE_BTN_IDS = ['fSave', 'fSubmit', 'fSave2', 'fSubmit2', 'fEditSave', 'fCrSubmit', 'fAbort', 'fConfirmApply', 'fConfirmSave']
/**
 * 写操作 busy 态开关：按钮禁用 + 文案加「…」+ 页面顶部 indeterminate 进度条。
 * off 时对已被 refresh 重建（脱离文档）的按钮跳过恢复——重建后的按钮本就是初始可用态。
 */
function setWriteBusy(on) {
  if (!rootEl) return
  for (const id of WRITE_BTN_IDS) {
    const b = rootEl.querySelector('#' + id)
    if (!b) continue
    if (on) {
      if (b.disabled) continue
      b.dataset.busyLabel = b.textContent
      b.disabled = true
      b.textContent = `${b.dataset.busyLabel}…`
    } else if (b.dataset.busyLabel != null) {
      if (b.isConnected) { b.disabled = false; b.textContent = b.dataset.busyLabel }
      delete b.dataset.busyLabel
    }
  }
  const pg = rootEl.querySelector('.pg')
  const bar = rootEl.querySelector('.cr-busybar')
  if (on && pg && !bar) pg.insertAdjacentHTML('afterbegin', '<div class="cr-busybar" aria-hidden="true"></div>')
  else if (!on && bar) bar.remove()
}

function doSave(submit) {
  const C = cmx()
  if (state.saving) { C.cmxWarn?.('正在保存/提交中，请稍候'); return false }
  const data0 = collectHeadData()
  const nameVal = (data0[state.nameFieldKey] != null ? String(data0[state.nameFieldKey]) : '').trim()
  if (!nameVal) { C.cmxWarn?.(`${state.nameCaption}不能为空`); return false }
  if (typeof C.saveDocData !== 'function') { C.cmxError?.('组件库未加载，无法保存'); return false }
  // 互斥锁在 commitGridEdits 的 rAF 窗口前置位：连点只放行第一次，其余在入口即被拒。
  state.saving = true
  setWriteBusy(true)
  // 返回 Promise<boolean>（保存成功与否）：「保存并提交」按钮据此决定是否接着办结发起人确认任务；
  // 其余调用方不取返回值，行为不变。
  return new Promise((resolve) => {
  // 先收拢未提交的明细行内编辑（失焦/派发 change 触发 revo-grid flush），再构造 changeset 保存
  commitGridEdits(async () => {
    try {
      // 保存并提交（submit=true）：前置确认——提交后进入审批流，本页不再可改。
      // 保存草稿（submit=false）为高频暂存，不加确认以免反复打断录入。
      if (submit) {
        const ok = await C.cmxConfirm?.({ title: '保存并提交', message: '确认保存并提交审批？提交后进入审批流程，单据内容将无法在此页继续修改。', danger: false })
        if (ok === false) { resolve(false); return }
      }
      const changes = {}
      if (state.savedCrId != null) {
        changes.cv_mdm_apply = { updated: [{ id: state.savedCrId, fields: buildHead() }] }
      } else {
        changes.cv_mdm_apply = { inserted: [{ id: HEAD_TID, fields: buildHead() }] }
      }
      const { inserted: lineIns, updated: lineUpd, deleted: lineDel } = collectLines()
      const lineChanges = {}
      if (lineIns.length) lineChanges.inserted = lineIns
      if (lineUpd.length) lineChanges.updated = lineUpd
      if (lineDel.length) lineChanges.deleted = lineDel
      if (lineIns.length || lineUpd.length || lineDel.length) changes[TABLE_NAMES[1]] = lineChanges
      try {
        const data = await C.saveDocData(null,
          { ...DOC_DEF, dbId: state.dbId },
          { saveMode: 'merge', changes, tableNames: TABLE_NAMES,
            // 单据字段铸号规则覆盖：activation 配置的 doc_code_rules 覆盖单据元数据 codeRule
            // （激活配置优先）。state.activation 在 init 时按 docType+crType 加载。
            codeRuleOverrides: (state.activation && state.activation.doc_code_rules) || undefined })
        const idMap = (data && data.idMap) || {}
        const isFirstSave = state.savedCrId == null
        if (isFirstSave && idMap[HEAD_TID] != null) state.savedCrId = idMap[HEAD_TID]
        if (lineIns.length) syncSavedLineIds(idMap)
        // 删行已落库（cmx-doc merge deleted 已执行），清空本次记录，避免下次保存重复删
        state.deletedLineIds = []
        const crId = state.savedCrId
        if (submit && crId != null) {
          await apiPost('/api/mdm/change-requests/submit', { crId }, state.dbId)
        }
        showCmxToast(submit ? `变更申请 ${crId} 已提交审批` : (isFirstSave ? `已创建变更申请 ${crId}（草稿）` : `变更申请 ${crId} 已更新`))
        // 回显后端铸号 doc_no：草稿保存不 refresh（保留用户输入继续编辑），拉详情把 doc_no 写进对应单元格；
        // view 草稿编辑态走下方 refresh 重建，由 headInitialValue 顶层列回退统一回显（二者互补不重复）。
        if (!submit && crId != null) {
          try {
            const detail = await apiGet(`/api/mdm/change-requests/detail?crId=${crId}`, state.dbId)
            state.crHead = (detail && detail.head) || {}
            // view 草稿编辑态保存成功后，同步刷新明细行（detail 现已返回 line 真实 id），
            // 使紧随的 refresh() 从最新 crLines 重建 grid：既有行带 _savedId（再编辑走 update），
            // 编辑期间新增的行也回写真实 id 而非退化成合成 id（否则下次保存又被当 insert）。
            if (state.mode === 'view') state.crLines = (detail && detail.lines) || state.crLines
            const docNo = state.crHead.doc_no
            if (docNo != null && docNo !== '' && state.mode !== 'view') {
              for (const f of headForms) {
                const cur = (f && typeof f.getData === 'function') ? f.getData() : null
                if (cur && Object.prototype.hasOwnProperty.call(cur, 'doc_no')) f.setDataSet({ ...cur, doc_no: String(docNo) })
              }
            }
          } catch (e) { /* 回显失败不阻断保存结果 */ }
        }
        // view 草稿编辑态保存（submit=false）成功后，回退到只读查看
        if (!submit && state.mode === 'view' && state.editing) { state.editing = false; refresh() }
        // 保存并提交成功后切只读视图：CR 已进审批流不应再改，避免重复点「保存并提交」
        // 触发 submit 状态校验失败 → cmxError 模态遮罩锁页面。保存草稿保持可编辑（可继续修改）。
        if (submit) {
          state.mode = 'view'
          // 同步 crId：create/update 新建保存并提交后切 view 详情页，doCrAction（提交/通过/驳回/作废）
          // 读 state.crId；不同步则 !crId 静默 return → 详情页操作无反应（无弹窗/无接口/无报错）。
          state.crId = crId
          // 提交后 CR 已进审批流：detail + 流程上下文一起重拉，按钮组（发起人撤回等）与
          // 流程卡随 approving 态渲染，否则显示"无操作权限/暂无审批记录"的过期内容。
          await reloadDetail()
          await reloadFlowCtx()
          refresh()
        }
        resolve(true)
      } catch (e) {
        if (e && e.violations && typeof C.formatViolations === 'function') {
          C.cmxError?.(`数据校验未通过：\n${C.formatViolations(e.violations, TABLE_NAMES)}`)
        } else {
          C.cmxError?.(`保存失败：${e.message}`)
        }
        resolve(false)
      }
    } finally {
      state.saving = false
      setWriteBusy(false)
    }
  })
  })
}

// 退回重办「保存并提交」：保存修改 → 办结发起人确认任务（confirm-apply）→ 流程继续审批。
// 保存失败则停在校正后的表单；确认失败不回滚已保存的修改（提示改走查看页「确认并继续」）。
async function doSaveConfirmApply() {
  const M = cmx()
  const saved = await doSave(false)
  if (!saved) return
  try {
    await apiPost('/api/mdm/change-requests/confirm-apply', { crId: Number(state.crId) }, state.dbId)
    M.cmxInfo?.('已保存并提交，流程继续审批')
    await reloadDetail()
    await reloadFlowCtx()
    refresh()
  } catch (e) { M.cmxError?.(`确认失败：${e.message}（修改已保存，可在操作区点「确认并继续」重试）`) }
}

// view 模式单据状态操作的确认文案（提交/通过/驳回/作废）。danger=true 走警示红，用于不可恢复操作。
const CR_ACTION_CONFIRM = {
  submit:  { title: '提交审批', msg: (id) => `确认提交 CR-${id}？提交后进入审批流程，单据内容将无法在此页修改。`, danger: false },
  abort:   { title: '作废',     msg: (id) => `确认作废 CR-${id}？作废后该单据终止，不可恢复。`, danger: true },
}

// view 模式单据状态操作（提交/作废/通过/驳回），复用 /api/mdm/change-requests/* 接口。
// confirmFirst=true 前置二次确认（文案取 CR_ACTION_CONFIRM）；needReason=true 从意见框取理由（驳回默认"详情页驳回"）。
async function doCrAction(act, confirmFirst = false, needReason = false) {
  const C = cmx()
  if (state.saving) { C.cmxWarn?.('单据操作处理中，请稍候'); return }
  const crId = Number(state.crId)
  if (!crId) { C.cmxWarn?.('单据 id 缺失，无法操作，请重新打开该单据'); return }
  state.saving = true
  setWriteBusy(true)
  try {
    if (confirmFirst) {
      const meta = CR_ACTION_CONFIRM[act]
      const ok = await C.cmxConfirm?.({
        title: meta?.title || '确认操作',
        message: meta ? meta.msg(crId) : `确认对 CR-${crId} 执行该操作？`,
        danger: meta?.danger ?? true,
      })
      if (ok === false) return
    }
    let url = ''
    if (act === 'submit') url = '/api/mdm/change-requests/submit'
    else if (act === 'abort') url = '/api/mdm/change-requests/abort'
    await apiPost(url, { crId }, state.dbId)
    const msgMap = { submit: '已提交审批', abort: '已作废' }
    C.cmxInfo?.(`CR-${crId} ${msgMap[act] || '操作成功'}`)
    // 提交/作废会改变流程状态与操作权限：detail + 流程上下文都要重拉，
    // 否则按钮区/轨迹卡仍按旧 reviewCtx/flowTrail 渲染（如提交后误显示"无操作权限"）。
    await reloadDetail()
    await reloadFlowCtx()
    refresh()
  } catch (e) { C.cmxError?.(`操作失败：${e.message}`) }
  finally { state.saving = false; setWriteBusy(false) }
}

// 状态操作后重新拉详情（doc_status 与表单回显数据源）；刷新由调用方在全部数据就绪后统一执行。
async function reloadDetail() {
  if (state.crId == null) return
  try {
    const detail = await apiGet(`/api/mdm/change-requests/detail?crId=${state.crId}`, state.dbId)
    state.crHead = (detail && detail.head) || {}
    state.crLines = (detail && detail.lines) || []
    state.editing = false
  } catch (e) { cmx().cmxError?.(`刷新详情失败：${e.message}`) }
}

// 重拉流程上下文（review-context 按钮权限 + 流程轨迹组件数据）。
// 状态动作（提交/作废/审批/撤回）后必须重拉——doc_status 变了，按钮组与轨迹都会变；
// 各自失败保持现状不阻断（详情已刷）。
async function reloadFlowCtx() {
  if (state.crId == null) return
  try {
    state.reviewCtx = await apiGet(`/api/mdm/change-requests/review-context?crId=${state.crId}`, state.dbId)
  } catch { /* noop */ }
  await ftLoad()
}

function syncSavedLineIds(idMap) {
  if (!idMap) return
  state.lineDefs.forEach((_, idx) => {
    const grid = lineGrids[idx]; if (!grid) return
    const ds = grid.getDataSet?.(); if (!ds || !ds.rows) return
    ds.rows.forEach((r) => {
      if (r._savedId == null && r.id != null && idMap[r.id] != null) r._savedId = idMap[r.id]
    })
  })
}

// ── 渲染编排 ────────────────────────────────────────────────────────────────
function bind(root) {
  rootEl = root
  if (state.loading || state.loadErr) return
  // 流程轨迹组件数据回填（flowTrailHtml 里的静态占位）。
  root.querySelectorAll('cmx-flow-trail').forEach((el) => {
    el.trail = (state.flowTrail || [])[0] || null
  })
  const showSteps = state.mode === 'create' && state.keyDefs.length > 0
  if (showSteps && state.step === 1) {
    try { buildKeyForm() } catch (e) { console.error('[cr-form] buildKeyForm fail', e) }
  } else {
    try { buildHeadForms() } catch (e) { console.error('[cr-form] buildHeadForms fail', e) }
    if (state.lineDefs.length) {
      try { buildLineGrids() } catch (e) { console.error('[cr-form] buildLineGrids fail', e) }
      // view 只读不绑明细增删；view 编辑态（editing）需要
      if (state.mode !== 'view' || state.editing) bindLineToolbar()
    }
  }
  root.querySelector('#fNext')?.addEventListener('click', onNext)
  root.querySelector('#fPrev')?.addEventListener('click', () => goStep(1))
  root.querySelector('#fSave')?.addEventListener('click', () => doSave(false))
  root.querySelector('#fSubmit')?.addEventListener('click', () => doSave(true))
  root.querySelector('#fSave2')?.addEventListener('click', () => doSave(false))
  root.querySelector('#fSubmit2')?.addEventListener('click', () => doSave(true))
  // view 模式右侧操作区按钮（按 doc_status 渲染，元素不存在则跳过）
  root.querySelector('#fEdit')?.addEventListener('click', () => { state.editing = true; refresh() })
  root.querySelector('#fEditCancel')?.addEventListener('click', () => { state.deletedLineIds = []; state.editing = false; refresh() })
  root.querySelector('#fEditSave')?.addEventListener('click', () => doSave(false))
  root.querySelector('#fConfirmSave')?.addEventListener('click', () => doSaveConfirmApply())
  root.querySelector('#fCrSubmit')?.addEventListener('click', () => doCrAction('submit', true))
  root.querySelector('#fAbort')?.addEventListener('click', () => doCrAction('abort', true))
  // M7.1：审批动作业务封装端点（通过/驳回/退回）——流程调用全在 MDM 后端。
  const doReviewAction = async (action) => {
    const M = cmx()
    const confirmMeta = {
      approve: { title: '审批通过', msg: '确认通过？通过后将自动激活并写入主数据，不可撤销。', danger: false },
      reject: { title: '驳回', msg: '确认驳回？申请人可修改后重新提交，主数据不受影响。', danger: true },
      ret: { title: '退回发起人', msg: '确认退回？流程将打回发起人确认节点（实例继续），单据数据不变。', danger: true },
    }[action]
    const ok = await M.cmxConfirm?.({ title: confirmMeta.title, message: confirmMeta.msg, danger: confirmMeta.danger })
    if (ok === false) return
    try {
      const comment = (rootEl.querySelector('#fOpinion')?.value || '').trim() || undefined
      if (action === 'ret') {
        await apiPost('/api/mdm/change-requests/return', { crId: Number(state.crId), reason: comment }, state.dbId)
        M.cmxInfo?.('已退回发起人确认')
      } else {
        const d = await apiPost('/api/mdm/change-requests/review', { crId: Number(state.crId), action, comment }, state.dbId)
        M.cmxInfo?.(action === 'approve' ? `已通过并激活（状态：${d.status}）` : '已驳回，申请人可修改重提')
      }
      await reloadDetail()
      // 重新拉流程上下文（按钮随状态/权限刷新）。
      await reloadFlowCtx()
      refresh()
    } catch (e) { M.cmxError?.(`操作失败：${e.message}`) }
  }
  root.querySelector('#fReviewApprove')?.addEventListener('click', () => doReviewAction('approve'))
  root.querySelector('#fReviewReject')?.addEventListener('click', () => doReviewAction('reject'))
  root.querySelector('#fReviewReturn')?.addEventListener('click', () => doReviewAction('ret'))
  // 退回重办确认：办结 apply 任务，流程继续走 review（可先「编辑修改」保存再确认）。
  root.querySelector('#fConfirmApply')?.addEventListener('click', async () => {
    const M = cmx()
    const ok = await M.cmxConfirm?.({ title: '确认继续', message: `确认重报 CR-${state.crId}？流程将从发起人确认继续走审批。`, danger: false })
    if (ok === false) return
    try {
      await apiPost('/api/mdm/change-requests/confirm-apply', { crId: Number(state.crId) }, state.dbId)
      M.cmxInfo?.('已确认，流程继续审批')
      await reloadDetail()
      await reloadFlowCtx()
      refresh()
    } catch (e) { M.cmxError?.(`操作失败：${e.message}`) }
  })
  // M7：发起人撤回（终止当前审批实例 + 回草稿）。
  root.querySelector('#fWithdraw')?.addEventListener('click', async () => {
    const M = cmx()
    const ok = await M.cmxConfirm?.({ title: '撤回申请', message: `确认撤回 CR-${state.crId}？当前审批将终止，单据回到草稿可修改后重新提交。`, danger: true })
    if (ok === false) return
    try {
      await apiPost('/api/mdm/change-requests/withdraw', { crId: Number(state.crId) }, state.dbId)
      M.cmxInfo?.('已撤回，单据回到草稿')
      const detail = await apiGet(`/api/mdm/change-requests/detail?crId=${state.crId}`, state.dbId)
      state.crHead = (detail && detail.head) || state.crHead
      // 撤回后轨迹重拉而非清空：被终止的这轮正是要留给用户看的轨迹。
      state.reviewCtx = null
      await ftLoad()
      refresh()
    } catch (e) { M.cmxError?.(`撤回失败：${e.message}`) }
  })
}

function bindLineToolbar() {
  rootEl.querySelector('#fAddRow')?.addEventListener('click', () => {
    const lm = state.lineDefs[activeLineIdx]; const grid = lineGrids[activeLineIdx]; if (!lm || !grid) return
    const seed = newLineRow(lm)
    const ds = grid.getDataSet?.()
    if (ds?.addRow) ds.addRow(seed); else grid.addRow?.(seed)
    queueMicrotask(() => grid?.refreshLayout?.())
  })
  rootEl.querySelector('#fDelRow')?.addEventListener('click', () => {
    const grid = lineGrids[activeLineIdx]; if (!grid) return
    const ids = grid.getSelectedIds?.() || []
    if (!ids.length) return
    // 已入库的被删行（有 _savedId）记录真实 id，供 changeset 的 deleted 增量删（仿 cmx-doc merge 语义）。
    // 未入库的新建行（合成 id 无 _savedId）仅从 grid 移除，无需进 deleted。
    const rows = lineRows(grid)
    for (const id of ids) {
      const r = rows.find((x) => String(x.id) === String(id))
      if (r && r._savedId != null && !state.deletedLineIds.includes(r._savedId)) state.deletedLineIds.push(r._savedId)
    }
    grid.removeRows(ids); queueMicrotask(() => grid?.refreshLayout?.())
  })
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

// ── 入口 ────────────────────────────────────────────────────────────────────
async function init(tok) {
  // 过期判定：content 每次重新进入会 initToken++，使旧 tok 作废；
  // 每个 await 之后检查，作废则立即中止，避免旧异步链覆盖新状态（根治刷新并发）。
  const stale = () => tok !== initToken
  try {
    if (stale()) return
    // view 模式：先加载 CR 详情，从 CR 头取 docType/crType 定位 activation 配置
    if (state.mode === 'view' && state.crId) {
      const detail = await apiGet(`/api/mdm/change-requests/detail?crId=${state.crId}`, state.dbId)
      if (stale()) return
      state.crHead = (detail && detail.head) || {}
      state.crLines = (detail && detail.lines) || []
      state.docType = state.crHead.doc_type || state.docType
      state.crType = state.crHead.cr_type || state.crType
      // view 草稿编辑保存走 update 该 CR（复用 doSave 的 savedCrId 分支）
      state.savedCrId = Number(state.crId) || null
      // autoEdit（修改重提入口）：rejected/draft 原单据直接进编辑态，省去用户再点「编辑」；
      // flowEdit（退回重办入口）：approving 退回单同样直接进编辑态——发起人改完「保存并提交」即可。
      if (state.flowEdit || (state.autoEdit && (state.crHead.doc_status === 'rejected' || state.crHead.doc_status === 'draft'))) {
        state.editing = true
      }
      // M7：轨迹对草稿也拉——提交过又撤回的单据有已终止轮次，回草稿仍要看；
      // 全新草稿拉到空数组走占位卡（「提交后此处展示」），失败不阻断表单。
      // M7.1：流程操作按钮上下文仅对已进流转的单据拉（草稿无审批任务）。
      try {
        state.flowTrail = null
        await ftLoad()
      } catch (e) { console.warn('[cr-form] 流程轨迹加载失败', e) }
      if (state.crHead.doc_status !== 'draft') {
        try {
          state.reviewCtx = await apiGet(`/api/mdm/change-requests/review-context?crId=${state.crId}`, state.dbId)
        } catch (e) { console.warn('[cr-form] 流程按钮上下文加载失败', e) }
      }
    }
    state.activation = await loadActivation()
    if (stale()) return
    await buildFieldModel()
    if (stale()) return
    // update 变更模式：按 targetId 加载目标字典的头记录 + 各明细类型记录（元数据驱动，预填表单）
    if (state.mode === 'update' && state.targetId) {
      await loadTargetData(tok, state.targetId)
      if (stale()) return
    }
  } catch (e) {
    if (stale()) return
    state.loadErr = `元数据加载失败：${e.message}`
    console.error('[cr-form] init fail', e)
  }
  if (stale()) return
  state.loading = false
  refresh()
}

export default {
  defaultView: 'content',
  views: {
    async content(ctx) {
      const host = ctx && ctx.host; currentHost = host
      const p = (ctx && ctx.props) || {}
      const wctx = host && host.workspace && host.workspace.context
      const ctxGet = (k) => { try { return wctx && wctx.get ? wctx.get(k) : undefined } catch { return undefined } }
      state.coord = readCoord(ctx)
      state.dbId = state.coord.dbId || p.dbId || p.db_id || ''
      state.crId = ctxGet('crId') || p.crId || null
      state.docType = ctxGet('docType') || p.docType || ''
      state.crType = ctxGet('crType') || p.crType || 'create'
      // M7 审批态（待办中心打开）：formMode=approve + bizId 定位单据，复用 view 只读渲染；
      // 退回重办态：formMode=edit + bizId（apply 节点任务）——同样走 view 态打开原 CR，
      // 否则落入 create 分支：props 无 docType → 激活映射按空 docType 匹配 → 「未找到激活映射配置」。
      // 宿主注入的 mode:'task' 显式忽略（'task' 会落入 create 式可编辑渲染）。
      const formModeVal = ctxGet('formMode') || p.formMode
      state.flowApprove = formModeVal === 'approve'
      state.flowEdit = formModeVal === 'edit' && !!(ctxGet('bizId') || p.bizId)
      if (state.flowApprove || state.flowEdit) {
        state.crId = ctxGet('bizId') || p.bizId || state.crId
        state.taskId = ctxGet('taskId') || p.taskId || ''
        state.instanceId = ctxGet('instanceId') || p.instanceId || ''
      } else {
        state.taskId = ''; state.instanceId = ''
      }
      // mode：view（只读详情，由 cr-todo 传 crId）/ update（变更，列表台传 target）/ create（新增）
      let modeVal = ctxGet('mode') || p.mode || ''
      if (modeVal === 'task') modeVal = '' // 待办中心宿主标识，非本页模式
      state.mode = (state.flowApprove || state.flowEdit) ? 'view'
        : (modeVal || (state.crId ? 'view' : (state.crType === 'update' ? 'update' : 'create')))
      state.targetId = ctxGet('targetId') || p.targetId || null
      state.targetName = ctxGet('targetName') || p.targetName || ''
      state.step = state.mode === 'create' ? 1 : 2
      state.keyVals = {}; state.keyDefs = []; state.savedCrId = null
      state.crHead = null; state.crLines = []
      state.target = null; state.targetLines = {}
      state.editing = false
      state.autoEdit = ctxGet('autoEdit') || p.autoEdit || false
      state.deletedLineIds = []
      state.loading = true; state.loadErr = ''
    activeLineIdx = 0; lineSeq = 0
    initToken++; const tok = initToken
    if (host) whenRendered(host, '.pg', () => { init(tok) })
      return `<style>${styleCss()}</style>${viewHtml()}`
    },
  },
}
