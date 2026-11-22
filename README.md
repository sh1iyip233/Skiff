# epicd — 第三方 Epic 游戏下载器

用**你自己的 Epic 账号**读取游戏库，按你指定的并发线程数下载、校验并安装游戏，并写出
`.egstore` 清单让官方启动器识别为“已安装”。

> **非官方工具**。调用 Epic 启动器使用但未公开文档的接口，可能随 Epic 侧变更而失效。
> 仅能下载你账号 entitlement 覆盖的内容（服务端强校验），不包含任何 DRM/反作弊绕过。
> 请只在自有机器的自有账号上使用。

---

## 1. 能力范围

| 能力 | 状态 |
|---|---|
| 设备码登录 / 浏览器授权码登录，token 自动刷新 | ✅ |
| DPAPI 加密存储 token（当前用户作用域） | ✅ |
| 读取 entitlement + library + catalog，合并出可安装游戏列表 | ✅ |
| 按 catalog `mainGameItem` 把 DLC 归到主游戏，默认不再作为独立游戏显示 | ✅ |
| 安装主游戏时可把已拥有的 DLC 合并进同一安装目录（`--no-dlc` 可关闭） | ✅ |
| 解析 Epic 二进制 manifest（`0x44BEC00C`，含列式 CDL/FML）与 JSON 兜底 | ✅ |
| 分块下载：用户指定并发、多 CDN host 轮转、连接复用 | ✅ |
| 逐块 SHA-1 + rolling hash 校验，zlib / AES-GCM 解包 | ✅ |
| 断点续传（续传账本）、`--repair` 只重下有问题的分块 | ✅ |
| 写 `.egstore`（`.item` / `.mancpn` / `.manifest`），可选注册到官方启动器 | ✅ |
| 图形界面 | ⏳ 见 §8 |

---

## 2. 构建

需要 Rust（`rustup`）。Windows 上优先用 **GNU 目标**，因为它不需要安装 Visual Studio：

```powershell
cargo build --release                                   # 有 MSVC 链接器时
cargo +stable-x86_64-pc-windows-gnu build --release      # 没有 Visual Studio 时
```

产物：`target/release/epicd.exe`（单文件，无运行时不依赖）。

> 若两个目标都不可用，先装 GNU 目标：`rustup target add x86_64-pc-windows-gnu`。
> `scripts\build.ps1` 会自动挑选可用目标。

设计上刻意**不依赖异步运行时与 TLS crate**：

- HTTPS 由系统 WinHTTP 提供（每个工作线程复用一条会话/连接），因此不需要 OpenSSL/ring，
  也不需要 MSVC 的 `dlltool` 流程；
- 并发用 std 线程池，和“并发线程”语义一一对应；
- 其余依赖（serde / sha1 / flate2 / brotli / aes-gcm）都是纯 Rust，可离线构建。

---

## 3. 快速开始

```powershell
# 1) 登录：给出激活链接与 8 位代码，浏览器里确认即可
epicd auth login

# 2) 看游戏库（首次会拉取并缓存）
epicd library list
epicd library list --search 赛博

# 3) 看某款游戏的构建与体量
epicd info Fortnite

# 4) 安装（默认 16 并发）
epicd install Fortnite -j 32 --limit-rate 80M

# 5) 校验 / 修复
epicd verify "D:\Games\Fortnite"
epicd verify "D:\Games\Fortnite" --repair
```

---

## 3.1 主游戏与 DLC

`library list` 默认只显示主游戏，DLC 列显示已拥有的追加内容数量；`--all` 才平铺显示它们：

```powershell
epicd library list                 # 主游戏 + DLC 数量
epicd library list --all           # 主游戏和 DLC，DLC 行标注父游戏
epicd library list --json          # 分组 JSON（games[].dlc[]）
```

关联依据是 Epic catalog 的 `mainGameItem`（DLC 的 catalog 条目会带上主条目），解析顺序为：
catalog id → release appId → 标题唯一匹配 → 同命名空间唯一主游戏。没有 `mainGameItem` 的
“额外内容”（例如某些 Bonus Content、Soundtrack、Wallpaper）默认保持独立；可用状态目录下的
`parents.json` 手工归组：

```json
{
  "81ed1e946da54050af4b3be7774ea038": "ginger"
}
```

键为 DLC 的 appName，值为主游戏 appName 或标题；改完执行一次 `epicd library list --refresh`。

安装主游戏时默认把同组已拥有的 DLC 一并解析并写入**同一个安装目录**（Epic 的 DLC 本身就是这样
覆盖到主游戏目录的）；只想要主游戏时加 `--no-dlc`：

```powershell
epicd install "Cyberpunk 2077"              # 主游戏 + Phantom Liberty / REDmod
epicd install "Cyberpunk 2077" --no-dlc     # 只装主游戏
epicd install "Cyberpunk 2077: Phantom Liberty"  # 自动解析到 Cyberpunk 2077
```

DLC 的构建解析失败（没有 Live 构建、加密 manifest、网络错误）只会告警并跳过，不会拖垮主游戏。
同一路径出现不同内容时保留主游戏版本并告警。

---

## 4. 并发线程（核心约定）

并发是**你定义的硬契约**，引擎不做任何“聪明”的自适应升降。

| 参数 | 别名 | 默认 | 范围 | 含义 |
|---|---|---|---|---|
| `--concurrency` | `-j` | 16 | 1–128 | 同时在途的分块下载线程数 |
| `--write-threads` |  | 4 | 1–16 | 落盘写入线程数 |
| `--verify-threads` |  | `min(CPU,8)` | 1–32 | 校验阶段并发文件数 |
| `--per-host` |  | 不限 | 1–128 | 单个 CDN host 的在途上限 |

规则：

1. **优先级**：命令行显式参数 > `config.toml` > 内置默认。
2. **越界即报错**（退出码 2），不会静默裁剪。
3. 稳态下在途请求数**恰好等于** `-j`（有测试断言，`peak_inflight` 会打印出来自证）。
4. 安装结束后会打印 `peak inflight: N (configured M)`，两者应一致。
5. 持久化：`epicd config set concurrency 32`。

调参建议（1 MiB 分块）：

```
吞吐 ≈ 并发数 × 1 MiB / RTT
```

- 100 Mbps 链路：`-j 8` 足够；
- 500 Mbps：`-j 16`；
- 千兆：`-j 32~64`，并配合 `--per-host 16` 避免单 host 被限速；
- 并发不是越大越好：过高会触发 CDN 429，看日志里的重试次数再调。

---

## 5. 命令一览

```
epicd auth login [--device-code | --code <authorizationCode>]
epicd auth logout | status
epicd library list [--search <text>] [--all] [--refresh] [--json]
epicd info <appName> [--json]
epicd install <appName> [-j N] [--write-threads N] [--per-host N] [--limit-rate 50M]
                        [--dir <path>] [--platform Windows] [--label Live]
                        [--no-dlc] [--verify] [--repair] [--register-launcher] [-y]
epicd verify <installDir> [--repair]
epicd uninstall <installDir> [--keep-files]
epicd config get | set <key> <value> | list
epicd debug manifest <file> [--json]
epicd debug cdn-probe <appName>
```

全局：`--json`、`--no-progress`。退出码：`0` 成功、`1` 运行错误、`2` 用法错误、`130` 中断。

`--dir` 指向**父目录**时会自动追加游戏的 `FolderName`；要精确指定安装目录请直接用目标路径
（目录名即安装目录）。

---

## 6. 目录与状态文件

```
%LOCALAPPDATA%\epicd\
├── config.toml        配置（并发、安装根目录、CDN 偏好等）
├── auth.json          账号 token（DPAPI 加密；损坏就重新 login）
├── games.json         游戏列表缓存（schema 2，含 DLC 关联字段）
├── titles.json        代号标题的本地覆盖缓存
└── parents.json       可选的 DLC→主游戏手工覆盖

<install_dir>\
├── .egstore\
│   ├── <GUID>.item       官方启动器格式的安装清单（主游戏和每个 DLC 各一套）
│   ├── <GUID>.mancpn     安装对比信息
│   ├── <GUID>.manifest   原始 manifest 备份
│   ├── epicd.files       本次安装会写入的文件清单（成功后自动删除）
│   └── epicd.resume      续传账本（成功后自动删除）
└── ...游戏文件...
```

合并安装时，DLC 的 `.item` 里 `MainGameAppName` 指向主游戏、`InstallLocation` 与主游戏相同，
`bIsApplication` / `bIsExecutable` 为 `false`；主游戏和 DLC 的文件都写在同一目录。

删除 `.egstore` 会让官方启动器不再识别该安装；`epicd uninstall` 会清理全部组件的
`.egstore` 以及 `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\<GUID>.item`。

**暂停 vs 取消**（GUI 的两个按钮）：

| 操作 | 行为 |
|---|---|
| 暂停 | 停止下载，保留已下载分块与续传账本；再点「继续安装」从断点继续 |
| 取消 | 停止下载，并删除本次安装写入的文件（只删 `epicd.files` 列出的路径，不会动目录里的其他文件；若该目录已有完整安装则保留原文件） |

`epicd serve` 的网页界面只有「暂停」；命令行按 `Ctrl-C` 等同暂停。

---

## 7. 工作原理

```
设备码/授权码 → TokenSet(DPAPI)
      ↓
entitlements + library + catalog(mainGameItem) → GameEntry(appName/namespace/catalogItemId/is_dlc/parent)
      ↓
library_view：主游戏 + 已拥有 DLC 分组（无父的 DLC 单独标记，不冒充游戏）
      ↓
安装主游戏：逐个解析 DLC 构建 → 合并 files[]/chunks[]（去重、主游戏优先）→ 单一并发计划
      ↓
launcher assets v2 → manifest URI + CDN base URLs（多 host 并集）
      ↓
binary manifest → chunks[] + files[]（列式解析，逐块记录 feature level）
      ↓
N 个下载线程：GET chunk → SHA-1/rolling hash 校验 → zlib/AES-GCM 解包
      ↓ 逐 part
预分配文件 + seek_write 定位写入（多 writer 线程）
      ↓
按文件 SHA-1 校验 → 每组件写 .egstore → （可选）注册启动器
```

关键实现点：

- **分块即工作单元**：Epic CDN 的 chunk 是内容寻址的独立对象，并发天然安全；
- **连接复用**：每个下载线程持有一条 WinHTTP 会话，握手只付一次；
- **多 host 轮转**：Akamai/Fastly 多个域名分摊，实测可显著提升聚合吞吐；
- **去重**：同一 chunk 被多个文件引用时只下一次（写入时扇出到各偏移）；
- **原子状态**：账本与配置用“临时文件 + rename”写入，断电不会写坏。

---

## 8. 图形界面（Tauri）

方案已确定为 CLI + Tauri GUI，`epic-core` 是两者的公共内核，GUI 只做薄适配层。
当前仓库先交付 CLI：**构建环境无法访问 crates.io 之外的包源，Tauri 的依赖树（tauri /
windows-sys）与前端 npm 依赖都无法在此环境拉取与验证**，与其交付一个编译不过的壳，
不如先把可验证的内核与 CLI 做完。

GUI 落地时只需：

1. `apps/epic-gui/src-tauri` 依赖 `epic-core`，把 `install_manifest`/`download` 跑在
   `tauri::async_runtime::spawn_blocking` 中；
2. 通过 `Progress` 回调 `app.emit("download://progress", …)`（约 5 Hz 节流）；
3. 并发控件直接读写 `Concurrency`，与 CLI 完全同一套校验规则。

---

## 9. 测试

```powershell
cargo +stable-x86_64-pc-windows-gnu test -p epic-core --offline
```

覆盖：

- **清单格式**：二进制往返（含 UTF-16 字段）、JSON 兜底、损坏检测、分块索引；
- **分块容器**：压缩/未压缩解析、魔数校验、篡改检测、加密块无密钥时报错；
- **CDN**：用真实抓到的 URL 断言路径模板（`ChunksV4/07/…`、`ChunksV4/54/…`）、host 轮转；
- **路径安全**：穿越、盘符、UNC、Windows 保留名、尾点空格；
- **并发契约**：`-j 6` 时峰值在途 = 6，`-j 1` 时串行；
- **DLC 关联**：catalog `mainGameItem` 解析、父游戏 id/app/标题/命名空间匹配、孤儿 DLC、手工覆盖；
- **端到端**（本地假 CDN）：完整安装、`.egstore` 产物、独立复验、续传不重下、修复只重下损坏块、
  多 host 轮转、损坏分块被拒、主游戏 + DLC 合并安装与联合卸载。

---

## 10. 已知限制

1. **feature level ≥ 22 的加密 manifest / 加密 chunk**：当前会明确报错并提示用官方启动器。
   支持它需要从资产接口拿 `encryptionKey` 并用 AES-GCM 解 manifest 头部字段，是下一步工作。
2. **注册到官方启动器**需要写 `%ProgramData%`，非管理员会失败（会给出提示，不影响已安装文件）。
3. **反作弊/EOS 游戏**：本工具只负责下载安装；部分游戏脱离官方启动器仍无法运行。
4. `Ctrl-C` 中断最多丢失最近一次账本落盘（每 64 块一次）的进度，重启后重下的量小于 64 MiB。
5. 只在 Windows 上测试；Linux/macOS 可编译（走 plain HTTP 后端），但 Epic 的 HTTPS 需要额外 TLS 方案。
6. **DLC 归组只以 Epic catalog 的 `mainGameItem` 为准**；目录里没有该字段的额外内容
   （部分 Bonus Content / Soundtrack / Wallpaper）默认仍是独立条目，需要 `parents.json` 手工归组。
7. **只拥有 DLC、没有主游戏**的条目不会安装：`library list --all` 可见，`install` 会明确报错。
8. 主游戏与 DLC 出现**同路径不同内容**时保留主游戏版本并告警；这属于少数游戏的覆盖差异，
   若遇到需要以 DLC 版本为准的情况，当前没有逐游戏覆盖规则。
9. 官方启动器对合并安装中 DLC `.item` 的识别是 best-effort（字段依据现有 `.item` 样本推断）；
   即使启动器把某个 DLC 显示为未安装，主游戏文件和运行不受影响。

---

## 11. 许可与致谢

本项目源码为 MIT。

Epic 的启动器接口没有公开文档，本项目实现所依据的**协议事实**（OAuth 授权流程、资产 v2 端点、
二进制 manifest 的列式布局、chunk 容器头、CDN 路径模板、`.egstore` 字段集）是在开发期参照社区
开源项目 [legendary](https://github.com/legendary-gl/legendary)（GPL-3.0）的实际行为与公开讨论
交叉核对的——只借鉴**事实与格式**，没有任何 Python 代码被移植或复制，Rust 侧实现均为独立编写。
若你分发本项目，请自行评估所在司法辖区的合规要求。

使用风险自担，详见文首声明。

---

## 12. 当前状态与实测结论（v0.2）

### 已用真实账号、真实 CDN 验证

| 项目 | 结果 |
|---|---|
| 授权码登录 | ✅ 绿道验证（设备码不可用，见下表） |
| 游戏库 | ✅ 165 条记录 → **132 款主游戏 + 33 个 DLC**（含 Cyberpunk 2077 的 `Ginger`） |
| DLC 关联 | ✅ Cyberpunk 2077 +2、Civ VI +9、Model Builder Complete Edition +10、Outer Worlds +2、Dying Light +2 等按 `mainGameItem` 归组 |
| 构建解析 | ✅ The Bridge `Sunbird`；Cyberpunk `Ginger` → manifest level 21 / 113,209 chunks / 110.93 GiB；Phantom Liberty → 30.10 GiB / 37 files |
| 实际安装 | ✅ The Bridge：344.40 MiB、159 文件、峰值并发 48/48、21.4s（≈16 MiB/s）、`verify` 159/159 clean |
| 合并安装 | ✅ 假 CDN 端到端：主游戏 + DLC 同目录、双 `.item`、共享 chunk 去重、联合 `verify`/`uninstall` |
| `.egstore` | ✅ `.item` 1741 B / `.mancpn` 161 B / `.manifest` 38,147 B；DLC 项 `MainGameAppName` 指向主游戏 |
| 并发契约 | ✅ `peak inflight` 严格等于设定值（1/16/48/128 均验证） |

### Epic 接口实测（避免重复踩坑）

| 现象 | 结论 |
|---|---|
| `/oauth/deviceAuthorization` → 401 | 设备码流程**不对 `launcherAppClient2` 开放**，必须用 `authorizationCode` |
| 所有业务接口 → 401 | `Authorization` 头必须带 **`bearer `** 前缀 |
| token 响应解析失败 | `scope` 是**字符串数组**，且刷新时长字段名是 `refresh_expires` |
| `/entitlement/.../entitlements` → 404 | 路由已变更；改为从 `launcher/api/public/assets` 推导 appName |
| CDN 三主机只有一个通 | 国内网络下 `akamaized.net` 常 12002 超时；安装前探活剔除 |
| `catalog bulk/items` 无标题 | 必须带 `country`/`locale`，否则多数条目只返回 id |
| `catalog bulk/items` 多 id 返回 `{}` | id 必须写成重复参数 `id=a&id=b`；逗号拼接 `id=a,b` 会被静默当成空结果 |
| `mainGameItem` | DLC 条目带父游戏详情；`mainGameItemList` 是合集内容，不是父指针 |

### 环境适配（本机踩到的坑）

- **状态目录**自动降级：`%LOCALAPPDATA%` → `%APPDATA%` → `%TEMP%` → `./.epicd`（本机 `AppData\Local` 对非管理员不可写）
- **安装根目录**不可写时提示并兜底，绝不静默改用相对路径
- **授权码登录先探测可写性**，避免白烧一次性 code
- `EPICD_HOME` 可固定状态位置；CLI 与 GUI 需用同一个值才共享登录态

### 构建

```powershell
.\scripts\build.ps1        # CLI（自动加载 dlltool + rust-lld）
.\scripts\build-gui.ps1    # GUI（Slint）
```

三个必需条件（缺一不可，脚本已固化）：MinGW `dlltool` 在 PATH、`RUSTUP_TOOLCHAIN=…-gnu`、`RUSTFLAGS=-C linker=rust-lld`。

### 三个界面

| 形态 | 路径 | 特点 |
|---|---|---|
| **Slint 原生窗口（主推）** | `apps/epic-slint` → `epicd-ui.exe`（12.6 MiB） | 自绘、无 WebView2、**免管理员**、启动快 |
| 浏览器 GUI | `epicd serve --open` | 零依赖，任何机器可用，功能与上面一致 |
| Tauri（已弃用） | `apps/epic-gui` | 已从 workspace 排除；在本机需管理员权限，仅留作参考 |

### 已知未完成

1. **标题/FolderName 补全**：`Ginger` 显示为代号而非 `Cyberpunk 2077`；34 条显示「（无商店信息）」。需要按 offer/productSlug 兜底查询。
2. **增量更新**：每次 `install` 全量重下；`.mancpn` 已写入但未用于差异下载。
3. manifest feature level **≥22**（ChunksV5 加密）仍不支持，会明确报错。
