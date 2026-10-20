# 官方用量漏采集与已有对话绕过网关

2026-10-20，build 7。

## 问题与修正

用户重启Codex后确认新对话已经接入网关，但用量只有字符估算，输出／缓存／推理详细计数均未知。读取本应用管理接口可见4次新请求均成功，却没有终止流事件或任何输出观察；此前没有采集响应正文。

旧观察器只在响应Content-Type以text/event-stream开头时解析SSE。官方Codex上游可能省略此头，因此被按整包JSON解析，流文本无法作为JSON读取，末尾usage也就全部遗漏。修正为：显式SSE MIME按大小写不敏感、忽略参数识别；原请求stream:true且成功响应缺少Content-Type时同样按SSE观察。明确返回其他类型时保留原规则。转发的响应头、原始字节、背压和代理策略均不改动。

格式参考：[OpenAI streaming Responses](https://developers.openai.com/api/docs/guides/streaming-responses/)。公开只读参考cc-switch的response_processor.rs在is_sse_response中也明确记录chatgpt.com Codex响应缺少Content-Type的情况；本次独立实现同类缺头判定，没有引入正文日志或真实账号生成。

用户还确认当前旧对话仍访问原自定义源。根配置改为codexpress只影响默认选择，桌面已有线程仍可保存原provider ID。接管现在同时将已声明自定义ID建立为网关连接别名，预览列出这些ID；原定义和受管定义保存在可选gateway_aliases快照中。逐别名冲突检测、重复应用、官方往返和局部还原均保留原定义，其他后来添加的供应商不被清除。内置保留ID不改写，对话正文／历史数据库不改写。

兼容配置必须由正常应用／重启流程加载；更新Codexpress本身不会热更新正在运行的Codex连接。未知原源加密状态仍返回409，不通过删除密文、猜测账号或重写历史来伪造兼容。此前漏采的明细无法从已保存的数字元数据恢复，新请求使用修正后的解析器。

## 验证

- 新缺头SSE回归在旧实现上失败：上游报告输入1234，旧记录只有估算2；修正后输入1234、输出56、缓存1000、缓存写入20、推理12、总计1290完整记录，响应头及字节保持不变。
- 兼容连接回归验证custom／second及codexpress均指向本机；原表可通过重复应用、官方往返及恢复取回，后来新增provider和MCP保留，历史哨兵字节相同；外部修改受管别名时拒绝覆盖。
- `bash scripts/check.sh`：70项Rust测试、Clippy `-D warnings`、Swift编译通过。`swift test`：19项通过。
- `scripts/check-gateway-continuation.py`扩展missing-content-type和legacy-provider-id场景。真实CLI0.162.1、桌面内置0.163.0-alpha.10.2分别运行全部4场景，每场景仅两次localhost上游请求、一次工具执行，计数核验通过，无409。legacy-provider-id使用原本不可达的旧供应商定义，应用后显式选择该旧ID仍成功进入临时网关；不读取真实线程。
- arm64 Release构建及签名校验通过。网关inflight为0时正常退出并安装到`/Applications/Codexpress.app`；Swift／Rust二进制哈希与构建产物一致。安装后build7、helper来自安装目录，监听17863。
- 真实配置只读预览成功：valaa／cch／yw将接入本地地址，gpt-6.1-sol的512000和gpt-6-astra的250000压缩阈值不变。真实兼容补丁尚待用户确认应用，未自动重启Codex或写真实配置；没有删除缓存或对话。
- 独立只读审查未发现新增差异中的实质问题。

全部自动生成与行为验证使用临时状态和localhost模拟上游。没有真实付费生成、重置卡消费、凭据／正文日志。
