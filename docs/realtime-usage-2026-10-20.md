# 实时请求、Token 明细与来源统计

本次实现用户连续追加的三个需求：实时更新请求列表、读取回复 Token 明细，缺失计数时按字符估计，以及按来源展示 TPS 等统计并调整布局。

## 展示和口径

用量页每秒查询一次本地IPC，只在页面可见期间持续运行。顶部支持来源及24小时／7天／30天筛选，展示Token总量、请求、失败、成功率、TPS、首Token和平均耗时；中间来源表显示Token占比、缓存命中率及速度，点击行过滤总览和明细；下方从最近200次尝试中按20条展开。来源比较的统计覆盖所选时间范围，不仅是明细列表的200条。

每次上游尝试拥有独立稳定ID，开始时以连接中显示，接收时更新，结束后在同一锁下落库并移出进行中集合，避免刷新时双计。自动回退分开记录尝试，以共同request_id关联。进行中请求不计入失败和成功率分母。

实际读取Responses JSON/SSE的input_tokens、output_tokens、input_tokens_details.cached_tokens、cache_write_tokens、output_tokens_details.reasoning_tokens及total_tokens。公开字段依据：[OpenAI Responses 参考](https://developers.openai.com/api/reference/resources/responses/methods/retrieve)。只采固定数值，不保存任意usage对象。总量统一input+output，缓存及推理不重复加；上游total单独保留元数据。

缺失输入／输出独立估算：CJK及全角字符约1字符/Token，其他约4字符/Token。输入观察input、instructions和tools的文字；输出按每个output_index/item_id合并增量和完成快照，以最终output优先。省略图片、音频、文件和加密字段；无法观察的输出保持未知。估算明确带≈，随后获得上游计数即替换，真实0不被估算覆盖。缓存／推理从不猜测。

首Token从提交该次尝试到首个流式文字／工具参数／推理文字计时。TPS=输出Token/该时刻至结束事件的秒数，事件结束后即冻结，连接延后关闭不影响速度。只有成功且有非零生成时长的样本进入加权平均。一次性JSON或同一瞬间到齐的流不展示假速度。基于估算Token的速度也标≈；没有观测到的隐藏推理及多模态开销不包含在字符估算中。

SQLite迁移只新增metadata列，保留旧数据；旧非零计数继续可见，未知零值及从未采集的速度保持未知。持久化仍只有名称、模型、时间、状态、计数和时长；不记录请求／响应正文或凭据。超出1MiB观察上限、压缩或不兼容的流仍原字节透传，但估算或明细可能不完整。

## 验证

- 56项Rust测试、Clippy `-D warnings`，19项Swift测试通过。
- 本地模拟上游分阶段暂停请求，验证连接中可见、流式估算增长、最终上游值替换、缺失值估算、明确零值、缓存/推理明细、前后尝试无双计及TPS计算。
- 多输出项增量／快照合并、终止后继续保持连接的计时冻结、非空429回退仍保留失败状态、旧SQLite迁移／重新打开及来源筛选验证通过。
- Swift验证多来源加权TPS和首Token、成功率分母排除进行中请求。隔离预览检查来源筛选和详情布局，无真实账号数据。
- Release arm64构建版本5；使用真实Codex CLI及桌面内置Codex配合本机模拟上游检查完整工具循环和最终usage IPC字段。未进行付费生成。
- 已更新并正常重启 `/Applications/Codexpress.app`，二进制与构建产物一致、完整签名校验通过；只检查本应用运行状态，未读取真实Codex配置、认证或历史。应用自身旧用量库采用兼容迁移。

视觉记录：[统计总览](images/usage-live-overview.png)、[单来源](images/usage-live-source.png)。
