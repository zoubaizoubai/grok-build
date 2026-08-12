# BYOK 文本模型与视觉辅助模型实现可行性研究

> 状态：第一阶段已实现；真实 provider 端到端验证待隔离 API key
>
> 调研日期：2026-08-12
>
> 目标：在 Grok Build fork 中支持“DeepSeek 等纯文本 BYOK 模型作为主模型，Kimi 作为独立视觉辅助模型”，并避免原始图片进入不支持图片的主模型请求。

## 1. 结论摘要

1. **总体方案可行。** fork 已经具备图片持久化、Base64 编码、视觉辅助模型请求、描述缓存和描述文本注入能力，不需要重新设计图片传输层。
2. **当前问题不在 `image_description` 配置本身。** 原生 TUI 的普通贴图路径由 `is_cursor_harness() == false` 分流到原始图片路径，绕过了 `transcribe_user_images()`，所以 DeepSeek 仍收到 `image_url` 并返回 400。
3. **不能把 `is_cursor_harness()` 全局改成 `true`。** 该判断还控制 Cursor 专用的 prompt、skills、tool result、plan reminder 等行为，副作用与图片功能无关。
4. **推荐新增独立的图片输入策略。** 当前主模型支持图片时保留原始多模态输入；不支持图片时调用视觉 helper，只向主模型发送描述文本和本地图片路径。
5. **模型切换必须处理历史图片。** 当前实现不会改写或静默删除历史：切到文本模型后，如请求仍含历史用户图片，会在采样前返回结构化阻断，提示切回视觉模型或使用 `/new`。
6. **Kimi 可以替代 `grok-4.5`。** 最小改动路径建议先用 `moonshot-v1-32k-vision-preview`；`kimi-k2.6`/`kimi-k3` 可作为后续质量评估目标，但 K2.5/K2.6 需要处理 temperature/thinking 参数兼容性。

## 2. 问题背景

### 2.1 DeepSeek 直接收到图片

当前异常的本质是主请求中出现了 OpenAI 多模态结构：

```text
messages[N].content[*].type = "image_url"
```

DeepSeek 的纯文本 Chat Completions 接口只接受 `text`，因此返回类似：

```text
unknown variant `image_url`, expected `text`
```

正确的目标链路应为：

```text
用户图片
  → 视觉辅助模型生成描述
  → 描述文本进入主模型
  → DeepSeek 只收到 text
```

### 2.2 模型切换造成历史污染

当前 `/model` 只切换后续推理使用的模型，不会重写已有 conversation history。因此下列流程仍会失败：

```text
Grok/Kimi 直接看图
  → /model deepseek
  → 继续发送纯文本
  → 历史 messages 仍包含 image_url
  → DeepSeek 400
```

`/new` 可以绕开这个问题，但不是同一 session 无损切换的完整解决方案。

## 3. 当前代码链路

相关源码均位于当前仓库：

| 能力 | 代码位置 | 当前结论 |
|---|---|---|
| 图片描述、缓存、持久化 | [`image_describe.rs`](../../crates/codegen/xai-grok-shell/src/session/image_describe.rs) | 基础设施已存在，可复用 |
| 普通用户贴图分支 | [`turn.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs) | 非 Cursor 路径把原图继续交给主模型 |
| interjection 图片 | [`interjection.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) | 目前只有 Cursor 路径会 describe |
| Cursor 判断 | [`session_mode.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/session_mode.rs) | `is_cursor_harness()` 固定返回 `false` |
| 辅助模型路由 | [`config.rs`](../../crates/codegen/xai-grok-shell/src/agent/config.rs) | 支持独立 model、base URL、API key/env key、API backend |
| 模型切换 | [`model_switch.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs) | 只切换采样配置，不迁移 history 图片 |
| 模型图片能力 | [`model_state.rs`](../../crates/codegen/xai-grok-pager/src/acp/model_state.rs) | 已能读取 `acceptsImages`，但主要用于 UI |
| 工具结果图片 | [`tool_calls.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs) | 非 Cursor 路径还可能把工具图片放入后续请求 |
| 历史图片恢复 | [`jsonl/mod.rs`](../../crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs) | 可清理无效图片，但不是按模型能力做转换 |

### 3.1 已有图片描述基础设施

`image_describe.rs` 已经完成以下工作：

- 将用户图片保存到 session `assets/` 目录
- 生成 `data:<mime>;base64,...` URI
- 通过 `ConversationRequest` 携带文本和图片调用辅助模型
- 缓存相同图片和描述请求的结果
- 将结果封装为 `<image_description>` 文本块
- 生成 `<image_files>` 路径块，让主模型仍可读取本地原图

因此要解决的重点是**何时调用 helper，以及如何构造主模型 history**，不是重新实现视觉请求。

### 3.2 当前普通贴图分支

当前逻辑可概括为：

```text
本轮无图片
  → 发送纯文本

本轮有图片 && is_cursor_harness()
  → transcribe_user_images()
  → 主模型收到描述文本

本轮有图片 && !is_cursor_harness()
  → persist_and_prepend_image_files()
  → user_images 继续进入主模型多模态 history
```

最后一条路径正是 DeepSeek 400 的直接来源。

### 3.3 辅助模型路由的安全边界

`resolve_aux_model_sampling_config()` 支持按 model catalog 解析独立凭据。必须为 Kimi 建立显式 `[model.*]` 配置并设置 `env_key`，否则辅助模型没有凭据时可能回退到当前主模型。对 DeepSeek 主模型而言，这种回退会让视觉描述再次落到纯文本模型，不能满足目标。

## 4. Kimi 视觉辅助模型方案

### 4.1 API 兼容性

Kimi 官方 API 提供 OpenAI-compatible Chat Completions，视觉模型支持 Base64 `image_url`。当前 fork 已将图片转为 data URI，因此 Kimi 接入可以复用现有 `chat_completions` backend。

官方参考：

- [Use the Kimi Vision Model](https://platform.kimi.ai/docs/guide/use-kimi-vision-model)
- [Kimi Model List](https://platform.kimi.ai/docs/models)
- [Kimi API Overview](https://platform.kimi.ai/docs/api/overview)
- [Migrating from OpenAI to Kimi API](https://platform.kimi.ai/docs/guide/migrating-from-openai-to-kimi)

Kimi 视觉接口不应依赖普通远程图片 URL；优先使用当前代码生成的 Base64 data URI，或后续接入 Kimi 文件上传 ID。

### 4.2 第一阶段推荐配置

为了避开 Kimi K2.5/K2.6 的特殊采样参数限制，第一阶段建议使用明确的 Vision Preview 模型：

```toml
[models]
image_description = "kimi-vision"

[model.kimi-vision]
model = "moonshot-v1-32k-vision-preview"
name = "Kimi Vision"
base_url = "https://api.moonshot.ai/v1"
api_backend = "chat_completions"
env_key = "MOONSHOT_API_KEY"
context_window = 32768
max_completion_tokens = 4096
```

API key 通过环境变量提供：

```bash
export MOONSHOT_API_KEY="..."
```

不要把真实 key 写入配置文件、日志或仓库。

### 4.3 K2.6/K3 的后续评估

官方当前模型列表中包含支持视觉输入的 `kimi-k2.6`、`kimi-k2.5` 和原生视觉理解的 `kimi-k3`。**已修复（2026-08-12）：** image-describe 请求不再写死 `temperature = 0.2`。  
`finalize_image_describe_sampler_config` 会：

- 使用 `[model.<id>].temperature` / `[models].temperature`（若已配置）
- 否则回退 `IMAGE_DESCRIBE_DEFAULT_TEMPERATURE = 0.2`

因此 Kimi K3 可在模型配置中设 `temperature = 1.0`。K2.5/K2.6 的 thinking 参数约束仍可能需要额外 policy。

## 5. 推荐实现架构

### 5.1 新增独立图片输入策略

新增与 Cursor 模式无关的策略，概念上类似：

```text
ImageInputPolicy::PassThrough
ImageInputPolicy::DescribeWithAuxModel
```

决策依据应是当前主模型的能力，而不是 `is_cursor_harness()`：

```text
当前主模型 accepts_images = true
  → PassThrough
  → 主模型接收原始图片

当前主模型 accepts_images = false
  → DescribeWithAuxModel
  → 调用 Kimi helper
  → 主模型只接收描述文本和图片文件路径
```

不要全局把 `is_cursor_harness()` 改为 `true`，因为它还影响与图片无关的 Cursor 专用行为。

### 5.2 普通用户贴图

在 [`turn.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs) 中：

1. 识别当前主模型是否支持图片
2. 不支持时调用已有 `transcribe_user_images()`
3. 将描述文本和 `<image_files>` 注入当前用户消息
4. 不把 `user_images` 或 `extra_images` 追加到主模型的 `ConversationItem`
5. 支持图片时保留现有直接多模态路径

describe 失败应终止当前 turn，并返回包含模型名、错误原因和重试建议的可理解错误；不能静默删除图片上下文。

### 5.3 interjection 图片

在 [`interjection.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs) 中复用同一个 `ImageInputPolicy`：

- 文本模型：先 describe，再将图片附件清空，避免 synthetic user message 把原图写入 history
- Vision 模型：保留原始图片
- 失败处理与普通贴图保持一致

如果只修改普通贴图而遗漏 interjection，仍可能通过中途图片把 `image_url` 注入 DeepSeek history。

### 5.4 能力元数据

建议为 `[model.*]` 增加：

```toml
accepts_images = false
```

并在 ACP model metadata 中写入 `acceptsImages` 或等价的 `inputModalities`。

该字段同时服务于：

- UI 是否显示图片提示
- 普通贴图路由
- interjection 路由
- history 序列化
- 测试中对请求 payload 的断言

但不能只修改 UI metadata；真正的请求构造处必须再次使用同一能力结果。

### 5.5 history 的安全阻断

为避免不可逆地改写已持久化会话，第一阶段采用采样前阻断：文本模型的请求中只要仍包含历史 `ConversationItem::User` 图片，就不调用主模型，并返回 `TEXT_MODEL_HISTORY_CONTAINS_IMAGES`、模型名、图片数量及处理建议。切回视觉模型后可继续原会话；也可用 `/new` 开始文本会话。工具结果图片保持现状，明确列为第一阶段边界。

### 5.6 工具结果图片的范围

工具结果（截图、PDF、浏览器输出）也可能携带图片。第一阶段可明确限定范围为“用户粘贴图片”，先不改变工具结果路径；但验收文档必须标注这一边界。

如果产品要求“任何情况下 DeepSeek 都不能收到图片”，则必须继续处理 [`tool_calls.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs) 等工具图片路径，并让工具图片也走描述或模型感知序列化。

## 6. 推荐实施顺序

### Phase 1：最小可行版本

1. 配置 `moonshot-v1-32k-vision-preview`
2. 为普通用户贴图接入 `ImageInputPolicy`
3. 确认 DeepSeek 主请求中不再出现 `image_url`
4. 添加 Kimi helper 的 HTTP payload 集成测试

### Phase 2：补齐交互路径

1. interjection 复用图片策略
2. 增加 `accepts_images` 配置字段
3. 将能力写入 ACP model metadata
4. 增加多图、缓存、超时和鉴权失败测试

### Phase 3：解决同 session 模型切换

1. 引入图片资产与描述的双表示
2. 实现按主模型能力的 history 序列化
3. 为旧 history 增加图片描述迁移
4. 验证 Vision → DeepSeek 切换后纯文本继续对话不再 400

### Phase 4：扩展模型和工具图片

1. 评估 `kimi-k2.6`/`kimi-k3` 视觉质量
2. 增加 Kimi thinking/temperature provider policy
3. 决定是否处理工具结果图片
4. 评估与上游定期同步时的维护成本

## 7. 验收清单

### 主流程

- [x] DeepSeek + 用户贴图：主请求不包含用户 `image_url`
- [ ] Kimi helper 收到 Base64 图片并返回非空描述
- [x] 主模型收到 `<image_description>` 和可用的 `<image_files>` 路径
- [x] Vision 主模型仍可保留原始图片直传能力

### 模型切换与 history

- [ ] Kimi/Grok 看图后同 session 切换 DeepSeek，继续发纯文本不返回 400
- [x] 旧 history 中没有描述的图片可以迁移或给出明确阻断
- [ ] `/new` 仍能作为安全降级路径
- [ ] 历史恢复、压缩、fork/subagent 不会重新注入不兼容图片

### 边界路径

- [x] interjection 图片使用同一策略
- [ ] 多图顺序和描述对应关系正确
- [ ] 描述缓存命中时不重复调用 Kimi
- [ ] Kimi 超时、401、429、空响应时错误可理解
- [x] 工具结果图片是否纳入第一阶段范围已明确

### Provider 兼容性

- [ ] `moonshot-v1-32k-vision-preview` 的 `chat_completions` 请求通过
- [ ] `kimi-k2.6` 的 temperature/thinking 参数有专门测试后再启用
- [ ] DeepSeek 主模型和 Kimi helper 使用各自的 API key，不发生凭据串用

## 8. 已完成验证

基于当前 fork 的源码和测试：

| 验证项 | 结果 |
|---|---|
| `cargo check --locked -p xai-grok-shell --lib` | 通过 |
| `cargo test --locked -p xai-grok-shell --test test_image_strip_recovery` | 1 passed |
| `cargo test --locked -p xai-grok-pager --lib acp::model_state::tests` | 13 passed |
| `cargo test --locked -p xai-grok-shell --lib image_` | 102 passed |
| 回环 HTTP：视觉 helper 收到 Base64，文本模型 history 无图片 part | 通过 |
| `cargo clippy --locked -p xai-grok-shell --lib -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `git diff --check` | 通过 |

`image_describe` 单测最初被测试文件缺少 `use base64::Engine;` 阻断，已补充该 import 后恢复通过。该修复只影响测试编译，不改变生产行为。

尚未完成真实 provider 端到端验证，原因是本次扫描未读取或使用 DeepSeek/Kimi 真实 API key。上线前应使用隔离测试 key 验证：Kimi 图片请求、DeepSeek 纯文本主请求、错误重试和同 session 模型切换。

## 9. 参考路径

### 本地源码

- [`image_describe.rs`](../../crates/codegen/xai-grok-shell/src/session/image_describe.rs)
- [`turn.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs)
- [`interjection.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/interjection.rs)
- [`session_mode.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/session_mode.rs)
- [`model_switch.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/model_switch.rs)
- [`config.rs`](../../crates/codegen/xai-grok-shell/src/agent/config.rs)
- [`model_state.rs`](../../crates/codegen/xai-grok-pager/src/acp/model_state.rs)
- [`default_models.json`](../../crates/codegen/xai-grok-models/default_models.json)
- [`tool_calls.rs`](../../crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs)
- [`jsonl/mod.rs`](../../crates/codegen/xai-grok-shell/src/session/storage/jsonl/mod.rs)

### 外部文档

- [Kimi Vision Model](https://platform.kimi.ai/docs/guide/use-kimi-vision-model)
- [Kimi Model List](https://platform.kimi.ai/docs/models)
- [Kimi API Overview](https://platform.kimi.ai/docs/api/overview)
- [Migrating from OpenAI to Kimi API](https://platform.kimi.ai/docs/guide/migrating-from-openai-to-kimi)
- [Grok Build README](../../README.md)
