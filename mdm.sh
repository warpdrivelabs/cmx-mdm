#!/usr/bin/env bash
#
# 启动主数据微服务（MEGA MDM · :8095）。
#
# 统一启动契约（门户/流程/报表/规则/模型/主数据各服务同一套）：
#   1) cd 到本 workspace 根（.env / *-server.toml 相对路径基准）
#   2) cargo run 对应 bin（bin 自动读 .env → 配置生效）
#
# 用法：./mdm.sh [--release]
# 能力：激活映射 / CR 审批 / 查重去重 / 合并 / 订阅分发 / 审计事件 / 流程对接。
# 依赖：PostgreSQL（业务库 fico + 平台主库 cmx）+ 并排的 ../cmx-container、../cmx-model、../cmx-portalservice。
set -euo pipefail
cd "$(dirname "$0")"
exec cargo run -p cmx-mdm-server "$@"
