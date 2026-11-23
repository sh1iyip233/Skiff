# Skiff

一个用 Rust 写的第三方 Epic 游戏下载器（Windows）。

主要解决官方启动器下载慢、并发调不了的问题：登录你自己的 Epic 账号，读取游戏库，按自己指定的线程数多线程下载、校验并安装游戏，同时生成 `.egstore` 文件，让官方启动器也能直接识别为已安装。

> 声明：非官方工具，调用的是 Epic 启动器未公开的接口，官方改接口随时可能失效。只能下载你自己账号已拥有的游戏（服务端会校验权限），不包含任何 DRM 或反作弊绕过。建议只在自己的电脑和账号上用，风险自担。

## 都有什么功能

- **三种使用方式**：
  - 桌面客户端（Slint 自绘界面，`skiff-ui.exe`，不依赖 WebView2，不用管理员权限）
  - 本地网页界面（`skiff serve --open`，浏览器里直接用）
  - 命令行（`skiff.exe`）
- **账号登录**：通过浏览器授权码登录，token 用 Windows DPAPI 按当前用户加密保存在本地，过期自动刷新。
- **游戏库与封面**：拉取游戏库，按 Epic 目录的 `mainGameItem` 自动把 DLC 归到对应主游戏下面；桌面端以封面网格展示主游戏，附加内容单独一栏，封面按需下载并缓存到本地。安装主游戏时默认把已拥有的 DLC 一起装进同一目录（加 `--no-dlc` 可关闭）。
- **独立的下载页面**：桌面端的下载进度、暂停/取消和续传都放在「下载」页，游戏库页只负责挑选和安装，不再挤在同一个界面里。下载、校验、修复三个阶段都显示实时进度。
- **给已装好的游戏补 DLC**：`skiff dlc add` 只解析并下载缺的那个 DLC，本体不重新解析、不重新下载、不重新校验；`.egstore` 里本体和已装 DLC 的清单原样保留，新 DLC 复用本体的 `InstallationGuid`。支持 `--dry-run` 先看体积再决定。
- **多线程分块下载**：自己定下载并发数，支持多 CDN 节点轮转、连接复用和自动剔除连不上的节点（比如国内经常超时的 Akamai 节点）。
- **校验与续传**：支持断点续传、逐块 SHA-1 / rolling hash 校验、zlib 解包，文件坏了可以用 `--repair` 只重下损坏的分块。
- **兼容官方启动器**：自动写出 `.egstore`（`.item` / `.mancpn` / `.manifest`），也可以选择注册到官方启动器（注册后需要把启动器从托盘完全退出再打开才生效，见下文）。

## 快速上手

### 图形界面

编译好之后直接运行 `skiff-ui.exe` 即可，或者用命令行起一个本地网页版：

```powershell
skiff serve --open
```

### 命令行

```powershell
# 1. 登录（按提示在浏览器打开链接，把拿到的 authorizationCode 填进来）
skiff auth login

# 2. 查看游戏库（首次会拉取并缓存）
skiff library list
skiff library list --search 赛博

# 3. 查看游戏体积和构建信息
skiff info Fortnite

# 4. 下载安装（-j 指定下载线程数，--limit-rate 限速）
skiff install Fortnite -j 32 --limit-rate 80M

# 5. 校验或修复已安装的游戏
skiff verify "D:\Games\Fortnite"
skiff verify "D:\Games\Fortnite" --repair
```

> 注：Epic 的设备码登录（`--device-code`）目前不对 `launcherAppClient2` 开放，会直接返回 401，所以登录请走默认的浏览器授权码流程。

## DLC 处理

`skiff library list` 默认只列出主游戏，后面标出已拥有的 DLC 数量；想全部摊开看就加 `--all`：

```powershell
skiff library list            # 主游戏 + DLC 数量
skiff library list --all      # 全部列出，DLC 会标明所属主游戏
skiff library list --json     # 按主游戏分组输出 JSON
```

装主游戏时默认会把同组已拥有的 DLC 合并装到同一个目录里（官方启动器也是这么放的）。如果只装本体，加上 `--no-dlc`：

```powershell
skiff install "Cyberpunk 2077"                  # 主游戏 + 已拥有的 DLC
skiff install "Cyberpunk 2077" --no-dlc         # 只装主游戏
skiff install "Cyberpunk 2077: Phantom Liberty" # 会自动定位到主游戏
```

DLC 归组依赖 Epic 目录里的 `mainGameItem` 字段。有些额外内容（比如原声带、壁纸、Bonus Content）目录里没写这个字段，默认会单独列出来。如果想手动归到某个主游戏下面，可以在状态目录里新建一个 `parents.json`：

```json
{
  "81ed1e946da54050af4b3be7774ea038": "ginger"
}
```

左边填 DLC 的 `appName`，右边填主游戏的 `appName` 或标题，保存后跑一次 `skiff library list --refresh` 生效。

如果某个 DLC 拉取失败（比如没发布 Live 版本、manifest 加密或者网络报错），会打个警告跳过，不影响主游戏安装。主游戏和 DLC 如果有同路径但内容不同的文件，目前默认保留主游戏的版本。

### 给已经装好的游戏补 DLC

本体已经装完，事后才想补 DLC，不用重装本体：

```powershell
skiff dlc list "Cyberpunk 2077"                     # 看哪些 DLC 装了、哪些没装
skiff dlc add  "Cyberpunk 2077" --dry-run           # 只预览：解析版本和体积，不写盘
skiff dlc add  "Cyberpunk 2077"                     # 补装所有已拥有但没装的 DLC
skiff dlc add  "Cyberpunk 2077" "Phantom Liberty"   # 只补指定的那个，标题可以只写一部分
```

安装目录默认按 `install_root` 自动找（认 `.egstore` 里的 appName），找不到或者装在了别处就用 `--dir` 指过去，指安装目录本身或它的上级目录都行。

具体做法是从 `.egstore` 读回本体的清单参与合并——只为了复用分块和同路径去重，本体的文件既不下载也不重新校验，本体和已装 DLC 的 `.item` / `.mancpn` / `.manifest` 都原样保留，新 DLC 的 `.item` 复用本体的 `InstallationGuid`，所以在官方启动器看来只是同一个安装多了一个附加内容，而不是重新装了一遍。

有一个必要的例外：新 DLC 的文件如果用到和本体相同的分块，这个分块会重新下一次（新文件里还没有这段字节），一次下载写进所有用到它的文件。

退出码和 `install` 一致；`--register-launcher` 只把新 DLC 的 `.item` 拷给官方启动器，本体的注册不动。

## 并发与限速

下载线程数完全按你设定的来，程序不会偷偷帮你调高或调低。填超范围会直接报错（退出码 2），安装完后会打印实际峰值并发 `peak inflight` 供核对。

| 参数 | 缩写 | 默认值 | 范围 | 说明 |
|---|---|---|---|---|
| `--concurrency` | `-j` | 16 | 1–128 | 同时下载的分块线程数 |
| `--write-threads` | | 4 | 1–16 | 写盘线程数 |
| `--verify-threads` | | `min(CPU, 8)` | 1–32 | 校验并发数 |
| `--per-host` | | 不限 | 1–128 | 单个 CDN 域名的并发上限 |

优先级为：命令行参数 > `config.toml` > 默认值。想改默认并发可以直接写进配置：

```powershell
skiff config set concurrency 32
```

一般怎么调（Epic 分块大约 1 MiB 一块）：
- 100M 宽带：`-j 8` 基本就够了
- 500M 宽带：`-j 16`
- 千兆宽带：`-j 32` 到 `64`，最好配上 `--per-host 16` 防止单个 CDN 节点限速
- 不是越大越好，开太高容易撞 CDN 的 429 限流，看日志里重试多不多来微调就行

## 全部命令

```text
skiff auth login [--device-code | --code <authorizationCode>]
skiff auth logout | status
skiff library list [--search <text>] [--all] [--refresh] [--json]
skiff info <appName> [--json]
skiff install <appName> [-j N] [--write-threads N] [--per-host N] [--limit-rate 50M]
                        [--dir <path>] [--platform Windows] [--label Live]
                        [--no-dlc] [--verify] [--repair] [--register-launcher] [-y]
skiff dlc list <appName> [--dir <path>] [--json]
skiff dlc add <appName> [<dlcName>...] [--dry-run] [-j N] [--write-threads N] [--per-host N]
                        [--limit-rate 50M] [--dir <path>] [--register-launcher] [-y]
skiff verify <installDir> [--repair]
skiff uninstall <installDir> [--keep-files]
skiff register <installDir>
skiff config get | set <key> <value> | list
skiff serve [--port <n>] [--open]
skiff debug manifest <file> [--json]
skiff debug cdn-probe <appName>
```

全局参数：`--json`、`--no-progress`。退出码：`0` 正常、`1` 运行出错、`2` 参数错误、`130` 手动中断。

`--dir` 如果传的是上级目录（比如 `D:\Games`），会自动拼上游戏的文件夹名；想装到指定目录就直接写完整路径。

## 文件位置与目录结构

配置和缓存默认放在 `%LOCALAPPDATA%\Skiff`（如果不可写会自动往 `%APPDATA%`、`%TEMP%`、`./.Skiff` 降级，也可以通过环境变量 `SKIFF_HOME` 指定固定目录，CLI 和 GUI 设成同一个值就会共享登录态）。从旧版 `epicd` 升级时会自动迁移目录。

```text
%LOCALAPPDATA%\Skiff\
├── config.toml        配置文件（并发、默认安装目录等）
├── auth.json          登录凭证（DPAPI 加密，损坏删掉重新登录即可）
├── games.json         游戏库缓存
├── titles.json        代号标题的本地缓存
├── parents.json       手动设置的 DLC 归组（可选）
└── covers\            游戏封面缩略图缓存（可整目录删除，缺了会重新下载）

<游戏安装目录>\
├── .egstore\
│   ├── <GUID>.item       官方启动器识别用的清单（主游戏和各 DLC 各一份）
│   ├── <GUID>.mancpn     安装对照信息
│   ├── <GUID>.manifest   原始 manifest 备份
│   ├── skiff.files       本次安装写入的文件列表（装完自动删）
│   └── skiff.resume      断点续传记录（装完自动删）
└── ...游戏文件...
```

删掉 `.egstore` 后官方启动器就认不出这个游戏了；用 `skiff uninstall` 卸载时会一并清理 `.egstore`、`%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\` 里对应的 `.item`，以及 EOS Install Helper 的安装记录（见下）。

**注册到官方启动器到底写了什么**：从启动器 20.x（2026 年 11 月）开始，安装状态由 EOS Install Helper（EOSH）管理，只往 `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\` 放 `.item` **已经不够了**——启动器每次启动都会执行一次对账：

```text
EOSH reconciliation: removing installation not reported by EOSH '.../<GUID>.item'
```

清单会被当成过期数据删掉，库里就一直是「安装 / 继续下载」。所以 skiff 现在同时写两处：

```text
%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\<GUID>.item
%ProgramData%\Epic\EpicOnlineServicesShared\InstallHelper\InstalledItems\<GUID>.egi
%ProgramData%\Epic\EpicOnlineServicesShared\InstallHelper\InstalledItems\Revision.json
```

`.egi` 是 EOSH 的安装记录（`state: "Installed"`），`Revision.json` 是整份记录的版本号，两处要保持同一个 revision，启动器才会认。具体格式是照启动器自己写出来的记录反推的，属于未公开格式，**Epic 改版后可能失效**；真失效了再更新适配。

**注册后为什么要重启启动器**：启动器只在**启动时**读这两处（日志里的 `LoadItemFiles` 和 EOSH 对账），运行中不会重新读取。所以如果安装时启动器正开着，游戏不会立刻出现：

1. 右键任务栏通知区域里的 Epic 图标 → **退出**（只点窗口右上角的 × 只是最小化到托盘，进程还在，等于没重启）；
2. 重新打开 Epic 启动器，游戏就会显示为「已安装」，可以直接开始游戏。

如果清单被启动器清掉了（比如启动器大版本更新之后），不用重装，直接补注册即可：

```powershell
skiff register "D:\Games\Cyberpunk2077"
```


**关于暂停和取消**：
- **暂停**（命令行按 `Ctrl-C` 也是这个效果）：停掉下载，保留已经下好的文件和续传记录，下次接着下。
- **取消**（桌面版提供）：停掉下载，并删掉本次安装新写入的文件（只删 `skiff.files` 里记的路径，不动目录里原本就有的文件）。

## 编译构建

需要 Rust（`rustup`）。Windows 上直接跑仓库里的构建脚本最省事（默认走 GNU 工具链 + `rust-lld`，不需要装 Visual Studio）：

```powershell
.\scripts\build.ps1        # 编译 CLI -> target/release/skiff.exe
.\scripts\build-gui.ps1    # 编译 Slint 桌面端 -> target/release/skiff-ui.exe
```

也可以直接用 Cargo：

```powershell
cargo build --release                                   # 本机有 MSVC 时
cargo +stable-x86_64-pc-windows-gnu build --release     # 用 GNU 工具链
```

如果缺 GNU 目标，先装一下：`rustup target add x86_64-pc-windows-gnu`。脚本依赖 MinGW 的 `dlltool`、`x86_64-pc-windows-gnu` 工具链和 `rust-lld`。

为了打包干净、不折腾 OpenSSL / MSVC 链接，代码里没有引入 tokio 或第三方 TLS 库：HTTPS 直接调系统自带的 WinHTTP（每个下载线程复用一条连接），并发直接用标准库线程池，其余加密和压缩依赖都是纯 Rust。

## 跑测试

```powershell
cargo +stable-x86_64-pc-windows-gnu test -p skiff-core --offline
```

包含 manifest 解析、分块校验与解压、CDN 路径拼接与轮转、路径穿越检查、并发数验证、DLC 归组，以及基于本地假 CDN 的端到端安装/续传/修复/卸载测试。

## 目前的限制和已知问题

1. **不支持加密 manifest / 加密 chunk（feature level ≥ 22）**：遇到会直接报错提示改用官方启动器。
2. **还不支持增量更新**：目前每次 `install` 都是全量校验/下载，还没利用 `.mancpn` 做差异更新。
3. **部分游戏名显示为内部代号**：比如 `Cyberpunk 2077` 在目录里可能只拿到代号 `Ginger`，个别条目会显示无商店信息，后续需要补查询逻辑。
4. **注册到官方启动器需要管理员权限**：因为要写 `%ProgramData%`。没权限时注册会失败，但不会让整次安装失败，游戏文件照常保留（只是官方启动器里看不到）。注册用的 EOSH 记录格式是未公开的，启动器大版本更新后可能需要用 `skiff register` 重新注册。
5. **反作弊 / EOS 依赖**：本工具只管下载安装，有些游戏不开官方启动器还是进不去。
6. **只测了 Windows**：Linux/macOS 能编译，但没接 HTTPS 后端，目前没法直接下 Epic 的东西。
7. **只买了 DLC 没买本体**：`library list --all` 能看到，但无法单独安装（补装到已装好的本体上是支持的，见 `skiff dlc add`）。
8. **中途强退的进度损耗**：续传记录每 64 个分块刷一次盘，按 `Ctrl-C` 最多重下不到 64 MiB。
9. **官方启动器对合并 DLC 的识别**：`.item` 字段是照着官方样本拼的，万一启动器把某个 DLC 标成未安装，一般不影响主游戏正常游玩。

<details>
<summary>开发备忘：实测踩过的 Epic 接口坑</summary>

- `/oauth/deviceAuthorization` 对 `launcherAppClient2` 返回 401，必须用 `authorizationCode`。
- 业务接口的 `Authorization` 请求头必须带 `bearer ` 前缀。
- token 响应里的 `scope` 是字符串数组，刷新过期时间字段叫 `refresh_expires`。
- 旧的 `/entitlement/.../entitlements` 路由会 404，现在改从 `launcher/api/public/assets` 拿 `appName`。
- 国内网络下 `akamaized.net` 经常 12002 超时，所以安装前会先探活把不通的节点踢掉。
- `catalog bulk/items` 必须带 `country` 和 `locale`，不然大部分条目不给标题；而且多个 id 必须写成重复参数 `id=a&id=b`，用逗号拼成 `id=a,b` 会直接返回空 `{}`。
- `mainGameItem` 才是 DLC 指向主游戏的字段，`mainGameItemList` 是合集里包含的内容。

</details>

## 致谢与协议

代码采用 MIT 协议开源。

Epic 启动器的接口没有官方文档，开发时对照了开源项目 [legendary](https://github.com/legendary-gl/legendary)（GPL-3.0）的行为和社区资料来确认 OAuth 流程、manifest 结构、chunk 格式、CDN 路径规则和 `.egstore` 字段。本项目只参考了协议格式，Rust 代码全部重写，没有复制或移植任何 Python 代码。
