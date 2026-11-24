---
name: workcontext
description: 用户问上次怎么处理、之前定了什么、还有什么待验证，接手旧问题、复盘排障或核对项目背景时，按业务词找回工作记录。即使用户没点名工具也适用。取得有依据的新结论时查重并主动保存，纯查询只答复。
---

# 使用 WorkContext 接续工作

在桌面授权范围内自主查询和保存，无需等用户逐次提醒。先判断当前 Agent 和模型是否获准接收所需内容。远程会话中的工具输出可能直接发送给模型；本机接口开关、读取归档和向指定接收方外发各有独立授权。外发许可不明时，让涉及私有内容的查询留在获准的本机环境。

## 按任务找依据

- 从具体业务词开始，例如 `workcontext search "图纸预览" --limit 5 --json`。需要近期变化用 `workcontext recent`，需要全局背景用 `workcontext overview`。默认文本供人读，自动解析加 `--json`。
- 长句、组合词无命中时改用单个设备名、故障码或项目简称；再从 `workcontext recent --limit 5 --json` 或相关项目活动找线索。查过仍无依据就说明缺口。
- 先看摘要，再用返回的 ID 调 `workcontext records show ID`。需要项目目标和背景时用 `workcontext projects list`、`workcontext projects show ID`。区分本人已确认的决定、历史建议和 Agent 推断。没有项目或命中时继续当前任务。
- 只有证据会影响判断时，才用 `workcontext messages search "关键词" --limit 5 --json` 查本机归档，可用 `--conversation ID`、`--since`、`--until` 缩小范围。把命中的 `sourceRef` 原样传给 `workcontext source get 引用 --json`。标明缺失、过期或不可读的依据，避免把附件下载状态当成正文。
- 需要专门方法时用 `workcontext skills find 关键词 --json`，已知范围可加 `--scope ID`。只在 `available` 为真且自己有读取权限时读取返回路径的 `SKILL.md`。

## 完成有价值的工作就保存

已验证的修复、带依据的决定、可复用的失败尝试，或会影响下次行动的未决问题，都值得在结束前主动追加。写清结果和核验依据，把推断、待证问题分开。仅保留必要资料引用，省略整段会话、命令流水、凭据和内部推理。

追加前按对象和结果关键词查已有记录，打开相近条目比对事实、依据和未决问题。本轮已读的详情可直接用于比对。同一事实换了措辞、复述上次结论、纯查询或闲聊，直接答复，无需询问是否保存。有新增核验结果、决定或后续行动时只写增量，并用 `relatedRecordIds` 连接原记录。`requestId` 用于重试同一次提交，不能代替这一步查重。

1. 首次发送前为这一条成果生成并固定唯一 `requestId`，保留请求内容。将 UTF-8 JSON 经 stdin 交给 `workcontext records add --json`。示例结构如下，实际执行时换成本次任务的键和事实：

   ```json
   {"producer":"codex","requestId":"drawing-preview-01","title":"图纸预览色差复核","result":"两次导出的图例颜色一致，预览仍显示旧色。","inference":"预览可能使用了旧色板。","openQuestions":["切换色板后能否复现？"]}
   ```

2. 零项目时直接写，不填 `scopeIds`。已知关联用已有范围 ID，名称未确认可放 `scopeHints`。简单结果也可用 `workcontext records add --producer codex --request-id 固定键` 从 stdin 接收文本。PowerShell 管道先设 `$OutputEncoding = [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)`，文件用 `Get-Content -Raw -Encoding UTF8` 读取。
3. 检查退出状态与回执，保留 `recordId`。`saved: true` 表示已保存；`processingState: pending` 时可立即检索，整理仍在等待。回执丢失或维护占用时，以原账号、原署名、原键和原内容重试。同键冲突先核对；新增事实另用新键，可通过 `relatedRecordIds` 关联旧记录。无回执就说明保存结果未知。

确需新建项目／领域或修改项目已确认目标／背景时，另用 `workcontext project suggest --json` 从 stdin 提交 `producer`、固定 `requestId`、`kind`（`project` 或 `domain`）、`title`、`goal`、`background`、`reason` 和 `source`。修改项目时先取详情，沿用当前名称和需保留的内容，加 `scopeId` 和 `baseVersion`。返回 `suggestionId` 后按实际状态报告，待本人在桌面确认后才采用变更。保存普通工作成果无需等待建议获批。

接口禁用、账号绑定变化或来源被拒时停止相应访问，说明缺口并继续可完成的工作。需迁移或重新授权时交由桌面处理。不要直读库、缓存、附件目录或调用实时 DWS 补取受限内容。
