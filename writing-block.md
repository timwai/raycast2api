# Raycast2API 账号级 Mihomo 代理管理模块设计

- 项目：timwai/raycast2api
- 版本：V1.1，设计评审修订稿
- 日期：2026-11-25
- 技术栈：Rust + Tokio + Axum + SQLite + Mihomo
- 状态：V1 已实现并完成本地测试；真实订阅和生产发布仍按第 15 节验收

## 1. 评审结论与修订记录

保留“一个专用 Mihomo 进程、每个绑定账号一个本地入口、每个账号固定一个节点”的总体方案。一个账号最多绑定一个节点，多个账号可以共享节点。未绑定账号继续使用现有全局网络策略。

原稿的主要问题及本稿决策如下。以下均为目标设计，不代表已经实现或运行验证。

| 级别 | 原设计问题 | 修订决策 |
|---|---|---|
| P1 | 选择组可能默认选中静态 REJECT；空组默认回退也不安全 | 单节点精确过滤，显式默认选择和空组拒绝，禁止恢复历史选择，发布前回读验证 |
| P1 | 仅替换普通请求客户端会遗漏预签名 PUT、SSE 重试后的图片下载 | 实际账号、响应、传输租约一起传递，覆盖完整请求链路 |
| P1 | 文件替换、核心重载和 SQLite 提交被视作一个事务 | 操作日志、不可变代次、请求屏障、可重复启动恢复 |
| P1 | 模块关闭、节点消失、订阅停用时可能误判为未绑定 | 保留绑定并阻断；仅显式解绑恢复全局策略 |
| P1 | 自动健康检查与“不主动调用接口”的要求冲突 | 关闭全部自动节点探测，普通账号调用继续受调度限制，仅必要 OAuth 续期例外 |
| P1 | 删除缓存不能关闭已有客户端克隆和 TCP 隧道 | 配置切换前暂停新租约、排空请求和连接，再复用端口 |
| P1 | 新路径未加入当前管理鉴权分类 | 同时修改路由注册和鉴权分类，拒绝托管推理 key |
| P2 | 节点表未保存完整配置，外键也没有实际启用保证 | 加密保存完整节点配置，每个 SQLite 连接启用并检查外键 |
| P2 | 邮箱主键、节点身份和端口回收缺少边界 | 使用 accounts.id；稳定匹配、缺失墓碑、排空后回收 |
| P2 | 版本、资源限制、密钥格式和故障状态不够具体 | 固定核心及摘要，定义输入边界、加密封装和错误语义 |

评审依据为当前仓库相关源码、Mihomo 官方文档及固定版本实现。实施前先完成核心契约原型，不能只凭 YAML 语法正确进入发布。

## 2. 功能范围与不可变约束

### 2.1 功能范围及默认值

支持多个 Clash/Mihomo YAML 订阅的增删改、停用、手动/定时刷新；节点统一搜索、分页、订阅/地区筛选和手动测速；账号独立绑定、恢复与异常展示。

| 项目 | 默认值 |
|---|---|
| 核心 | Mihomo v1.19.34 |
| 自动刷新 | 启用订阅每 21600 秒，加入少量抖动 |
| 账号入口 | 127.0.0.1，端口 18000–18999，共 1000 个 |
| 控制器 | 127.0.0.1:19090 |
| 自动节点测速 | 禁用 |
| 节点故障 | 失败关闭，不切换出口 |
| 系统网络 | 不接管系统代理、TUN、透明代理或 DNS |
| 未绑定账号 | 保留现有全局网络策略 |

订阅没有固定业务数量上限，但下载、解析、总节点数和内存使用必须有可配置的资源上限，不能理解成无限资源。

### 2.2 账号状态与必要续期

代理模块不改变现有账号调度：只在当前最高可调度优先级内轮询；有可用高优先级账号时，较低优先级账号待命。绑定代理不使停用、冷却或待命账号恢复普通上游调用资格。

| 动作 | 停用、冷却、待命账号 | 已绑定账号出口 |
|---|---|---|
| 聊天、模型、/me、额度、试用及普通自动刷新 | 禁止主动调用，保留现有资格检查 | 唯一绑定节点 |
| 已确认 401 后的必要 OAuth 续期 | 按现有续期规则处理 | 唯一绑定节点 |
| 已知 token 距到期不超过 60 秒 | 允许仅续期 | 唯一绑定节点 |
| 到期时间未知 | 不探测、不盲目轮换 | 不产生额外请求 |
| 用户显式手动测速 | 独立管理动作，不携带账号凭据 | 指定测试节点 |
| 订阅定时下载 | 独立控制面，不携带账号凭据 | 明确配置的下载策略 |

延续现有 30 秒本地到期扫描、逐账号续期串行化、失败后 5 分钟退避、相同凭据 invalid_grant 后停止重试等机制。续期只原子更新 token 对及到期信息，不重置 enabled、priority、available、冷却和原因，不附带 /me、额度等调用。

OAuth 会轮换凭据，代理错误不能触发跨出口重放。已发送但响应丢失的续期不能按无副作用请求自动重试。绑定出口不可用时记录失败和可见提示，不能为避免过期而绕过绑定。

账号池按当前本地实例独立维护，续期结果保存到该实例数据库。开发与验收使用假上游和专用测试凭据验证轮换及持久化。

### 2.3 失败关闭的准确含义

有绑定时只能使用该绑定已确认生效的入口，禁止回退到环境代理、原全局代理、DIRECT、COMPATIBLE、其他节点或其他账号。代理基础设施错误直接返回独立错误，不触发账号切换。

收到真实 Raycast 应用响应后，才适用原有 401、429 等账号业务处理；因此重选账号时必须重取新账号租约。代理 CONNECT 握手中的 401、407、429 不得解释为 Raycast 账号状态。

这里保证账号业务流量的出口。关闭 Mihomo DNS 服务不等于不发生 DNS 查询，代理服务器域名仍可能经系统解析；不宣称所有 DNS 报文也通过绑定节点。

## 3. 架构与进程边界

~~~mermaid
flowchart TB
    API[账号业务请求] --> D[现有账号调度与资格检查]
    D --> R[AccountProxyRouter]
    R --> U[无绑定：现有全局策略]
    R --> L[绑定：AccountTransportLease]
    L --> C[Got 与 ChatFetch 客户端]
    C --> P[127.0.0.1:账号端口]
    P --> M[专用 Mihomo 进程]
    M --> N[唯一绑定节点]
    S[订阅下载与解析] --> O[配置应用协调器]
    DB[(SQLite 与操作日志)] <--> O
    O --> M
    O --> R
~~~

Raycast2API 管理订阅、节点、绑定、端口、客户端和核心生命周期；Mihomo 负责协议连接与转发。控制器仅后端可用，不提供前端透传接口。

V1 要求同一数据库/工作目录只有一个代理管理实例。取得操作系统文件锁后才能启动核心、修改配置，第二实例启动失败；SQLite 写锁不能代替核心和端口所有权锁。

关闭 TUN、系统代理设置、透明代理、DNS 服务、嗅探、NTP 和自动地理数据库更新；使用本地 file Provider，不让核心自行下载订阅。所有入口显式监听 127.0.0.1，容器不发布内部端口。

同主机/同容器其他进程不属于入口隔离的安全边界；采用专用容器，不向不可信进程共享控制器或代理入口。

## 4. 数据模型

SQLite 保存可重建完整运行配置的业务事实；配置文件为派生产物。时间统一 UTC Unix 秒，延迟单位毫秒。内部随机 ID 采用固定 ASCII 字符集，不使用订阅名、节点名或邮箱。

### 4.1 proxy_subscriptions

| 字段 | 说明 |
|---|---|
| id TEXT PRIMARY KEY | 稳定随机 ID |
| name TEXT NOT NULL | 展示名 |
| url_ciphertext BLOB NOT NULL | 带版本、key_id、nonce 的 AEAD 密文 |
| enabled INTEGER NOT NULL | 0/1；停用停止刷新并阻断其绑定 |
| refresh_interval_secs INTEGER NOT NULL | 默认 21600，最小 300 |
| revision INTEGER NOT NULL | 乐观并发版本 |
| last_attempt_at / last_success_at | 最近尝试/成功时间，可空 |
| last_error_code / last_error_message | 脱敏错误，可空 |
| created_at / updated_at | 时间 |

### 4.2 proxy_nodes

| 字段 | 说明 |
|---|---|
| id TEXT PRIMARY KEY | 稳定随机 ID |
| subscription_id TEXT NOT NULL | 订阅外键 |
| mihomo_name TEXT NOT NULL UNIQUE | rpnode_<id> |
| original_name / protocol / server / server_port | 展示与身份匹配元数据 |
| identity_key TEXT NOT NULL | 订阅内稳定匹配键 |
| config_ciphertext BLOB NOT NULL | 完整规范化节点配置密文，不仅存地址和端口 |
| config_fingerprint TEXT NOT NULL | 规范化内容 HMAC，不对外返回 |
| presence TEXT NOT NULL | present / missing，不等同网络健康 |
| delay_ms / delay_checked_at / delay_error_code | 手动测试结果，可空 |
| country / country_source | 地区标签及来源；未知可空，不保证实际地理位置 |
| first_seen_at / last_seen_at / created_at / updated_at | 时间 |

约束 UNIQUE(subscription_id, identity_key)。订阅外键 ON DELETE RESTRICT。仍有绑定的缺失节点保留墓碑，不能删除后使绑定变成未绑定。

### 4.3 account_proxy_bindings

| 字段 | 说明 |
|---|---|
| account_id BIGINT PRIMARY KEY | 引用现有 accounts.id，删除账号级联；不用邮箱作主键 |
| node_id TEXT NOT NULL | 节点外键，ON DELETE RESTRICT |
| listener_port INTEGER NOT NULL UNIQUE | 稳定端口 |
| group_name / listener_name | 按账号 ID 生成的唯一内部名称 |
| revision INTEGER NOT NULL | 成功修改后递增 |
| created_at / updated_at | 时间 |

运行状态由已提交绑定、配置代次和核心状态计算，返回 applying、active、blocked、degraded 及原因，不作为另一份可独立改写的绑定事实。

账号删除须经协调器：阻止新租约、结束相关请求和连接，再提交删除及配置变更；SQL 级联不能代替运行资源清理。邮箱变化不重建端口。

### 4.4 配置代次、日志和端口预留

- proxy_runtime：单行 applied_generation、配置摘要、应用时间。
- proxy_apply_operations：id、调用者范围、幂等键、请求摘要、加密请求/候选快照、base_generation、target_generation、phase、脱敏错误和时间；准备完成前目标代次和候选可空。
- phase：queued → preparing → prepared → runtime_applied → committed，或 failed / rolled_back / recovery_required。
- proxy_port_reservations：port 主键、账号 ID、操作 ID、状态；未提交/未排空端口也不可再分配。

只有持久化 queued 操作及幂等记录后才能返回 202；重启后恢复排队任务，重新校验版本再执行。候选事实先放操作日志，只有最终提交时才在同一个 SQLite 事务内修改业务表、端口和 applied_generation。任何操作引用的历史配置都不能提前清理。

generation 指配置代次；另有仅运行期使用的 core_epoch，每次核心重启递增。路由快照和租约同时记录两者，防止同一配置重启后复用旧连接。

当前 Db::open 只设置 WAL 和 busy timeout。实施时每个连接须在事务外执行并检查 PRAGMA foreign_keys = ON；迁移前检查已有数据，迁移后执行 foreign_key_check，建表、索引和 Diesel schema 同步更新。

## 5. 订阅和节点语义

### 5.1 格式与协议

V1 接收顶层包含非空 proxies 数组的 Clash/Mihomo YAML，不导入 rules、dns、tun、listeners、proxy-groups、Provider、脚本和控制器设置。

支持固定核心内能独立运行、已通过兼容性样例的出站协议，保留必要认证、TLS、传输参数。至少验证 HTTP、SOCKS5、SS、VMess、VLESS、Trojan、Hysteria2、TUIC；其他协议按同样契约纳入，不笼统承诺所有类型和任意参数。

拒绝内置直连/放行策略、dialer-proxy 链、外部文件引用、本地插件执行、改变控制面的参数。未知协议或不能安全保留的必要参数使整个候选失败，不能静默丢字段再发布。URI/Base64/Surge 格式不支持。

使用有资源限制的 YAML 解析器，拒绝重复映射键、自定义标签和过度别名展开。仅替换节点 name 为内部名称；允许参数保留。最终候选还须通过固定核心配置校验。

### 5.2 下载边界

独立控制面客户端下载订阅，不使用账号绑定或账号 token，不继承未显式配置的环境代理。默认 direct，可明确选择现有 global 策略，下载失败不更换出口。

默认仅 HTTPS 并验证证书，最多 3 次重定向，每跳重新验证。默认拒绝 URL 用户信息及回环、私网、链路本地、元数据服务地址；局域网订阅通过运维白名单开放。实际连接使用已校验的解析地址，避免检查后重新解析绕过限制。

默认超时 20 秒、解压后上限 10 MiB、单订阅最多 10000 节点、下载并发 3；另设 YAML 深度、字段长度和总节点数上限。所有列表分页。

错误只记录订阅 ID、阶段和脱敏信息，不输出完整 URL、查询串、响应正文或密码，不原样转发订阅错误响应。

### 5.3 稳定身份

同一订阅内，以原始名称、规范化协议、服务器和端口形成 identity_key。只做明确的协议/域名规范化，不忽略有语义的大小写。跨订阅同名节点独立。

- 匹配键相同：保留 ID；密码、TLS 等内容变化更新配置，经完整应用流程生效。
- 名称、协议、服务器或端口改变：新增节点；未绑定的旧节点删除，仍有绑定的旧节点标记 missing，原绑定不自动迁移。
- 同订阅重复原始名称或匹配有歧义：拒绝候选，保留上次成功版本。
- 有效新订阅移除节点：删除未绑定的旧节点；仅为仍有绑定的节点保留墓碑和绑定，新配置拒绝该入口。解绑、换绑或删除账号后清理已无引用的墓碑。
- 已绑定缺失节点重新出现且键相同：恢复原 ID，验证应用后恢复绑定。普通节点列表默认只展示当前订阅节点，可切换查看失效绑定节点。

内容摘要采用主密钥派生的独立 HMAC 密钥；规范化映射键顺序但保留数组语义。摘要不作节点 ID，避免密码轮换使绑定丢失。

### 5.4 刷新、停用与删除

手动/自动刷新同一流程，同订阅串行；不同订阅可并发下载，配置应用全局串行，应用前重查 revision。失败不紧密重试。每次成功、失败或调整计划后，在正常周期后增加 1 秒至周期 10%（最多 300 秒）的随机错峰，持久化 next_refresh_at，重启不重新抽取已有计划。

网络错误、无效 YAML、空列表、资源超限、核心校验失败均保留上次已提交数据和运行版本。有效候选缺失节点则按上节阻断，不能与下载失败混为一谈。

停用订阅停止下载并阻断全部原绑定，保留数据/端口。重新启用须先验证上次成功配置；没有成功版本则保持阻断并提示刷新。删除仍有绑定节点的订阅返回 409，先显式解绑/换绑；V1 不隐式批量解绑。

## 6. 固定核心与配置生成

### 6.1 版本锁

实施基线为 v1.19.34，运行时不下载 latest。下列摘要为评审日上游 release API 返回的 gzip 文件 SHA-256，不是解压后二进制摘要；实施时写入锁文件并在构建中校验。

| 目标 | 资源 | SHA-256 |
|---|---|---|
| linux/amd64 | mihomo-linux-amd64-compatible-v1.19.34.gz | 2763f5b414527f7baf6dc57e3f82f3ab561eedf34bfd7c2f2cde1f7e592efe06 |
| linux/arm64 | mihomo-linux-arm64-v1.19.34.gz | daaf78a6240a282cdfd75bcbf98fe321f6bd52765cc29e79f344191198f0b50f |

来源：[官方 v1.19.34 release](https://github.com/MetaCubeX/mihomo/releases/tag/v1.19.34)。这是版本/完整性锁定，不替代来源审查。升级须同步更新摘要、配置契约与真实核心测试。

### 6.2 文件布局

~~~text
store/mihomo/
├── manager.lock
├── controller.secret
├── generations/
│   └── <generation>/
│       ├── config.yaml
│       └── providers/
│           └── <subscription_id>.yaml
└── logs/
~~~

代次文件不可变；Provider 使用该代次绝对路径，位于核心 -d 工作目录内，不依赖 shell 工作目录。Provider 仅包含完整节点配置，不保存导入的控制面设置。

目录 0700，配置、Provider、secret 文件 0600。派生 YAML 含解密凭据，备份和挂载同样按敏感数据处理；日志脱敏轮转，不将运行文件提交到 Git。

### 6.3 正常配置

以下为生成器目标示例，占位 secret/路径必须替换；只建立账号 listener，不建立公共代理入口。

~~~yaml
port: 0
socks-port: 0
mixed-port: 0
redir-port: 0
tproxy-port: 0
allow-lan: false
mode: rule
log-level: warning
external-controller: 127.0.0.1:19090
secret: "<generated-controller-secret>"
geo-auto-update: false
tun:
  enable: false
dns:
  enable: false
sniffer:
  enable: false
ntp:
  enable: false
profile:
  store-selected: false
  store-fake-ip: false
proxy-providers:
  rp_sub_demo:
    type: file
    path: "/app/store/mihomo/generations/g42/providers/sub_demo.yaml"
    health-check:
      enable: false
proxy-groups:
  - name: rp_acc_123
    type: select
    use: [rp_sub_demo]
    filter: "^rpnode_ab12$"
    default-selected: rpnode_ab12
    empty-fallback: REJECT
    interval: 0
listeners:
  - name: rp_listener_123
    type: mixed
    listen: 127.0.0.1
    port: 18000
    proxy: rp_acc_123
rules:
  - MATCH,REJECT
~~~

正常组不插入静态 REJECT 成员，精确 filter 仅由转义的内部 ID 生成。发布前成员集合必须严格等于目标节点，当前选择也必须是目标节点。

缺失/停用节点直接生成下列拒绝组，保留同一 listener 和端口：

~~~yaml
proxy-groups:
  - name: rp_acc_123
    type: select
    proxies: [REJECT]
    default-selected: REJECT
    empty-fallback: REJECT
    interval: 0
~~~

空组默认回退是 COMPATIBLE，而该内置策略等价于直连，必须显式覆盖。参见[官方组配置](https://wiki.metacubex.one/en/config/proxy-groups/)、[内置策略](https://wiki.metacubex.one/en/config/proxies/built-in/)及固定版本 [parser.go](https://github.com/MetaCubeX/mihomo/blob/v1.19.34/adapter/outboundgroup/parser.go)、[selector.go](https://github.com/MetaCubeX/mihomo/blob/v1.19.34/adapter/outboundgroup/selector.go)。

### 6.4 禁止自动探测

Provider health-check.enable 为 false；组不配置健康检查 URL，interval 为 0；不采用 url-test、fallback、load-balance，不用 lazy: true 代替禁用。首次加载、重载、失败触发等隐式探测也要通过真实核心测试证明没有外联。

Provider 和组的健康检查是不同入口，依据[官方 Provider 配置](https://wiki.metacubex.one/en/config/proxy-providers/)及固定版本 [healthcheck.go](https://github.com/MetaCubeX/mihomo/blob/v1.19.34/adapter/provider/healthcheck.go)。允许本地控制器状态轮询，不通过访问 Raycast/外部站点判断核心健康。

Listener 的 proxy 必须是存在的组名且不为空，listen 显式为 127.0.0.1；依据[官方 listener 字段](https://wiki.metacubex.one/en/config/inbound/listeners/)。全局 MATCH,REJECT 同时拒绝误入规则引擎的请求。

## 7. 配置应用与崩溃恢复

### 7.1 状态机

SQLite、文件系统、核心不存在跨三者的 ACID 事务。V1 使用完整代次重载，不将 PUT Provider 成功等同于组、listener、数据库都已生效。

刷新、绑定、解绑、订阅停用、账号删除等影响运行配置的操作均经过协调器。仅改展示名称或刷新周期等元数据时，在版本校验后用本地事务提交，无需重载；首次创建订阅也不以“初次刷新已成功”作为响应含义。

配置应用流程：

1. 下载/解析等准备在应用锁外完成，不持有 SQLite 锁等待网络。
2. 取得全局应用锁，校验 expected_revision、base_generation、账号/节点存在性；冲突拒绝。
3. 预留端口，持久化 prepared 操作和候选快照；写新代次临时文件，完成同步后原子改名并记录摘要。
4. 使用固定核心执行配置校验：mihomo -t -d <work_dir> -f <candidate_config>。失败释放预留，旧版不变。
5. 关闭全部已绑定账号及本次受影响账号的新租约入口。首次绑定涉及的原未绑定账号也须排空；其他未绑定账号不受影响。
6. 等待旧请求结束，按第 8 节取消超时的普通请求；已发送 OAuth 先完成结果处理及持久化。若仍有租约无法结束，本次应用失败并恢复旧入口。
7. 关闭旧客户端池及本专用核心残留连接，确认归零后，通过 PUT /configs?force=true 加载候选。
8. 验证控制器身份、Provider 成员、组成员/当前选择、实际 listener 绑定；必要时显式选择再回读，不能只凭 204。写 runtime_applied。
9. 同一 SQLite 事务提交业务表、端口、applied_generation 与 committed；原子发布新路由快照，再开放已确认入口。

控制器重载、查询、选择和连接管理以[官方 API](https://wiki.metacubex.one/en/api/)为准。路径仅来自受控工作目录，前端不能传任意核心 payload。API 先返回操作 ID，只有操作 committed 才表示生效。

### 7.2 故障与启动恢复

| 故障阶段 | 恢复方式 |
|---|---|
| 核心应用前 | 保留旧版，取消候选和预留 |
| 核心应用后、数据库提交前 | 屏障关闭，重新加载并验证旧已提交代次，成功才回滚并恢复入口 |
| 回滚也失败 | recovery_required，绑定请求持续 503 |
| 数据库已提交、路由发布前崩溃 | 重启按已提交代次重建并验证，再开放 |
| 派生文件丢失或核心状态不明 | 从完整业务数据重建，验证前保持阻断 |

启动顺序：实例锁 → SQLite 和未结束操作 → 加载已提交代次 → 核心验证 → 路由发布。prepared/runtime_applied 但未提交的候选不自动晋升；不能猜测上次控制器调用结果。

每阶段幂等、可重复恢复；至少保留当前和上一成功代次，待恢复操作/租约引用的代次不得清理。queued/preparing 可恢复执行；未提交的 prepared/runtime_applied 先恢复旧事实，不能自动晋升。总应用超时从关闭新租约入口开始计算，只触发回滚/恢复，不直接释放屏障。

### 7.3 端口

在 BEGIN IMMEDIATE 短事务中选区间最小空闲端口并预留，依靠唯一约束处理并发。控制器端口不得与账号区间重叠。

数据库预留不代表系统监听成功，须核心实际绑定确认。临时 bind 探测不能消除随后抢占。新分配冲突可在候选阶段尝试下一端口；已有绑定重启遭占用则阻断，不改用外来进程或擅自换端口。

换绑保留端口；解绑/删除须等旧请求、客户端和连接结束后才能回收。端口耗尽返回明确错误，不无限扫描。

## 8. 请求上下文、租约和切换

### 8.1 类型与缓存

AccountProxyRouter 返回三种明确结果：

- Unbound：现有全局策略。
- Bound(AccountTransportLease)：已验证生效的唯一入口。
- Blocked(reason)：持久化绑定存在但不可用，直接拒绝。

不能用 Option<Client> 混淆未绑定与绑定失效。所有账号请求都登记生命周期；即使原来未绑定，也能在首次绑定时等待其旧请求结束。

租约至少携带 account_id、binding_revision、generation、core_epoch、Got/ChatFetch 两类客户端及生命周期/取消句柄。缓存按账号、绑定版本、配置代次和核心运行代次区分，响应体及后续图片处理完成前不释放。

客户端复用 src/proxies.rs 的 base_builder，显式 no_proxy 后仅设置 http://127.0.0.1:<port>。保留原有超时、TLS、ALPN、连接池、签名及请求头；绑定路径不能进入全局 fallback 列表。

控制器客户端独立构造，仅本地直连并携带 secret，避免通过账号入口形成循环。

### 8.2 完整链路

| 链路 | 当前切入点 | 必须满足 |
|---|---|---|
| 聊天 | AccountDispatcher::send_chat、RaycastClient::chat | 资格检查后取租约；重选账号重取 |
| 文件注册 | upload_file 中 POST | 当前账号 Got 传输 |
| 预签名上传 | upload_file 当前 self.http.execute 的 PUT | 同样走租约，URL 不同不回全局 |
| 模型/用户/额度/试用 | fetch_models、fetch_me、credits/trial、quota_refresh | 先保留资格检查，再选路 |
| OAuth | token_renewal → oauth_token | 显式账号上下文，必要续期授权，唯一出口 |
| 生成图片下载 | raycast/stream.rs::decode_events 等 | 使用产生响应的实际账号租约 |
| SSE 重试 | dispatcher::events、canonical_events_with_retry | 响应和租约一起更新 |

聊天内部返回值建议为 AccountResponse { response, transport_lease }，重发闭包也返回该封装。当前只返回 Response、在外层提前捕获全局 fetch_channels 的做法不能保留，否则重试换账号后图片仍走旧出口。

附件处理当前存在记录错误后跳过附件的路径；代理基础设施错误必须冒泡并终止请求，不能丢附件后继续聊天。普通附件格式错误按原产品规则处理。

### 8.3 排空与锁顺序

下载和候选校验时照常服务；真正应用时短暂暂停全部绑定账号及本次受影响账号的新租约。

删除缓存不能结束客户端克隆、连接池或既有隧道。必须跟踪完整请求生命周期，等待旧流结束。超过宽限期可取消普通聊天/下载并明确返回中断；保留“已经输出后不重放生成”的现有规则。取消后仍未排空，则放弃变更，不能冒险重载。

已发送 OAuth 不强制取消，须处理响应并原子保存 token 后才能切换；等待超过总应用预算则本次变更失败，继续旧版本。新续期在入口关闭后等待或延后，不盲目轮换。

锁顺序：协调器取得全局应用锁 → 关闭新租约入口 → 等待已有租约。续期持账号锁及租约时不得反向等待应用锁；获取失败应释放/等待后重试本地条件检查。任何数据库锁都不跨网络等待。

旧连接结束后再重载；committed 表示后续请求使用新版本，不承诺 TCP 会话无中断迁移。界面提交前提示配置应用可能中断在途的绑定账号请求。

绑定账号的 SSE 异常截断不自动重放，以免代理连接故障触发账号轮换。未绑定路径保留原有输出前重试，并从重试响应更新图片传输上下文。绑定图片下载失败返回错误，不让客户端抓取原始 URL 绕过服务端出口。

### 8.4 错误来源

代理入口连接、CONNECT、TLS 和真实上游响应应有不同错误类型/阶段。proxy_connect_failed、proxy_route_blocked、proxy_unavailable 不改额度、冷却或 token 拒绝状态，不重选账号；不把代理故障写成 invalid_grant。

## 9. 生命周期与停用语义

启用前校验核心版本、密钥、权限、地址区间、实例锁和可恢复数据。配置错误使启用失败；运行中核心异常进入 degraded，绑定请求 503，未绑定按原策略。

控制器使用高熵 secret，保存在受控文件。就绪需同时验证版本、鉴权及本实例进程身份，不能把端口上已有服务当作可接管核心。处理遗留进程须验证 PID 启动时间、可执行文件和工作目录，不仅凭 PID 文件发送信号。

子进程退出后按 1、2、4、8 秒至最多 60 秒退避恢复。重新启动递增 core_epoch，使旧租约/客户端失效；已提交配置重新加载并回读后才开放。关闭所有旧请求连接，不让旧代次继续在新核心上发送。

启用 Tokio process/fs 能力。退出先停止调度和新租约，处理已发送 OAuth 的结果，再终止并回收本实例核心；容器可使用 init 转发信号，不能遗留孤儿进程。

模块 enabled=false 但仍有绑定时返回 proxy_manager_disabled，绝不能当作未绑定。允许管理端读取并显式解绑：仅修改绑定及清理本地资源，不需要解密订阅内容；重新启用时按已提交数据完整重建并验证。丢失密钥时不得启动绑定流量或覆盖密文。

/health 表示进程存活；/ready 根据可服务账号容量判断，不因单节点失效必然整体失败。/v1/proxy/status 返回核心、恢复阶段、代次及可用/阻断绑定数量，不访问外部站点判定就绪。

## 10. API 与管理界面

### 10.1 权限、并发和生效状态

将 /v1/proxy/ 加入 src/server/middleware/auth.rs 的管理路径分类：允许配置的主 API key 或控制台会话，拒绝托管推理 key。路由注册不能代替权限分类；前端不接触控制器 secret、节点密码和任意控制接口。

修改带 expected_revision，冲突 409。界面可传 email，服务端解析现有 accounts.id 后存储与加锁。创建绑定 expected_revision=0，更新/解绑使用当前版本。

配置变更排队返回 202 和 operation_id，前端查询状态；仅 committed 显示成功。结果含 generation、revision、脱敏错误。Idempotency-Key 按调用者和接口去重；同键不同请求体 409，超时重发不重复执行。前端为每次变更生成随机键，网络异常/5xx 后手动重试复用原键，接受成功后下次主动操作换新键，退出登录清空本会话缓存。后端将已认证调用者、固定接口名及原键摘要为不含凭据的范围键；合并到现有刷新/测速任务时也保存请求键与请求摘要的关联。

列表默认 50、最大 200。401/403、404、409、422、503 分别用于鉴权、对象不存在、冲突、输入问题和运行不可用。刷新全部返回批次操作，轮询 result.items 查看逐订阅结果，result.completed/total/succeeded 表示进度。committed 只表示批次处理完毕，需逐项检查 success。下载/测速并发配置范围 1–32、默认 3，互相独立；配置应用串行。

### 10.2 接口

| 方法 | 路径 | 行为 |
|---|---|---|
| POST | /v1/proxy/subscriptions/list | 分页列表与刷新状态 |
| POST | /v1/proxy/subscriptions/create | 创建，URL 只写；初次刷新独立操作 |
| POST | /v1/proxy/subscriptions/create-batch | 每批 1–50 项，逐项校验并保存；首次刷新独立排队 |
| POST | /v1/proxy/subscriptions/update | 名称/URL/周期/启用；URL 留空不变 |
| POST | /v1/proxy/subscriptions/delete | 有绑定 409，协调器删除 |
| POST | /v1/proxy/subscriptions/refresh | 单个启用订阅刷新；停用时冲突 |
| POST | /v1/proxy/subscriptions/refresh-all | 启用订阅子操作列表 |
| POST | /v1/proxy/nodes/list | 订阅、地区标签、关键词、presence 与延迟筛选排序 |
| POST | /v1/proxy/nodes/detail | 脱敏元数据，不返回原 YAML |
| POST | /v1/proxy/nodes/test | 显式单节点测试，202 返回操作对象 |
| POST | /v1/proxy/nodes/test-batch | 每批 1–20 项，去重并合并单节点任务，202 返回批次 |
| POST | /v1/proxy/nodes/test-all | 全部启用订阅当前节点；按配置并发分批排队，202 返回批次 |
| POST | /v1/proxy/bindings/list | 绑定版本、状态、原因 |
| POST | /v1/proxy/bindings/set | 新建/换绑操作 |
| POST | /v1/proxy/bindings/delete | 显式解绑操作 |
| POST | /v1/proxy/operations/detail | 排队/应用/提交/失败/恢复 |
| GET | /v1/proxy/status | 运行状态 |

~~~json
{
  "email": "account@example.com",
  "node_id": "ab12",
  "expected_revision": 0
}
~~~

现有账号列表/详情增加 proxy_binding，含节点、订阅名称、端口、revision、generation、运行状态和原因。null 只表示没有持久化绑定，不能表示核心失效。

### 10.3 手动测速

一键全量测试不受列表筛选、分页和勾选 20 项上限影响；跳过缺失节点和停用订阅，同节点任务仍合并。全量操作只保留与手动测速并发数相同的在途子任务窗口。结果包含 completed/total/succeeded 和逐项结果。

新建/更换绑定只允许 delay_checked_at 存在、delay_ms > 0 且无 delay_error_code 的节点。配置指纹变化后旧测试结果失效，必须重新测试。已有绑定测试失败不自动解绑或更换，账号卡片显示红色失败提示和最近测试时间；重测通过后清除。

只在显式操作时测试，不带账号 token。服务端固定测试目标与超时，默认 https://www.gstatic.com/generate_204、5000 ms，并发 3，每次最多 20 节点；同节点重复任务合并。单节点与批量均先持久化再返回 202，完成后可发起新一轮测试。

调用单节点 GET /providers/proxies/{provider}/{node}/healthcheck，正确编码路径/参数；不用 Provider 级 healthcheck 隐式测试全部。前端不能提供任意目标 URL。节点缺失、订阅停用或核心不可用时拒绝测试。

延迟仅反映该时刻该 URL 可达性，不等于 Raycast 可用；未知/超时不自动停用账号、不触发其他节点。

### 10.4 UI

实际导航是 src/page/views.js，不是原稿中的 src/page/app.js。新增 views/proxy.js，修改 views/accounts.js。

- 订阅：只写 URL 替换、脱敏地址、启用/周期、节点数、最近成功、错误；批量添加逐项反馈。
- 节点：分页、订阅/协议/地区标签与来源、presence、手动延迟及绑定数。
- 账号：按订阅分组选择；“不绑定，使用现有全局策略”明确表示解绑。
- 异常：保留原绑定并展示阻断原因，不显示成未绑定。
- 操作：区分已接受、应用中、已生效、恢复中、失败；提示在途请求可能中断。

## 11. 加密与配置

### 11.1 密钥约定

PROXY_ENCRYPTION_KEY 是 Base64 编码、解码后恰好 32 字节的随机主密钥，不是至少 32 个字符。使用 AES-256-GCM、随机 96 位 nonce，每次写入新 nonce；封装含版本和 key_id，AAD 绑定记录类型、稳定 ID、封装版本。

加密订阅 URL、完整节点配置、候选操作快照；摘要使用独立派生 HMAC 密钥。不在日志、数据库明文字段或前端保存主密钥。首次启用确认既有密文可解密；错误密钥不能按空数据继续。

备份须安全保存密钥与数据库，派生 YAML 同为敏感数据。V1 密钥轮换采用停机、备份、重新加密、校验再切换的运维流程，在线轮换不在范围内。

### 11.2 配置示例

~~~json
{
  "proxy_manager": {
    "enabled": false,
    "binary_path": "/usr/local/bin/mihomo",
    "expected_version": "v1.19.34",
    "work_dir": "./store/mihomo",
    "controller_addr": "127.0.0.1:19090",
    "port_start": 18000,
    "port_end": 18999,
    "refresh_interval_secs": 21600,
    "subscription_download_route": "direct",
    "subscription_timeout_secs": 20,
    "subscription_max_bytes": 10485760,
    "subscription_max_nodes": 10000,
    "subscription_concurrency": 3,
    "subscription_allowed_private_hosts": [],
    "manual_test_url": "https://www.gstatic.com/generate_204",
    "manual_test_timeout_ms": 5000,
    "manual_test_concurrency": 3,
    "apply_drain_timeout_secs": 15,
    "apply_timeout_secs": 60
  }
}
~~~

默认关闭兼容无绑定旧配置；有绑定时按第 9 节阻断。默认值及校验只定义一处，CLI、示例、UI 一致。总应用超时后操作仍须恢复到明确状态，查询接口持续反映结果。

## 12. 实施文件清单

本节为开发规划，本次文档评审没有修改这些业务文件。

~~~text
src/proxy_manager/
├── mod.rs
├── manager.rs       # 协调、屏障、操作和恢复
├── mihomo.rs        # 进程、控制器、生成与回读
├── subscription.rs  # 下载、解析、刷新
├── node.rs          # 身份、墓碑、手动测速
├── binding.rs       # 绑定、端口、资源清理
├── router.rs        # 租约、客户端、生命周期
├── store.rs         # 表、事务、日志
├── crypto.rs        # 密文封装和摘要
└── wire.rs          # API 类型及脱敏
~~~

| 现有/新增文件 | 变更 |
|---|---|
| src/proxies.rs | 复用 base_builder，两类绑定客户端 |
| src/raycast/client.rs | 聊天、注册、预签名 PUT、查询传递租约 |
| src/raycast/transport.rs | 绑定无全局 fallback；OAuth 账号上下文及错误阶段 |
| src/raycast/stream.rs | 首次/重试响应携带租约，覆盖图片 |
| src/account/dispatcher.rs | 资格检查后选路，代理错不换账号，不吞附件代理错 |
| src/account/token_renewal.rs | 必要续期租约、串行、退避、状态保留 |
| src/account/quota_refresh.rs | 保留资格检查，完整出口覆盖 |
| src/account/store.rs | 删除经协调器，保留账号策略 |
| src/storage/db.rs、schema.rs | 外键、迁移、索引及新表 |
| src/server/state.rs | 注入 Manager/Router |
| src/server/middleware/auth.rs | 新管理路径分类 |
| src/server/handlers/management/mod.rs、proxy.rs（新增） | 注册、handler、操作查询 |
| src/main.rs | 恢复、启停、任务取消和子进程回收 |
| src/page/views.js、views/accounts.js、views/proxy.js（新增） | 导航、绑定、代理管理 |
| Cargo.toml、Cargo.lock | process/fs、受限 YAML、AEAD 等必要依赖 |

源码依据包括：upload_file 的 PUT 当前直接用 self.http；events 提前捕获全局 fetch_channels；canonical_events_with_retry 的重发只返回 Response；oauth_token 当前为独立全局传输；Db::open 当前未开外键。以上为已核对切入点，不代表其他路径已完成安全验证。

构建修改 Dockerfile、docker-compose.yml、config.example.json、.github/workflows/docker-publish.yml 和 README；增加分架构版本摘要锁文件。至少支持 linux/amd64、linux/arm64，校验压缩包后解压，镜像内再验证版本和最小配置。

核心与应用同容器，工作目录持久化到现有本地 Docker 部署目录的专用子目录，不硬编码评审机绝对路径，不映射内部端口。数据库、密钥、运行目录按恢复单元备份。结构迁移后的回滚需按兼容性恢复，不能只更换镜像。

## 13. 错误与可观测性

日志含操作 ID、generation、内部账号 ID、revision、订阅/节点 ID 和阶段；不记录凭据、完整订阅 URL、原 YAML 或控制器 secret。邮箱遵循现有日志策略。

| code | 行为 |
|---|---|
| subscription_fetch_failed / subscription_invalid | 保留上次成功版本 |
| revision_conflict / binding_in_use | 不修改绑定，刷新状态后重试 |
| proxy_port_exhausted / proxy_port_in_use | 明确报错，不切换出口 |
| proxy_node_missing / proxy_subscription_disabled | 保留绑定并阻断 |
| proxy_manager_disabled / proxy_unavailable | 绑定 503，未绑定按原策略 |
| proxy_applying / proxy_route_blocked | 不发送账号上游请求 |
| proxy_connect_failed | 不改账号业务状态、不换账号 |
| proxy_apply_failed / proxy_recovery_required | 验证回滚或持续阻断 |

区分“配置生效”“最近手动测试”“真实业务失败”，不将一次延迟超时持久化成账号不可用。必要续期被代理阻断有独立提示，但不发探测请求解除提示。

## 14. 开发顺序

1. 核心原型：锁定版本/摘要，验证单节点、空组拒绝、listener、无自动探测、重载及连接关闭。
2. 存储与协调：外键、加密、日志、实例锁、端口预留、逐阶段恢复。
3. 订阅与节点：受限输入、稳定身份、墓碑、并发刷新。
4. 账号传输：两类客户端、完整链路、SSE 上下文、必要续期和排空。
5. API/UI：管理鉴权、版本冲突、操作状态及异常展示。
6. 双架构发布：真实核心集成、恢复演练、备份和部署文档。

各阶段按下一节验收，使用本地假上游和专用测试凭据，不对现有账号池进行探测式刷新或真实 token 轮换。

## 15. 验收与故障注入

### 15.1 核心契约

- 两种目标架构运行固定真实核心，不只 mock 控制器。
- 正常组恰好一个目标成员，默认选择正确；存在历史选择后重启仍正确。
- 空 Provider、无匹配、节点移除、订阅停用时拒绝；目标直连和其他节点计数均为零。
- 启动、加载、重载、节点失败、等待多个健康周期均无自动测速或其他非授权外联。
- listener/控制器均仅回环，无公共代理入口；错误 secret/外来核心不能被接管。
- 每种宣称支持的协议保留必要参数；不支持候选整体失败且保留旧版。

### 15.2 请求与 token

- 双账号不同节点时覆盖聊天、注册 POST、预签名 PUT、模型/用户/额度/试用和生成图片。
- 账号重选及 SSE 重试后，响应和图片均使用新账号租约。
- 代理失效不直连、不使用全局客户端、不换账号、不静默丢附件。
- 停用、冷却、低优先级待命账号普通调用计数为零；仅规定条件续期，未知到期不探测。
- 必要续期走绑定出口；错误不写成 invalid_grant、不改调度状态、不跨出口重放。
- 同账号并发 401/扫描只进行一次必要轮换，原子保存 token 对。
- 首次绑定、换绑、解绑、删除时无旧租约访问回收端口；已输出 SSE 不重放；OAuth 不因配置切换取消。

### 15.3 数据与恢复

- 跨订阅同名独立；订阅内重名、空列表、未知必要参数拒绝；密码变化保留 ID，服务器变化不自动换绑。
- 刷新失败保留旧版；有效删除节点阻断原绑定；节点按相同身份恢复后可验证恢复。
- 并发刷新/绑定/删除/测速不覆盖新版本、不复用未排空端口。
- 每阶段注入退出、磁盘满、SQLite 提交失败、核心重载及回滚失败，重启均恢复已提交事实或阻断。
- 实例冲突、端口占用/耗尽、核心崩溃和重启均无出口回退。
- 错误密钥、密文篡改、文件丢失、外键违规可检测，不以空配置覆盖。
- 模块关闭保留绑定阻断，显式解绑才恢复全局；备份恢复绑定与端口一致。

### 15.4 API 与发布

- 主 key/控制台可管理，托管推理 key/匿名不可管理。
- 验证 SSRF/重定向、解压/YAML 超限、日志/API 泄密边界。
- UI 区分 accepted/committed/失败/恢复，失效绑定不显示为未绑定，重发幂等。
- 双架构核心与锁文件一致，摘要正确，容器信号回收核心，无内部端口发布。
- 用假上游及出口计数验证零回退；无法观测真实出口时不能宣布失败关闭验收通过。

## 16. V1 范围外与实现取舍

不包含 URI/Base64/Surge 订阅、系统代理/TUN/DNS 劫持、自动换节点、负载均衡、链式代理、前端直接控制核心、多实例共享核心/数据库和在线密钥轮换。

完整代次重载会短暂暂停绑定账号新请求，必要时中断普通在途请求。这是 V1 为明确连接归属和恢复边界采用的取舍。未来增量更新须重新验证选择状态、旧连接与崩溃恢复，不能直接删除屏障。

最终约束：**绑定存在就只有已确认的唯一出口；不可用时保留绑定并拒绝。普通调用遵守现有账号调度，只有必要 token 续期例外。成功必须由数据库、核心及路由快照一致确认。**
