# CC Switch 与 codexbar 源码调研

日期：2026-10-20。目的：为 Apple Silicon 上的轻量 Codex 认证、代理与多源优先级网关确定可复用思路和边界。

## 已完成的工作

- 已把两个项目以浅克隆方式下载到本工作目录，保留各自 Git 元数据和许可证。
- 阅读了原生界面入口、认证流程、配置同步、代理选择、优先级路由、流式转发、用量采集和相关测试文件。
- 通过 OpenAI Docs 核对了认证存储、自定义供应商、保留的供应商 ID 及本地网关配置。
- 本阶段是静态源码研究。没有构建、安装或运行上游应用，没有读取用户的认证文件或令牌，也没有修改现有 Codex、系统代理或 Clash 配置。

| 项目 | 本地目录 | 本次研究的提交 | 许可证 |
| --- | --- | --- | --- |
| [codexbar](https://github.com/lizhelang/codexbar) | `/Users/rhetoric/Work/Codexpress/references/codexbar` | `3ade3f6579248599fb764f284f4cbedc8dd494e1` | MIT，Copyright 2025 codexbar contributors |
| [CC Switch](https://github.com/farion1231/cc-switch) | `/Users/rhetoric/Work/Codexpress/references/cc-switch` | `f92ff0b03964188f1cd011d9302ab8869d7a8219` | MIT，Copyright 2025 Jason Young |

以后实质性复用代码时，应保留对应 MIT 版权和许可说明。当前仅研究参考，没有把上游业务代码复制进新产品。

## 用户已经确认的边界

1. 官方 ChatGPT OAuth 账号和第三方 API 共同进入优先级池。
2. 只需覆盖系统代理模式；Clash TUN／增强模式保持关闭。
3. 官方源需要支持重置卡的手动使用。
4. 首先支持 Mac Apple Silicon；产品以 Codex Desktop 的使用体验为中心，同时验证共享配置下的 CLI。

第四项的 Desktop 优先是根据前序对话作出的产品定位；并不代表已经验证具体客户端版本兼容性。

## codexbar：值得借鉴的部分

### 原生菜单栏交互

项目采用 Swift／SwiftUI／AppKit。可参考其菜单栏控制器、短面板、独立设置窗口和后台刷新合并策略。当前版本已经管理多个 AI 工具、设备和趋势等页面，定制项目不需要照搬完整功能范围。

入口参考：[MenuBarStatusItemController.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/MenuBarStatusItemController.swift)、[CoalescedBackgroundRefreshController.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/CoalescedBackgroundRefreshController.swift)。

### 一个 CODEX_HOME，加上局部配置更新

[CodexSyncService.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/CodexSyncService.swift:129) 会先读取原配置，备份后写入，并在失败时尝试回滚。第三方 API 的认证保存在独立 provider 中，普通第三方切换不重写 OpenAI `auth.json`。

但不能把整个同步器直接当成“所有配置不变”的保证：`renderConfigTOML` 同时更新 model、review_model、推理强度，并移除部分服务档位或旧设置。定制项目应缩小写入白名单，避免沿用这些产品决策。

### 官方 OAuth 和额度

[OpenAIOAuthFlowService.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/OpenAIOAuthFlowService.swift:140) 实现 PKCE、state 和 localhost 回调。[WhamService.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/WhamService.swift:418) 查询 `chatgpt.com/backend-api/wham/usage`，解析套餐、多个限流窗口及重置时间。

需要调整两点：

- 额度查询直接使用 `URLSession.shared`；OAuth 流程默认也使用共享会话。网关上的账号代理配置不能自动覆盖这些网络请求。
- 部分缺失用量字段会被解析为 0。新产品应显示“未知／上次更新时间”，避免把缺字段误报为满额。

该额度路径是客户端内部接口；本次官方文档没有建立其为稳定、公开的余额 API。可以参考和适配，但需要显示失败与陈旧状态，不能把查询失败视为额度耗尽。

### 官方重置卡的查询与手动使用

[WhamService.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/WhamService.swift:31) 查询 `GET /backend-api/wham/rate-limit-reset-credits`；[consumeResetCredit](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/WhamService.swift:48) 向 `/backend-api/wham/rate-limit-reset-credits/consume` 提交 POST，包含 credit_id 和 redeem_request_id。

[RateLimitResetCredit.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Models/RateLimitResetCredit.swift:34) 区分 reset、nothing_to_reset、no_credit、already_redeemed 和 unknown；[ResetCreditsPanelView.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Views/ResetCreditsPanelView.swift:40) 提供逐卡使用入口。

可参考其卡片、到期时间和业务结果结构，但新产品必须把查询／提交一起纳入逐源代理；不能在操作超时后生成新请求 ID 盲目重试。上游存在 redeem_request_id 字段，并不能替代对幂等语义的验证。成功后重新查询额度和卡片，只解除已确认恢复的额度限制；累计 Token 统计不因重置清零。此功能加入首版设计，本阶段没有提交真实重置操作。

### 官方账号网关

[OpenAIAccountGatewayService.swift](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/OpenAIAccountGatewayService.swift:1471) 实现按账号替换 Authorization、chatgpt-account-id，支持流式输出、账号粘性及显式代理会话。

它并不是通用的字节透传器：[normalizeResponsesRequestBody](/Users/rhetoric/Work/Codexpress/references/codexbar/codexBar/Services/OpenAIAccountGatewayService.swift:1631) 强制 `store=false`、`stream=true`，移除若干参数并补全字段；compact 路径也有字段白名单。这些规则只适用于对应官方链路，不应对所有第三方请求套用。

## CC Switch：值得借鉴的部分

### 保留 TOML 的局部补丁

[live/patch/toml.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/live/patch/toml.rs:1) 使用 `toml_edit`，按指定键修改，并保留布局和注释。这比重新生成整个文件更适合“只管理连接配置”的目标。

新产品还需增加受管字段快照、外部修改检测及三方合并恢复，避免退出时用旧文件覆盖用户在接管期间的新设置。

### 优先级与熔断

[provider_router.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/provider_router.rs:15) 按队列排序候选源，并通过熔断器筛选可用项。值得参考其 Closed／Open／HalfOpen 状态和并发许可管理。

这里有一个直接影响本项目的边界：使用客户端原生 Authorization 的 Codex Official 卡片被明确排除在跨供应商重试之外，以免把账号 A 的凭据当成账号 B 使用。因此“把所有源加到现成故障转移列表”不能直接满足本项目。

CC Switch 也有单独管理的 Codex OAuth 供应商实现。需要区分该实现与原生凭据透传卡片；本调研不把这两类路径混为一谈，也不声称 CC Switch 完全不能聚合官方订阅。

### HTTP 客户端与代理

[http_client.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/http_client.rs:281) 包含 reqwest 客户端、显式 HTTP/SOCKS 代理和禁止自动解压等处理。但其通用客户端以全局代理为中心；未指定代理时仍存在跟随系统／环境代理的路径。

新产品应使用逐源客户端工厂：直连客户端显式关闭代理发现，代理客户端先关闭自动发现再设置指定代理；在源之间切换时不复用错误的连接池。

### 流式透传与旁路统计

[response_processor.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/response_processor.rs:174) 的原生流式路径复制状态和响应头，剥离逐跳头，再把上游字节流交给客户端。[create_logged_passthrough_stream](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/response_processor.rs:731) 在旁路解析 SSE 用量后继续产出原字节。

这符合本项目“统计不重写生成内容”的方向。新产品应进一步约束旁路缓存大小、UTF-8/SSE 跨块解析及压缩观察分支，避免统计失败中断转发。

[usage/logger.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/usage/logger.rs:69) 的来源归属、模型、请求状态、首字延迟和去重思路也值得参考。无需引入其所有软件的会话扫描和计费规则。

### OAuth 的刷新竞争

[codex_oauth_auth.rs](/Users/rhetoric/Work/Codexpress/references/cc-switch/src-tauri/src/proxy/providers/codex_oauth_auth.rs:182) 包含刷新令牌代际判断和原生认证变更检查。这说明直接复制一份现有 refresh token 并让两个程序同时刷新，会产生真实的一致性问题。

首版优先为 Codexpress 通过正常授权获得自己的官方会话，不让 Codex 和网关争写同一份认证文件。现有认证导入若以后增加，必须先明确唯一刷新所有者。

## 官方文档查证

- [Authentication](https://learn.chatgpt.com/docs/auth)：ChatGPT 与 API Key 是两种认证方法；认证可位于 `auth.json`、系统凭据库或进程内。不能只通过是否存在 `auth.json` 推断登录状态。
- [Advanced Configuration](https://learn.chatgpt.com/docs/config-file/config-advanced#custom-model-providers)：自定义 provider 定义 URL、协议、认证及可选请求头；不能使用保留 ID `openai`、`ollama`、`lmstudio`。
- 同页给出 `openai_base_url`，用于把内置 OpenAI provider 指向网关；不能新建 `[model_providers.openai]` 来覆盖内置 provider。

实际网页已读取；高级配置 Markdown 也已获取并核对。产品接管仍要按实际安装的 Codex 版本做兼容性验收。

## 建议

推荐新建 SwiftUI 菜单栏客户端和精简 Rust 网关。参考 codexbar 的原生交互、OAuth 和额度字段，参考 CC Switch 的 TOML 补丁、HTTP 流、统计与熔断思路。

不要整套拼接两个产品。首版只支持官方 Codex 协议和原生 Responses 上游，不做 Responses／Chat Completions／Anthropic 间的自动转换。

必须把以下区别写进产品承诺：

1. 历史文件和工具配置完整保留，可以通过文件与客户端验收证明。
2. 上游 `previous_response_id`、WebSocket 状态或加密推理内容能否跨账号继续，由上游能力决定；不能承诺任意时刻切源都无感。
3. 系统代理关闭与 Clash TUN 绕过不同。本项目依用户确认仅覆盖前者。
4. 原样透传指尽量保留请求体及响应内容；认证头、逐跳头和必要目标路径必须由网关正确处理，绝不能把官方凭据发给第三方。

## 研究进度

- [x] 克隆并固定两个上游提交。
- [x] 阅读相关仓库协作约定、许可证和关键源码。
- [x] 核实官方认证与供应商配置文档。
- [x] 确认混合池范围和 TUN 边界。
- [x] 形成可审阅的定制方案草案。
- [ ] 用户审阅架构与首版边界。
- [ ] 审阅通过后编写实施计划并开始实现。

最后两项是后续阶段，尚未执行。没有把静态调研当成产品功能验证。
